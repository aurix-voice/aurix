//! Content safety pipeline: transcripts of `safety_voice` channels and chat messages go
//! through the lexicon and the toxicity classifier; what crosses the thresholds becomes a
//! `moderation_events` row (`safety.voice` / `safety.text`), a tenant-scoped `safety.incident`
//! event, optionally an evidence clip stored through the recording subsystem, and — when the
//! operator opted in — an automatic server mute or kick through the same primitives human
//! moderators use.
//!
//! Risk is not kept in memory: it is recomputed from the user's recent incidents in the
//! database, so it is identical on every node and survives restarts.

use crate::chat::{FilterRequest, TextFilter, Verdict, SYSTEM_USER};
use crate::event_bus::{EventBus, ServerEvent};
use crate::moderation_actions::{kick_from_channel, set_server_mute, ModerationTarget};
use crate::plane::ControlPlane;
use aurix_common::config::{LexiconAction, SafetyConfig};
use aurix_common::error::{AurixError, Result};
use aurix_common::protocol::ChatMessage;
use aurix_common::safety::{
    decayed_risk, risk_level, trigger_fires, Classification, ClassifyRequest, HttpTextClassifier,
    Lexicon, LexiconVerdict, RiskLevel, SafetySource, TextClassifier,
};
use aurix_common::sink::{AudioEvidence, EvidenceStore};
use aurix_common::types::*;
use aurix_db::models::ModerationEventRow;
use aurix_db::DbPool;
use aurix_media::SfuNode;
use chrono::{DateTime, Duration, Utc};
use parking_lot::{Mutex, RwLock};
use serde::Serialize;
use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Weak};
use tokio::sync::Semaphore;
use tracing::{info, warn};
use uuid::Uuid;

pub const EVENT_TYPE_VOICE: &str = "safety.voice";
pub const EVENT_TYPE_TEXT: &str = "safety.text";

/// Incidents older than this many half-lives contribute less than 0.1% and are ignored.
const RISK_WINDOW_HALF_LIVES: i64 = 10;
const RISK_MAX_INCIDENTS: i64 = 500;
/// Pre-roll audio kept per speaker is dropped after this much silence.
const PRE_ROLL_IDLE: std::time::Duration = std::time::Duration::from_secs(120);
const AUTO_KICK_REASON: &str = "Removed by the content safety policy";

/// A transcript segment of a `safety_voice` channel, with the audio it came from.
pub struct VoiceSegment {
    pub app_id: AppId,
    pub channel_id: ChannelId,
    pub user_id: UserId,
    pub text: String,
    pub language: Option<String>,
    pub started_at: DateTime<Utc>,
    pub audio_ms: u64,
    /// Mono PCM at `sample_rate`.
    pub pcm: Arc<Vec<i16>>,
    pub sample_rate: u32,
}

/// Executes automatic actions. Implemented on top of the control plane + SFU by
/// [`ControlPlaneEnforcer`]; tests plug in their own.
#[async_trait::async_trait]
pub trait SafetyEnforcer: Send + Sync {
    /// Server-mute the user in `channel`, or in every channel of the app they are in.
    async fn mute(
        &self,
        app_id: AppId,
        user_id: UserId,
        channel: Option<ChannelId>,
    ) -> Result<Vec<ChannelId>>;
    async fn kick(
        &self,
        app_id: AppId,
        user_id: UserId,
        channel: Option<ChannelId>,
    ) -> Result<Vec<ChannelId>>;
}

/// Current risk of a user, recomputed from stored incidents.
#[derive(Debug, Clone, Serialize)]
pub struct RiskSnapshot {
    pub user_id: UserId,
    pub risk_score: f32,
    pub risk_level: RiskLevel,
    /// Incidents inside the scoring window.
    pub incidents: usize,
    pub half_life_secs: u64,
    pub computed_at: DateTime<Utc>,
}

/// One earlier message attached to a text incident.
#[derive(Debug, Clone, Serialize)]
pub struct ContextMessage {
    pub message_id: Uuid,
    pub from_user_id: UserId,
    pub display_name: String,
    pub text: String,
    pub sent_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum ContextKey {
    Channel(AppId, ChannelId),
    Direct(AppId, UserId, UserId),
}

impl ContextKey {
    fn direct(app_id: AppId, a: UserId, b: UserId) -> Self {
        if a.0 <= b.0 {
            Self::Direct(app_id, a, b)
        } else {
            Self::Direct(app_id, b, a)
        }
    }
}

struct PreRoll {
    segments: VecDeque<Arc<Vec<i16>>>,
    sample_rate: u32,
    last_seen: std::time::Instant,
}

/// Everything known about a flagged piece of content before it is persisted.
struct Incident {
    app_id: AppId,
    channel_id: Option<ChannelId>,
    session_id: Option<SessionId>,
    user_id: UserId,
    source: SafetySource,
    text: String,
    masked_text: Option<String>,
    language: Option<String>,
    score: f32,
    categories: Vec<String>,
    lexicon: Option<LexiconVerdict>,
    classification: Option<Classification>,
    blocked: bool,
    context: Vec<ContextMessage>,
    audio: Option<AudioEvidence>,
    audio_ms: u64,
}

pub struct SafetyService {
    cfg: SafetyConfig,
    pool: DbPool,
    events: Arc<EventBus>,
    classifier: Option<Arc<dyn TextClassifier>>,
    lexicon: Option<Lexicon>,
    mask_char: char,
    permits: Arc<Semaphore>,
    evidence: RwLock<Option<Arc<dyn EvidenceStore>>>,
    enforcer: RwLock<Option<Arc<dyn SafetyEnforcer>>>,
    context: Mutex<HashMap<ContextKey, VecDeque<ContextMessage>>>,
    pre_roll: Mutex<HashMap<(ChannelId, UserId), PreRoll>>,
}

impl SafetyService {
    pub fn new(cfg: SafetyConfig, pool: DbPool, events: Arc<EventBus>) -> Result<Self> {
        let classifier: Option<Arc<dyn TextClassifier>> = if cfg.enabled {
            HttpTextClassifier::from_config(&cfg.classifier)
                .map(|c| Arc::new(c) as Arc<dyn TextClassifier>)
        } else {
            None
        };
        Self::with_classifier(cfg, pool, events, classifier)
    }

    /// Builds the service around an already constructed classifier (tests use a mock).
    pub fn with_classifier(
        cfg: SafetyConfig,
        pool: DbPool,
        events: Arc<EventBus>,
        classifier: Option<Arc<dyn TextClassifier>>,
    ) -> Result<Self> {
        let lexicon = if cfg.enabled && cfg.text.enabled || cfg.enabled && cfg.voice.enabled {
            let lex = Lexicon::load(cfg.text.lexicon_path.as_deref(), &cfg.text.lexicon)?;
            (!lex.is_empty()).then_some(lex)
        } else {
            None
        };
        let mask_char = cfg.text.mask_char.chars().next().unwrap_or('*');
        let permits = Arc::new(Semaphore::new(
            cfg.classifier.max_concurrent_requests.max(1) as usize,
        ));
        if cfg.enabled {
            info!(
                "Content safety enabled (classifier: {}, lexicon: {} rules, voice: {}, text: {})",
                classifier
                    .as_ref()
                    .map(|c| c.classifier_name().to_string())
                    .unwrap_or_else(|| "none".into()),
                lexicon.as_ref().map(Lexicon::len).unwrap_or(0),
                cfg.voice.enabled,
                cfg.text.enabled
            );
        }
        Ok(Self {
            cfg,
            pool,
            events,
            classifier,
            lexicon,
            mask_char,
            permits,
            evidence: RwLock::new(None),
            enforcer: RwLock::new(None),
            context: Mutex::new(HashMap::new()),
            pre_roll: Mutex::new(HashMap::new()),
        })
    }

    pub fn config(&self) -> &SafetyConfig {
        &self.cfg
    }

    pub fn enabled(&self) -> bool {
        self.cfg.enabled && (self.classifier.is_some() || self.lexicon.is_some())
    }

    /// Transcripts of `safety_voice` channels are analyzed.
    pub fn voice_enabled(&self) -> bool {
        self.enabled() && self.cfg.voice.enabled
    }

    /// Chat and TTS text goes through the lexicon/classifier.
    pub fn text_enabled(&self) -> bool {
        self.enabled()
            && self.cfg.text.enabled
            && (self.lexicon.is_some() || (self.cfg.text.classify && self.classifier.is_some()))
    }

    pub fn set_evidence_store(&self, store: Arc<dyn EvidenceStore>) {
        *self.evidence.write() = Some(store);
    }

    pub fn set_enforcer(&self, enforcer: Arc<dyn SafetyEnforcer>) {
        *self.enforcer.write() = Some(enforcer);
    }

    /// The chat filter stage, to be placed before `chat.filter_webhook`.
    pub fn text_filter(self: &Arc<Self>) -> Option<Arc<dyn TextFilter>> {
        self.text_enabled()
            .then(|| Arc::new(SafetyTextFilter(self.clone())) as Arc<dyn TextFilter>)
    }

    /// Follows delivered chat messages (from every node) to keep the context attached to text
    /// incidents, and drops per-channel state when a channel goes quiet. No-op when disabled.
    pub fn start(self: &Arc<Self>) {
        if !self.enabled() {
            return;
        }
        let track_context = self.text_enabled() && self.cfg.text.context_messages > 0;
        let svc = Arc::downgrade(self);
        let mut rx = self.events.subscribe();
        tokio::spawn(async move {
            loop {
                match rx.recv().await {
                    Ok(ServerEvent::ChatMessage {
                        app_id, message, ..
                    }) if track_context => {
                        let Some(svc) = svc.upgrade() else { return };
                        svc.remember_message(app_id, &message);
                    }
                    Ok(ServerEvent::ChannelDeactivated {
                        app_id, channel_id, ..
                    })
                    | Ok(ServerEvent::ChannelDestroyed {
                        app_id, channel_id, ..
                    }) => {
                        let Some(svc) = svc.upgrade() else { return };
                        svc.forget_channel(app_id, channel_id);
                    }
                    Ok(_) => {}
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                        warn!("safety context stream lagged by {n} events");
                    }
                    Err(_) => return,
                }
            }
        });
    }

    fn context_key(
        app_id: AppId,
        channel_id: Option<ChannelId>,
        from: UserId,
        to: Option<UserId>,
    ) -> Option<ContextKey> {
        match (channel_id, to) {
            (Some(c), _) => Some(ContextKey::Channel(app_id, c)),
            (None, Some(t)) => Some(ContextKey::direct(app_id, from, t)),
            (None, None) => None,
        }
    }

    fn remember_message(&self, app_id: AppId, message: &ChatMessage) {
        let Some(key) = Self::context_key(
            app_id,
            message.channel_id,
            message.from_user_id,
            message.to_user_id,
        ) else {
            return;
        };
        let keep = self.cfg.text.context_messages as usize;
        let mut ctx = self.context.lock();
        let ring = ctx.entry(key).or_default();
        ring.push_back(ContextMessage {
            message_id: message.id,
            from_user_id: message.from_user_id,
            display_name: message.display_name.clone(),
            text: message.text.clone(),
            sent_at: message.sent_at,
        });
        while ring.len() > keep {
            ring.pop_front();
        }
        // Idle conversations: cap the map so a churn of channels cannot grow it unbounded.
        if ctx.len() > 10_000 {
            let cutoff = Utc::now() - Duration::minutes(30);
            ctx.retain(|_, r| r.back().is_some_and(|m| m.sent_at > cutoff));
        }
    }

    fn context_for(
        &self,
        app_id: AppId,
        channel_id: Option<ChannelId>,
        from: UserId,
        to: Option<UserId>,
    ) -> Vec<ContextMessage> {
        Self::context_key(app_id, channel_id, from, to)
            .and_then(|key| {
                self.context
                    .lock()
                    .get(&key)
                    .map(|r| r.iter().cloned().collect())
            })
            .unwrap_or_default()
    }

    /// Forget a channel's context and pre-roll once it is gone.
    pub fn forget_channel(&self, app_id: AppId, channel_id: ChannelId) {
        self.context
            .lock()
            .remove(&ContextKey::Channel(app_id, channel_id));
        self.pre_roll.lock().retain(|(c, _), _| *c != channel_id);
    }

    // ── Analysis ──

    async fn classify(
        &self,
        text: &str,
        language: Option<&str>,
        context: &[String],
        source: SafetySource,
    ) -> Result<Option<Classification>> {
        let Some(classifier) = self.classifier.as_ref() else {
            return Ok(None);
        };
        let _permit = self
            .permits
            .acquire()
            .await
            .map_err(|_| AurixError::Safety("classifier shut down".into()))?;
        classifier
            .classify(ClassifyRequest {
                text,
                language,
                context,
                source,
            })
            .await
            .map(Some)
    }

    fn lexicon_verdict(&self, text: &str) -> Option<LexiconVerdict> {
        self.lexicon
            .as_ref()
            .map(|lex| lex.apply(text, self.mask_char))
            .filter(|v| !v.is_clean())
    }

    /// Combined severity and categories of the lexicon hits and the classifier verdict.
    fn combine(
        &self,
        lexicon: Option<&LexiconVerdict>,
        classification: Option<&Classification>,
    ) -> (f32, Vec<String>) {
        let mut score = 0.0f32;
        let mut categories = Vec::new();
        if let Some(v) = lexicon {
            score = score.max(v.severity.clamp(0.0, 1.0));
            categories.extend(v.categories());
        }
        if let Some(c) = classification {
            let s = c.effective_score(&self.cfg.categories);
            score = score.max(s);
            let mut hits = c.categories_at_or_above(self.cfg.incident_threshold);
            if !self.cfg.categories.is_empty() {
                hits.retain(|h| self.cfg.categories.contains(h));
            }
            if hits.is_empty() && s >= self.cfg.incident_threshold {
                hits.extend(c.labels.iter().cloned());
            }
            categories.extend(hits);
        }
        categories.sort();
        categories.dedup();
        (score, categories)
    }

    /// Whether the combination is an incident: over the threshold, flagged by the provider,
    /// or an explicit `block`/`flag` lexicon rule with a non-zero severity.
    fn is_incident(
        &self,
        score: f32,
        lexicon: Option<&LexiconVerdict>,
        classification: Option<&Classification>,
    ) -> bool {
        if score >= self.cfg.incident_threshold {
            return true;
        }
        if classification.is_some_and(|c| c.flagged) {
            return true;
        }
        lexicon.is_some_and(|v| {
            v.severity > 0.0
                && matches!(
                    v.action,
                    Some(LexiconAction::Block) | Some(LexiconAction::Flag)
                )
        })
    }

    // ── Voice ──

    /// Analyze one transcript segment in the background (never blocks the audio pipeline).
    pub fn handle_voice(self: &Arc<Self>, seg: VoiceSegment) {
        if !self.voice_enabled() {
            return;
        }
        let svc = self.clone();
        tokio::spawn(async move {
            if let Err(e) = svc.process_voice(seg).await {
                warn!("safety: voice segment analysis failed: {e}");
            }
        });
    }

    async fn process_voice(&self, seg: VoiceSegment) -> Result<()> {
        let text = seg.text.trim();
        if text.is_empty() {
            self.push_pre_roll(&seg);
            return Ok(());
        }
        let lexicon = self.lexicon_verdict(text);
        let classification = match self
            .classify(text, seg.language.as_deref(), &[], SafetySource::Voice)
            .await
        {
            Ok(c) => c,
            Err(e) => {
                aurix_metrics::SAFETY_CHECKS
                    .with_label_values(&["voice", "error"])
                    .inc();
                warn!("safety: classifier failed for a transcript: {e}");
                None
            }
        };
        let (score, categories) = self.combine(lexicon.as_ref(), classification.as_ref());
        if !self.is_incident(score, lexicon.as_ref(), classification.as_ref()) {
            aurix_metrics::SAFETY_CHECKS
                .with_label_values(&["voice", "clean"])
                .inc();
            self.push_pre_roll(&seg);
            return Ok(());
        }
        aurix_metrics::SAFETY_CHECKS
            .with_label_values(&["voice", "incident"])
            .inc();

        let session_id = self
            .session_in_channel(seg.app_id, seg.channel_id, seg.user_id)
            .await;
        let audio = if self.cfg.voice.evidence && self.evidence.read().is_some() {
            session_id.map(|session_id| {
                let pcm = self.take_clip(&seg);
                AudioEvidence {
                    app_id: seg.app_id,
                    channel_id: seg.channel_id,
                    session_id,
                    user_id: seg.user_id,
                    pcm,
                    sample_rate: seg.sample_rate,
                    started_at: seg.started_at,
                    retention_days: self.cfg.voice.evidence_retention_days,
                }
            })
        } else {
            None
        };
        // The flagged segment itself stays available as pre-roll for a follow-up incident.
        self.push_pre_roll(&seg);

        self.record_incident(Incident {
            app_id: seg.app_id,
            channel_id: Some(seg.channel_id),
            session_id,
            user_id: seg.user_id,
            source: SafetySource::Voice,
            text: text.to_string(),
            masked_text: None,
            language: seg.language.clone(),
            score,
            categories,
            lexicon,
            classification,
            blocked: false,
            context: Vec::new(),
            audio,
            audio_ms: seg.audio_ms,
        })
        .await
    }

    fn push_pre_roll(&self, seg: &VoiceSegment) {
        let keep = self.cfg.voice.evidence_pre_segments as usize;
        if !self.cfg.voice.evidence || keep == 0 {
            return;
        }
        let now = std::time::Instant::now();
        let mut rolls = self.pre_roll.lock();
        rolls.retain(|_, r| now.duration_since(r.last_seen) < PRE_ROLL_IDLE);
        let roll = rolls
            .entry((seg.channel_id, seg.user_id))
            .or_insert_with(|| PreRoll {
                segments: VecDeque::new(),
                sample_rate: seg.sample_rate,
                last_seen: now,
            });
        if roll.sample_rate != seg.sample_rate {
            roll.segments.clear();
            roll.sample_rate = seg.sample_rate;
        }
        roll.last_seen = now;
        roll.segments.push_back(seg.pcm.clone());
        while roll.segments.len() > keep {
            roll.segments.pop_front();
        }
    }

    /// Pre-roll segments followed by the flagged one, as one PCM buffer.
    fn take_clip(&self, seg: &VoiceSegment) -> Vec<i16> {
        let mut out = Vec::new();
        if let Some(roll) = self.pre_roll.lock().get(&(seg.channel_id, seg.user_id)) {
            if roll.sample_rate == seg.sample_rate {
                for s in &roll.segments {
                    out.extend_from_slice(s);
                }
            }
        }
        out.extend_from_slice(&seg.pcm);
        out
    }

    async fn session_in_channel(
        &self,
        app_id: AppId,
        channel_id: ChannelId,
        user_id: UserId,
    ) -> Option<SessionId> {
        match aurix_db::queries::get_channel_members(&self.pool, app_id.0, channel_id.0).await {
            Ok(rows) => rows
                .into_iter()
                .find(|m| m.user_id == user_id.0)
                .map(|m| SessionId(m.session_id)),
            Err(e) => {
                warn!("safety: membership lookup failed: {e}");
                None
            }
        }
    }

    // ── Text ──

    async fn check_text(&self, req: FilterRequest<'_>) -> Verdict {
        let text = req.text;
        let lexicon = self.lexicon_verdict(text);
        if let Some(v) = lexicon
            .as_ref()
            .filter(|v| v.action == Some(LexiconAction::Block))
        {
            let (score, categories) = self.combine(Some(v), None);
            aurix_metrics::SAFETY_CHECKS
                .with_label_values(&["text", "blocked"])
                .inc();
            self.spawn_text_incident(&req, None, Some(v.clone()), None, score, categories, true);
            return Verdict::Block("Message rejected by content filter".into());
        }

        let classification = if self.cfg.text.classify && self.classifier.is_some() {
            let context: Vec<String> = self
                .context_for(req.app_id, req.channel_id, req.from_user_id, req.to_user_id)
                .into_iter()
                .map(|m| m.text)
                .collect();
            match self
                .classify(text, None, &context, SafetySource::Text)
                .await
            {
                Ok(c) => c,
                Err(e) => {
                    aurix_metrics::SAFETY_CHECKS
                        .with_label_values(&["text", "error"])
                        .inc();
                    if self.cfg.text.fail_open {
                        warn!("safety: classifier unavailable, delivering with lexicon only: {e}");
                        None
                    } else {
                        warn!("safety: classifier unavailable, blocking message: {e}");
                        return Verdict::Block("Content filter is unavailable".into());
                    }
                }
            }
        } else {
            None
        };

        let (score, categories) = self.combine(lexicon.as_ref(), classification.as_ref());
        let incident = self.is_incident(score, lexicon.as_ref(), classification.as_ref());
        let masked = lexicon.as_ref().and_then(|v| v.masked.clone());
        if incident && score >= self.cfg.text.block_threshold {
            aurix_metrics::SAFETY_CHECKS
                .with_label_values(&["text", "blocked"])
                .inc();
            self.spawn_text_incident(
                &req,
                masked,
                lexicon,
                classification,
                score,
                categories,
                true,
            );
            return Verdict::Block("Message rejected by content filter".into());
        }
        if incident {
            aurix_metrics::SAFETY_CHECKS
                .with_label_values(&["text", "incident"])
                .inc();
            self.spawn_text_incident(
                &req,
                masked.clone(),
                lexicon,
                classification,
                score,
                categories,
                false,
            );
        } else {
            aurix_metrics::SAFETY_CHECKS
                .with_label_values(&["text", "clean"])
                .inc();
        }
        match masked {
            Some(m) => Verdict::Replace(m),
            None => Verdict::Allow,
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn spawn_text_incident(
        &self,
        req: &FilterRequest<'_>,
        masked_text: Option<String>,
        lexicon: Option<LexiconVerdict>,
        classification: Option<Classification>,
        score: f32,
        categories: Vec<String>,
        blocked: bool,
    ) {
        let incident = Incident {
            app_id: req.app_id,
            channel_id: req.channel_id,
            session_id: req.session_id,
            user_id: req.from_user_id,
            source: SafetySource::Text,
            text: req.text.to_string(),
            masked_text,
            language: None,
            score,
            categories,
            lexicon,
            classification,
            blocked,
            context: self.context_for(req.app_id, req.channel_id, req.from_user_id, req.to_user_id),
            audio: None,
            audio_ms: 0,
        };
        let pool = self.pool.clone();
        let events = self.events.clone();
        let cfg = self.cfg.clone();
        let evidence = self.evidence.read().clone();
        let enforcer = self.enforcer.read().clone();
        tokio::spawn(async move {
            let ctx = IncidentContext {
                pool: &pool,
                events: &events,
                cfg: &cfg,
                evidence: evidence.as_ref(),
                enforcer: enforcer.as_ref(),
            };
            if let Err(e) = ctx.record(incident).await {
                warn!("safety: text incident could not be recorded: {e}");
            }
        });
    }

    async fn record_incident(&self, incident: Incident) -> Result<()> {
        let evidence = self.evidence.read().clone();
        let enforcer = self.enforcer.read().clone();
        IncidentContext {
            pool: &self.pool,
            events: &self.events,
            cfg: &self.cfg,
            evidence: evidence.as_ref(),
            enforcer: enforcer.as_ref(),
        }
        .record(incident)
        .await
    }

    // ── Risk ──

    pub async fn user_risk(&self, app_id: AppId, user_id: UserId) -> Result<RiskSnapshot> {
        let now = Utc::now();
        let (score, incidents) = compute_risk(&self.pool, &self.cfg, app_id, user_id, now).await?;
        Ok(RiskSnapshot {
            user_id,
            risk_score: score,
            risk_level: risk_level(score, &self.cfg),
            incidents,
            half_life_secs: self.cfg.risk_half_life_secs,
            computed_at: now,
        })
    }
}

/// Decayed risk of the user's stored incidents at `now` and how many were considered.
async fn compute_risk(
    pool: &DbPool,
    cfg: &SafetyConfig,
    app_id: AppId,
    user_id: UserId,
    now: DateTime<Utc>,
) -> Result<(f32, usize)> {
    let since = now
        - Duration::seconds(
            (cfg.risk_half_life_secs as i64).saturating_mul(RISK_WINDOW_HALF_LIVES),
        );
    let rows = aurix_db::queries::list_safety_incident_scores(
        pool,
        app_id.0,
        user_id.0,
        since,
        RISK_MAX_INCIDENTS,
    )
    .await
    .map_err(|e| AurixError::Database(format!("safety incidents lookup: {e}")))?;
    let n = rows.len();
    let score = decayed_risk(
        rows.into_iter().map(|(s, at)| (s as f32, at)),
        now,
        cfg.risk_half_life_secs,
    );
    Ok((score, n))
}

/// Borrowed view of the service used by detached incident tasks.
struct IncidentContext<'a> {
    pool: &'a DbPool,
    events: &'a EventBus,
    cfg: &'a SafetyConfig,
    evidence: Option<&'a Arc<dyn EvidenceStore>>,
    enforcer: Option<&'a Arc<dyn SafetyEnforcer>>,
}

impl IncidentContext<'_> {
    async fn record(&self, incident: Incident) -> Result<()> {
        let now = Utc::now();
        let incident_id = Uuid::now_v7();

        let recording_id = match (incident.audio.clone(), self.evidence) {
            (Some(audio), Some(store)) => match store.store_audio_evidence(audio).await {
                Ok(stored) => Some(stored.recording_id),
                Err(e) => {
                    warn!("safety: evidence clip for incident {incident_id} not stored: {e}");
                    None
                }
            },
            _ => None,
        };

        let (risk_before, _) =
            compute_risk(self.pool, self.cfg, incident.app_id, incident.user_id, now).await?;
        let risk_after = risk_before + incident.score.max(0.0);
        let level_before = risk_level(risk_before, self.cfg);
        let level_after = risk_level(risk_after, self.cfg);

        let (mute_trigger, kick_trigger) = match incident.source {
            SafetySource::Voice => (self.cfg.voice.auto_mute, self.cfg.voice.auto_kick),
            SafetySource::Text => (self.cfg.text.auto_mute, self.cfg.text.auto_kick),
        };
        let kick = self.enforcer.is_some() && trigger_fires(kick_trigger, level_after);
        let mute = !kick && self.enforcer.is_some() && trigger_fires(mute_trigger, level_after);
        let mut actions: Vec<String> = Vec::new();
        if kick {
            actions.push("kick".into());
        }
        if mute {
            actions.push("mute".into());
        }

        let reason = match incident.categories.first() {
            Some(c) => format!("{} ({c}, score {:.2})", incident.source, incident.score),
            None => format!("{} (score {:.2})", incident.source, incident.score),
        };
        let evidence_json = serde_json::json!({
            "source": incident.source,
            "score": incident.score,
            "categories": incident.categories,
            "text": incident.text,
            "masked_text": incident.masked_text,
            "language": incident.language,
            "blocked": incident.blocked,
            "session_id": incident.session_id,
            "classifier": incident.classification.as_ref().map(|c| serde_json::json!({
                "provider": c.provider,
                "score": c.score,
                "flagged": c.flagged,
                "categories": c.categories,
                "labels": c.labels,
            })),
            "lexicon": incident.lexicon.as_ref().map(|v| serde_json::json!({
                "action": v.action,
                "severity": v.severity,
                "matches": v.matches,
            })),
            "context": incident.context,
            "audio": incident.audio.as_ref().map(|a| serde_json::json!({
                "recording_id": recording_id,
                "started_at": a.started_at,
                "segment_ms": incident.audio_ms,
                "clip_ms": a.pcm.len() as u64 * 1000 / a.sample_rate.max(1) as u64,
                "sample_rate": a.sample_rate,
            })),
            "risk": {
                "before": risk_before,
                "after": risk_after,
                "level": level_after,
            },
            "actions": actions,
        });
        let row = ModerationEventRow {
            id: incident_id,
            app_id: incident.app_id.0,
            channel_id: incident.channel_id.map(|c| c.0),
            target_user_id: incident.user_id.0,
            reporter_user_id: None,
            moderator_user_id: None,
            event_type: match incident.source {
                SafetySource::Voice => EVENT_TYPE_VOICE.to_string(),
                SafetySource::Text => EVENT_TYPE_TEXT.to_string(),
            },
            reason,
            evidence: Some(evidence_json),
            recording_id,
            status: "pending".to_string(),
            resolution: None,
            created_at: now,
            resolved_at: None,
        };
        aurix_db::queries::create_moderation_event(self.pool, &row)
            .await
            .map_err(|e| AurixError::Database(format!("safety incident insert: {e}")))?;

        let classifier_name = incident
            .classification
            .as_ref()
            .map(|c| c.provider.clone())
            .unwrap_or_else(|| "lexicon".to_string());
        self.events.publish(ServerEvent::SafetyIncident {
            app_id: incident.app_id,
            incident_id,
            channel_id: incident.channel_id,
            session_id: incident.session_id,
            user_id: incident.user_id,
            source: incident.source,
            score: incident.score,
            categories: incident.categories.clone(),
            text: Some(incident.text.clone()),
            classifier: classifier_name,
            evidence_recording_id: recording_id,
            risk_score: risk_after,
            risk_level: level_after,
            actions: actions.clone(),
            timestamp: now,
        });
        if level_after != level_before {
            self.events.publish(ServerEvent::SafetyRiskChanged {
                app_id: incident.app_id,
                user_id: incident.user_id,
                session_id: incident.session_id,
                risk_score: risk_after,
                risk_level: level_after,
                previous_level: level_before,
                timestamp: now,
            });
        }
        info!(
            "safety incident {incident_id}: {} user {} score {:.2} risk {:.2} ({}) actions {:?}",
            incident.source,
            incident.user_id,
            incident.score,
            risk_after,
            level_after.as_str(),
            actions
        );

        if let Some(enforcer) = self.enforcer {
            if kick {
                match enforcer
                    .kick(incident.app_id, incident.user_id, incident.channel_id)
                    .await
                {
                    Ok(channels) => {
                        aurix_metrics::SAFETY_ACTIONS
                            .with_label_values(&["kick"])
                            .inc_by(channels.len() as u64);
                    }
                    Err(e) => warn!("safety: automatic kick failed: {e}"),
                }
            } else if mute {
                match enforcer
                    .mute(incident.app_id, incident.user_id, incident.channel_id)
                    .await
                {
                    Ok(channels) => {
                        aurix_metrics::SAFETY_ACTIONS
                            .with_label_values(&["mute"])
                            .inc_by(channels.len() as u64);
                    }
                    Err(e) => warn!("safety: automatic mute failed: {e}"),
                }
            }
        }
        Ok(())
    }
}

/// `TextFilter` stage wrapping the service, so `ChatService` runs it before the webhook.
struct SafetyTextFilter(Arc<SafetyService>);

#[async_trait::async_trait]
impl TextFilter for SafetyTextFilter {
    async fn check(&self, req: FilterRequest<'_>) -> std::result::Result<Verdict, String> {
        Ok(self.0.check_text(req).await)
    }
}

/// Automatic actions through the same code path as human moderators (persisted membership
/// state, SFU, Redis, audit log, cluster events), attributed to the system actor.
pub struct ControlPlaneEnforcer {
    control: Weak<ControlPlane>,
    sfu: Arc<RwLock<SfuNode>>,
}

impl ControlPlaneEnforcer {
    pub fn new(control: &Arc<ControlPlane>, sfu: Arc<RwLock<SfuNode>>) -> Self {
        Self {
            control: Arc::downgrade(control),
            sfu,
        }
    }

    async fn channels_of(
        &self,
        control: &ControlPlane,
        app_id: AppId,
        user_id: UserId,
        channel: Option<ChannelId>,
    ) -> Result<Vec<ChannelId>> {
        if let Some(c) = channel {
            return Ok(vec![c]);
        }
        let rows = aurix_db::queries::get_user_channels_in_app(&control.pool, app_id.0, user_id.0)
            .await
            .map_err(|e| AurixError::Database(format!("membership lookup: {e}")))?;
        let channels: std::collections::BTreeSet<Uuid> =
            rows.into_iter().map(|m| m.channel_id).collect();
        Ok(channels.into_iter().map(ChannelId).collect())
    }
}

#[async_trait::async_trait]
impl SafetyEnforcer for ControlPlaneEnforcer {
    async fn mute(
        &self,
        app_id: AppId,
        user_id: UserId,
        channel: Option<ChannelId>,
    ) -> Result<Vec<ChannelId>> {
        let control = self
            .control
            .upgrade()
            .ok_or_else(|| AurixError::Internal("control plane gone".into()))?;
        let mut done = Vec::new();
        for channel_id in self.channels_of(&control, app_id, user_id, channel).await? {
            let target = ModerationTarget {
                app_id,
                channel_id,
                user_id,
                actor: SYSTEM_USER,
                ip: None,
            };
            match set_server_mute(&control, &self.sfu, target, true).await {
                Ok(()) => done.push(channel_id),
                Err(e) => warn!("safety: mute in {channel_id} failed: {e}"),
            }
        }
        Ok(done)
    }

    async fn kick(
        &self,
        app_id: AppId,
        user_id: UserId,
        channel: Option<ChannelId>,
    ) -> Result<Vec<ChannelId>> {
        let control = self
            .control
            .upgrade()
            .ok_or_else(|| AurixError::Internal("control plane gone".into()))?;
        let mut done = Vec::new();
        for channel_id in self.channels_of(&control, app_id, user_id, channel).await? {
            let target = ModerationTarget {
                app_id,
                channel_id,
                user_id,
                actor: SYSTEM_USER,
                ip: None,
            };
            match kick_from_channel(&control, &self.sfu, target, AUTO_KICK_REASON.to_string()).await
            {
                Ok(_) => done.push(channel_id),
                Err(e) => warn!("safety: kick from {channel_id} failed: {e}"),
            }
        }
        Ok(done)
    }
}

/// Evidence export bundle for one incident: the stored row plus how to fetch the clip.
#[derive(Debug, Clone, Serialize)]
pub struct IncidentExport {
    pub incident: ModerationEventRow,
    pub source: Option<SafetySource>,
    pub text: Option<String>,
    pub context: Vec<serde_json::Value>,
    pub audio: Option<IncidentAudio>,
    pub exported_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize)]
pub struct IncidentAudio {
    pub recording_id: Uuid,
    pub format: String,
    pub duration_secs: f64,
    pub size_bytes: i64,
    pub encrypted: bool,
    pub expires_at: DateTime<Utc>,
    /// Relative API path that streams the decrypted clip (`recordings:read`).
    pub download_path: String,
    pub expired: bool,
}

impl IncidentExport {
    pub fn from_row(
        incident: ModerationEventRow,
        recording: Option<aurix_db::models::RecordingRow>,
        now: DateTime<Utc>,
    ) -> Self {
        let ev = incident.evidence.clone().unwrap_or(serde_json::Value::Null);
        let source = match ev.get("source").and_then(|s| s.as_str()) {
            Some("voice") => Some(SafetySource::Voice),
            Some("text") => Some(SafetySource::Text),
            _ => None,
        };
        let text = ev.get("text").and_then(|t| t.as_str()).map(str::to_owned);
        let context = ev
            .get("context")
            .and_then(|c| c.as_array())
            .cloned()
            .unwrap_or_default();
        let audio = recording.map(|r| IncidentAudio {
            recording_id: r.id,
            format: r.format.clone(),
            duration_secs: r.duration_secs,
            size_bytes: r.file_size_bytes,
            encrypted: r.encrypted,
            expires_at: r.expires_at,
            download_path: format!("/v1/recordings/{}/download", r.id),
            expired: r.expires_at <= now,
        });
        Self {
            incident,
            source,
            text,
            context,
            audio,
            exported_at: now,
        }
    }
}

/// Whether the given moderation event was produced by the safety pipeline.
pub fn is_safety_event(row: &ModerationEventRow) -> bool {
    row.event_type == EVENT_TYPE_VOICE || row.event_type == EVENT_TYPE_TEXT
}

/// Source filter string (`voice` / `text`) → stored `event_type`.
pub fn event_type_for_source(source: &str) -> Option<&'static str> {
    match source {
        "voice" => Some(EVENT_TYPE_VOICE),
        "text" => Some(EVENT_TYPE_TEXT),
        _ => None,
    }
}
