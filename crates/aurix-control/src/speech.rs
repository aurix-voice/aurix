//! Text-to-speech policy on top of the media engine: who may speak, what text and voice,
//! how often — and the node-wide bookkeeping that turns engine progress into events.
//!
//! Two request shapes exist:
//! * a player's `TtsSpeak` over the control WebSocket is spoken as that participant (same
//!   routing and receiver preferences as their microphone) and is content-filtered like chat;
//! * an operator's `POST /v1/channels/:id/tts` is an announcement: it is replicated through
//!   the event bus and every node with local participants of the channel plays it to them.

use crate::chat::{ChatService, FilterRequest};
use crate::event_bus::{EventBus, ServerEvent};
use aurix_common::config::TtsConfig;
use aurix_common::error::{AurixError, Result};
use aurix_common::rate_limit::RateLimiter;
use aurix_common::tts_stt::{HttpProviderOptions, HttpTtsProvider, TtsProvider};
use aurix_common::types::{AppId, ChannelId, SessionId};
use aurix_media::tts::{TtsEngine, TtsEngineOptions, TtsRequest, TtsSource, TtsStatusEvent};
use aurix_media::{MediaSession, SfuNode};
use parking_lot::RwLock;
use std::sync::Arc;
use std::time::Duration;
use tracing::{debug, info, warn};
use uuid::Uuid;

/// A player's request as received over the WebSocket.
pub struct ParticipantSpeak {
    pub app_id: AppId,
    pub channel_id: ChannelId,
    pub session: Arc<MediaSession>,
    pub display_name: String,
    pub text: String,
    pub voice: Option<String>,
    pub to_channel: bool,
    pub to_self: bool,
    pub client_ref: Option<String>,
}

pub struct SpeechService {
    cfg: TtsConfig,
    engine: Option<Arc<TtsEngine>>,
    events: Arc<EventBus>,
    chat: Arc<ChatService>,
    rate: RateLimiter,
}

impl SpeechService {
    pub fn new(
        cfg: TtsConfig,
        downlink_bitrate_bps: i32,
        events: Arc<EventBus>,
        chat: Arc<ChatService>,
    ) -> Self {
        let provider: Option<Arc<dyn TtsProvider>> = match (&cfg.enabled, &cfg.endpoint) {
            (true, Some(endpoint)) => Some(Arc::new(HttpTtsProvider::new(
                endpoint,
                HttpProviderOptions {
                    api_key: cfg.api_key.clone(),
                    model: cfg.model.clone(),
                    timeout: Some(Duration::from_millis(cfg.timeout_ms)),
                },
            ))),
            _ => None,
        };
        Self::with_provider(cfg, downlink_bitrate_bps, events, chat, provider)
    }

    pub fn with_provider(
        cfg: TtsConfig,
        downlink_bitrate_bps: i32,
        events: Arc<EventBus>,
        chat: Arc<ChatService>,
        provider: Option<Arc<dyn TtsProvider>>,
    ) -> Self {
        let engine = provider.map(|p| {
            info!("Text-to-speech enabled (provider {})", p.provider_name());
            Arc::new(TtsEngine::new(
                p,
                TtsEngineOptions {
                    max_audio: Duration::from_secs(u64::from(cfg.max_audio_secs)),
                    bitrate_bps: downlink_bitrate_bps,
                    max_concurrent_synth: cfg.max_concurrent_requests as usize,
                    max_queued_per_session: cfg.max_queued_per_session,
                    max_queued_per_channel: cfg.max_queued_per_channel,
                },
            ))
        });
        // The bucket holds a minute's worth of requests and refills `per_minute` tokens per
        // second; a request costs 60 tokens, so the sustained rate is `per_minute` per minute.
        let per_minute = cfg.requests_per_minute_per_session.max(1);
        let rate = RateLimiter::new(per_minute, per_minute.saturating_mul(60));
        Self {
            cfg,
            engine,
            events,
            chat,
            rate,
        }
    }

    pub fn enabled(&self) -> bool {
        self.engine.is_some()
    }

    pub fn config(&self) -> &TtsConfig {
        &self.cfg
    }

    pub fn engine(&self) -> Option<&Arc<TtsEngine>> {
        self.engine.as_ref()
    }

    /// Bind the engine to the media plane and start forwarding its progress to the event bus.
    /// Call once after the SFU has started.
    pub fn start(&self, sfu: &SfuNode) {
        let Some(engine) = &self.engine else {
            return;
        };
        let Some(router) = sfu.router() else {
            warn!("Text-to-speech configured but the media plane has no router");
            return;
        };
        engine.attach_router(router.clone());
        let mut rx = engine.subscribe();
        let events = self.events.clone();
        tokio::spawn(async move {
            loop {
                match rx.recv().await {
                    Ok(ev) => events.publish(status_event(ev)),
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                        warn!("TTS status stream lagged by {n} events");
                    }
                    Err(_) => return,
                }
            }
        });
    }

    fn engine_or_disabled(&self) -> Result<&Arc<TtsEngine>> {
        self.engine.as_ref().ok_or(AurixError::TtsDisabled)
    }

    /// Configured voice list (the first entry is the default).
    pub fn voices(&self) -> Vec<String> {
        let mut v = vec![self.cfg.default_voice.clone()];
        v.extend(
            self.cfg
                .voices
                .iter()
                .filter(|x| **x != self.cfg.default_voice)
                .cloned(),
        );
        v
    }

    fn resolve_voice(&self, voice: Option<&str>) -> Result<String> {
        match voice {
            None => Ok(self.cfg.default_voice.clone()),
            Some(v) if self.cfg.voices.iter().any(|x| x == v) => Ok(v.to_string()),
            Some(_) => Err(AurixError::Validation("Unknown voice".into())),
        }
    }

    fn validate_text(&self, text: &str) -> Result<()> {
        if text.trim().is_empty() {
            return Err(AurixError::Validation("Text is empty".into()));
        }
        if text.chars().count() > self.cfg.max_text_chars {
            return Err(AurixError::Validation(format!(
                "Text exceeds {} characters",
                self.cfg.max_text_chars
            )));
        }
        if text
            .chars()
            .any(|c| c.is_control() && c != '\n' && c != '\t')
        {
            return Err(AurixError::Validation(
                "Text contains control characters".into(),
            ));
        }
        Ok(())
    }

    /// Speak on behalf of a player. The caller has verified channel membership; this applies
    /// the tenant policy (client requests allowed, voice, length, per-session rate, content
    /// filter) and queues the request. Returns the request id; progress arrives as
    /// `TtsStatus` events for the session.
    pub async fn speak_as_participant(&self, req: ParticipantSpeak) -> Result<Uuid> {
        let engine = self.engine_or_disabled()?;
        if !self.cfg.allow_client_requests {
            return Err(AurixError::AuthorizationDenied(
                "Clients may not request text-to-speech".into(),
            ));
        }
        if !req.to_channel && !req.to_self {
            return Err(AurixError::Validation("Nowhere to play the speech".into()));
        }
        self.validate_text(&req.text)?;
        let voice = self.resolve_voice(req.voice.as_deref())?;
        if !self
            .rate
            .check_with_cost(&format!("tts:{}", req.session.session_id), 60.0)
        {
            return Err(AurixError::RateLimitExceeded(
                "Too many text-to-speech requests".into(),
            ));
        }
        let mut text = req.text;
        if req.to_channel && self.chat.has_filter() {
            let filter_req = FilterRequest {
                app_id: req.app_id,
                channel_id: Some(req.channel_id),
                from_user_id: req.session.user_id,
                to_user_id: None,
                display_name: &req.display_name,
                text: &text,
                metadata: None,
            };
            if let Some(replacement) = self
                .chat
                .filter_text(filter_req, self.cfg.max_text_chars * 4)
                .await?
            {
                text = replacement;
            }
        }
        engine.submit(TtsRequest {
            app_id: req.app_id,
            channel_id: req.channel_id,
            text,
            voice,
            source: TtsSource::Participant {
                session: req.session,
                to_channel: req.to_channel,
                to_self: req.to_self,
            },
            client_ref: req.client_ref,
            request_id: None,
        })
    }

    /// Cancel a player's request (`request_id`) or everything they queued (`None`).
    /// Returns how many requests were cancelled.
    pub fn cancel_for_session(&self, session_id: SessionId, request_id: Option<Uuid>) -> usize {
        let Some(engine) = &self.engine else {
            return 0;
        };
        match request_id {
            Some(id) => usize::from(engine.cancel(&id, Some(session_id))),
            None => engine.cancel_session(&session_id),
        }
    }

    /// Operator announcement: validated here and replicated to every node; the caller has
    /// verified the channel belongs to `app_id`. Returns the request id shared by all nodes.
    pub fn announce(
        &self,
        app_id: AppId,
        channel_id: ChannelId,
        text: &str,
        voice: Option<&str>,
    ) -> Result<Uuid> {
        self.engine_or_disabled()?;
        self.validate_text(text)?;
        let voice = self.resolve_voice(voice)?;
        let request_id = Uuid::new_v4();
        self.events.publish(ServerEvent::TtsAnnouncement {
            app_id,
            channel_id,
            request_id,
            text: text.to_string(),
            voice,
        });
        Ok(request_id)
    }

    /// Play a replicated announcement to this node's participants of the channel (no-op when
    /// the channel has none here).
    pub fn play_announcement(
        &self,
        sfu: &RwLock<SfuNode>,
        app_id: AppId,
        channel_id: ChannelId,
        request_id: Uuid,
        text: String,
        voice: String,
    ) {
        let Some(engine) = &self.engine else {
            return;
        };
        let hosted = {
            let sfu = sfu.read();
            sfu.get_channel(&channel_id)
                .is_some_and(|c| c.app_id == app_id && c.participant_count() > 0)
        };
        if !hosted {
            return;
        }
        if let Err(e) = engine.submit(TtsRequest {
            app_id,
            channel_id,
            text,
            voice,
            source: TtsSource::System,
            client_ref: None,
            request_id: Some(request_id),
        }) {
            debug!("announcement {request_id} not queued in {channel_id}: {e}");
            self.events.publish(ServerEvent::TtsStatus {
                app_id,
                channel_id,
                request_id,
                session_id: None,
                user_id: None,
                client_ref: None,
                state: aurix_common::protocol::TtsState::Failed,
                duration_ms: None,
                message: Some(e.public_message()),
            });
        }
    }
}

fn status_event(ev: TtsStatusEvent) -> ServerEvent {
    let TtsStatusEvent {
        request_id,
        app_id,
        channel_id,
        session_id,
        user_id,
        client_ref,
        state,
        duration_ms,
        message,
    } = ev;
    ServerEvent::TtsStatus {
        app_id,
        channel_id,
        request_id,
        session_id,
        user_id,
        client_ref,
        state,
        duration_ms,
        message,
    }
}
