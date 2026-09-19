//! Real-time audio streaming of a channel to an operator-controlled service.
//!
//! A *live stream* is a node-local tap on the authenticated, non-E2EE Opus packets of one
//! channel. Frames are either pulled by the operator over a WebSocket on this node
//! (`GET /v1/channels/:id/audio/stream`) or pushed by the node to an operator WebSocket
//! endpoint (`POST /v1/channels/:id/audio/streams`). Nothing is written to disk; the same
//! consent rules as for stored recordings apply, and a participant's frames are never
//! emitted before that participant accepted.
//!
//! Wire format (both directions of transport, one WebSocket message per frame):
//!
//! * text messages: JSON [`ControlFrame`]s — `hello`, `participant`, `dropped`, `end`;
//! * binary messages: one audio frame with a fixed 36-byte big-endian header:
//!
//! ```text
//!  0     u8   version (1)
//!  1     u8   codec: 1 = Opus packet, 2 = PCM S16LE 48 kHz mono
//!  2     u8   flags: bit0 = timestamp gap since previous frame of this participant,
//!                     bit1 = first frame of this participant
//!  3     u8   reserved (0)
//!  4..8  u32  SSRC of the participant on this node
//!  8..12 u32  RTP timestamp (48 kHz clock)
//! 12..20 u64  server receive time, Unix milliseconds
//! 20..36 [16] participant user id (RFC 4122 byte order)
//! 36..   payload
//! ```
//!
//! The media hot path only does a `try_send` into a bounded queue per stream; a slow
//! consumer loses frames (accounted in `dropped` control frames and the stream status)
//! instead of adding latency to other participants.

use aurix_common::config::LiveStreamConfig;
use aurix_common::error::{AurixError, Result};
use aurix_common::net::{resolve_outbound, validate_outbound_url};
use aurix_common::types::{AppId, ChannelId, RecordingConsent, UserId};
use bytes::Bytes;
use chrono::{DateTime, Utc};
use futures_util::{SinkExt, StreamExt};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::{HeaderName, HeaderValue};
use tokio_tungstenite::tungstenite::Message;
use tracing::{info, warn};
use url::Url;
use uuid::Uuid;

pub const FRAME_VERSION: u8 = 1;
pub const FRAME_HEADER_LEN: usize = 36;
pub const CODEC_OPUS: u8 = 1;
pub const CODEC_PCM_S16LE: u8 = 2;
pub const FLAG_GAP: u8 = 0b01;
pub const FLAG_FIRST: u8 = 0b10;
pub const SAMPLE_RATE: u32 = 48_000;
pub const FRAME_SAMPLES: u32 = 960;

/// Largest RTP jump still considered "continuous" (250 ms at 48 kHz).
const CONTINUITY_SAMPLES: u32 = 12_000;
const MAX_PCM_SAMPLES: usize = 5760;
const MAX_PUSH_HEADERS: usize = 8;
const MAX_PUSH_HEADER_LEN: usize = 1024;
const MAX_PUSH_BACKOFF: Duration = Duration::from_secs(30);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum StreamFormat {
    #[default]
    Opus,
    PcmS16le,
}

impl StreamFormat {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Opus => "opus",
            Self::PcmS16le => "pcm_s16le",
        }
    }

    fn codec_byte(self) -> u8 {
        match self {
            Self::Opus => CODEC_OPUS,
            Self::PcmS16le => CODEC_PCM_S16LE,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StreamMode {
    /// The operator holds a WebSocket to this node and receives frames.
    Pull,
    /// This node connects to the operator's WebSocket endpoint and sends frames.
    Push,
}

impl StreamMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pull => "pull",
            Self::Push => "push",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StreamState {
    Streaming,
    Connecting,
    Reconnecting,
}

/// Operator-supplied parameters of a new stream.
#[derive(Debug, Clone, Default)]
pub struct StreamSpec {
    pub format: StreamFormat,
    /// Restrict to these participants; `None` streams every participant of the channel.
    pub users: Option<Vec<UserId>>,
    /// Free-form operator label (≤ 128 chars), echoed in status and events.
    pub label: Option<String>,
}

/// Push target. Header values may carry credentials and are never echoed back.
#[derive(Debug, Clone)]
pub struct PushTarget {
    pub url: String,
    pub headers: Vec<(String, String)>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ParticipantEvent {
    /// First frame of the participant is about to follow.
    AudioStarted,
    /// Consent state changed (`consent` is set).
    Consent,
    /// The participant left the channel (or was purged); no more frames for them.
    Left,
}

/// JSON control frames interleaved with binary audio frames.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ControlFrame {
    Hello {
        stream_id: Uuid,
        app_id: Uuid,
        channel_id: ChannelId,
        format: StreamFormat,
        sample_rate: u32,
        channels: u8,
        frame_ms: u32,
        frame_version: u8,
        users: Option<Vec<UserId>>,
        consent_required: bool,
        label: Option<String>,
        started_at: DateTime<Utc>,
    },
    Participant {
        user_id: UserId,
        ssrc: u32,
        event: ParticipantEvent,
        consent: Option<RecordingConsent>,
    },
    /// Frames the consumer did not receive because its queue was full.
    Dropped { frames: u64 },
    End {
        reason: String,
        frames_sent: u64,
        frames_dropped: u64,
    },
}

#[derive(Debug, Clone)]
pub enum Outgoing {
    Control(ControlFrame),
    Audio(Bytes),
}

impl Outgoing {
    pub fn into_ws_message(self) -> Message {
        match self {
            Self::Control(c) => Message::Text(
                serde_json::to_string(&c).unwrap_or_else(|_| "{\"type\":\"end\"}".into()),
            ),
            Self::Audio(b) => Message::Binary(b.to_vec()),
        }
    }
}

/// Operator-visible status of a stream. Push header values are never included and the
/// URL is reduced to scheme/host/path.
#[derive(Debug, Clone, Serialize)]
pub struct LiveStreamInfo {
    pub id: Uuid,
    pub app_id: Uuid,
    pub channel_id: ChannelId,
    pub mode: StreamMode,
    pub format: StreamFormat,
    pub state: StreamState,
    pub users: Option<Vec<UserId>>,
    pub label: Option<String>,
    pub push_url: Option<String>,
    pub started_at: DateTime<Utc>,
    pub frames_sent: u64,
    pub frames_dropped: u64,
    pub reconnects: u32,
    pub consent: HashMap<UserId, RecordingConsent>,
}

/// Lifecycle notifications drained by the server and turned into `ServerEvent`s.
#[derive(Debug, Clone)]
pub enum LiveNotice {
    Opened(LiveStreamInfo),
    Closed {
        info: LiveStreamInfo,
        reason: String,
    },
}

struct Talker {
    ssrc: u32,
    last_ts: Option<u32>,
    decoder: Option<opus::Decoder>,
}

struct Tap {
    id: Uuid,
    app_id: Uuid,
    channel_id: ChannelId,
    mode: StreamMode,
    format: StreamFormat,
    state: StreamState,
    users: Option<HashSet<UserId>>,
    users_order: Option<Vec<UserId>>,
    label: Option<String>,
    push_url: Option<String>,
    started_at: DateTime<Utc>,
    deadline: Option<DateTime<Utc>>,
    tx: mpsc::Sender<Outgoing>,
    consent: HashMap<UserId, RecordingConsent>,
    talkers: HashMap<UserId, Talker>,
    frames_sent: u64,
    frames_dropped: u64,
    /// Drops not yet announced to the consumer with a `dropped` frame.
    unannounced_drops: u64,
    reconnects: u32,
}

impl Tap {
    fn info(&self) -> LiveStreamInfo {
        LiveStreamInfo {
            id: self.id,
            app_id: self.app_id,
            channel_id: self.channel_id,
            mode: self.mode,
            format: self.format,
            state: self.state,
            users: self.users_order.clone(),
            label: self.label.clone(),
            push_url: self.push_url.clone(),
            started_at: self.started_at,
            frames_sent: self.frames_sent,
            frames_dropped: self.frames_dropped,
            reconnects: self.reconnects,
            consent: self.consent.clone(),
        }
    }

    fn wants_user(&self, user_id: &UserId) -> bool {
        self.users.as_ref().is_none_or(|u| u.contains(user_id))
    }

    /// Non-blocking enqueue. Audio is dropped when the queue is full; control frames evict
    /// nothing but are counted as drops too so the consumer notices.
    fn enqueue(&mut self, msg: Outgoing) -> bool {
        let is_audio = matches!(msg, Outgoing::Audio(_));
        match self.tx.try_send(msg) {
            Ok(()) => {
                if is_audio {
                    self.frames_sent += 1;
                }
                true
            }
            Err(mpsc::error::TrySendError::Full(_)) => {
                self.frames_dropped += 1;
                self.unannounced_drops += 1;
                false
            }
            Err(mpsc::error::TrySendError::Closed(_)) => false,
        }
    }

    fn control(&mut self, frame: ControlFrame) -> bool {
        self.enqueue(Outgoing::Control(frame))
    }
}

#[derive(Default)]
struct Registry {
    by_id: HashMap<Uuid, Tap>,
    by_channel: HashMap<ChannelId, Vec<Uuid>>,
}

impl Registry {
    fn remove(&mut self, id: &Uuid) -> Option<Tap> {
        let tap = self.by_id.remove(id)?;
        if let Some(list) = self.by_channel.get_mut(&tap.channel_id) {
            list.retain(|x| x != id);
            if list.is_empty() {
                self.by_channel.remove(&tap.channel_id);
            }
        }
        Some(tap)
    }
}

/// Handle returned to the consumer side of a stream (WebSocket handler or push task).
pub struct StreamReceiver {
    pub id: Uuid,
    pub rx: mpsc::Receiver<Outgoing>,
}

pub struct LiveStreams {
    cfg: LiveStreamConfig,
    require_consent: bool,
    production: bool,
    max_recording_duration_secs: u64,
    reg: Mutex<Registry>,
    notices: mpsc::UnboundedSender<LiveNotice>,
    notice_rx: Mutex<Option<mpsc::UnboundedReceiver<LiveNotice>>>,
}

impl LiveStreams {
    pub fn new(
        cfg: LiveStreamConfig,
        require_consent: bool,
        production: bool,
        max_recording_duration_secs: u64,
    ) -> Self {
        let (notices, notice_rx) = mpsc::unbounded_channel();
        Self {
            cfg,
            require_consent,
            production,
            max_recording_duration_secs,
            reg: Mutex::new(Registry::default()),
            notices,
            notice_rx: Mutex::new(Some(notice_rx)),
        }
    }

    pub fn enabled(&self) -> bool {
        self.cfg.enabled
    }

    pub fn config(&self) -> &LiveStreamConfig {
        &self.cfg
    }

    /// The lifecycle notice receiver; may be taken once (by the server binary).
    pub fn take_notices(&self) -> Option<mpsc::UnboundedReceiver<LiveNotice>> {
        self.notice_rx.lock().take()
    }

    fn private_allowed(&self) -> bool {
        self.cfg.allow_private_urls.unwrap_or(!self.production)
    }

    fn tls_required(&self) -> bool {
        self.cfg.require_tls.unwrap_or(self.production)
    }

    /// Validate a push URL against the SSRF/TLS policy (no credentials, no fragments,
    /// private targets only when explicitly allowed).
    pub fn validate_push_url(&self, raw: &str) -> Result<Url> {
        if !self.cfg.push_enabled {
            return Err(AurixError::InvalidConfiguration(
                "recording.live.push_enabled is false".into(),
            ));
        }
        validate_outbound_url(
            raw,
            "wss",
            "ws",
            self.tls_required(),
            self.private_allowed(),
            "recording.live.require_tls",
        )
    }

    fn validate_spec(&self, spec: &StreamSpec) -> Result<()> {
        if !self.cfg.enabled {
            return Err(AurixError::InvalidConfiguration(
                "Live audio streaming is disabled (recording.live.enabled)".into(),
            ));
        }
        if spec.format == StreamFormat::PcmS16le && !self.cfg.allow_pcm {
            return Err(AurixError::Validation(
                "PCM streams are disabled (recording.live.allow_pcm)".into(),
            ));
        }
        if let Some(users) = &spec.users {
            if users.is_empty() || users.len() > 256 {
                return Err(AurixError::Validation(
                    "users must contain 1..=256 participants".into(),
                ));
            }
        }
        if let Some(label) = &spec.label {
            if label.chars().count() > 128 {
                return Err(AurixError::Validation("label must be <= 128 chars".into()));
            }
        }
        Ok(())
    }

    /// Register a stream and return the consumer end. The `hello` control frame is already
    /// queued. Fails when the node-wide/per-app/per-channel limits are reached.
    pub fn open(
        &self,
        app_id: AppId,
        channel_id: ChannelId,
        spec: StreamSpec,
        mode: StreamMode,
        push_url: Option<&Url>,
    ) -> Result<(LiveStreamInfo, StreamReceiver)> {
        self.validate_spec(&spec)?;
        let (tx, rx) = mpsc::channel(self.cfg.queue_frames);
        let id = Uuid::new_v4();
        let now = Utc::now();
        let duration_limit = if self.cfg.max_duration_secs > 0 {
            self.cfg.max_duration_secs
        } else {
            self.max_recording_duration_secs
        };
        let deadline =
            (duration_limit > 0).then(|| now + chrono::Duration::seconds(duration_limit as i64));
        let mut tap = Tap {
            id,
            app_id: app_id.0,
            channel_id,
            mode,
            format: spec.format,
            state: match mode {
                StreamMode::Pull => StreamState::Streaming,
                StreamMode::Push => StreamState::Connecting,
            },
            users: spec.users.as_ref().map(|u| u.iter().copied().collect()),
            users_order: spec.users.clone(),
            label: spec.label.clone(),
            push_url: push_url.map(redact_url),
            started_at: now,
            deadline,
            tx,
            consent: HashMap::new(),
            talkers: HashMap::new(),
            frames_sent: 0,
            frames_dropped: 0,
            unannounced_drops: 0,
            reconnects: 0,
        };
        let info = {
            let mut reg = self.reg.lock();
            let in_channel = reg.by_channel.get(&channel_id).map_or(0, |v| v.len());
            if in_channel >= self.cfg.max_per_channel as usize {
                return Err(AurixError::Conflict(format!(
                    "Channel already has {} live streams (recording.live.max_per_channel)",
                    in_channel
                )));
            }
            let in_app = reg.by_id.values().filter(|t| t.app_id == app_id.0).count();
            if in_app >= self.cfg.max_per_app as usize {
                return Err(AurixError::Conflict(format!(
                    "Application already has {} live streams on this node (recording.live.max_per_app)",
                    in_app
                )));
            }
            tap.control(ControlFrame::Hello {
                stream_id: id,
                app_id: app_id.0,
                channel_id,
                format: spec.format,
                sample_rate: SAMPLE_RATE,
                channels: 1,
                frame_ms: 20,
                frame_version: FRAME_VERSION,
                users: spec.users.clone(),
                consent_required: self.require_consent,
                label: spec.label.clone(),
                started_at: now,
            });
            let info = tap.info();
            reg.by_channel.entry(channel_id).or_default().push(id);
            reg.by_id.insert(id, tap);
            info
        };
        info!(
            "Live stream {} opened: {:?}/{:?} for channel {} (app {})",
            id, mode, spec.format, channel_id, app_id.0
        );
        let _ = self.notices.send(LiveNotice::Opened(info.clone()));
        Ok((info, StreamReceiver { id, rx }))
    }

    /// Close a stream. With `app_id`, closing a stream of another tenant is refused as
    /// "not found" (existence must not leak across tenants).
    pub fn close(&self, app_id: Option<AppId>, id: Uuid, reason: &str) -> Option<LiveStreamInfo> {
        let tap = {
            let mut reg = self.reg.lock();
            match reg.by_id.get(&id) {
                Some(t) if app_id.is_none_or(|a| a.0 == t.app_id) => reg.remove(&id),
                _ => None,
            }
        };
        let mut tap = tap?;
        tap.control(ControlFrame::End {
            reason: reason.to_string(),
            frames_sent: tap.frames_sent,
            frames_dropped: tap.frames_dropped,
        });
        let info = tap.info();
        info!(
            "Live stream {} closed ({reason}): {} frames sent, {} dropped",
            id, info.frames_sent, info.frames_dropped
        );
        let _ = self.notices.send(LiveNotice::Closed {
            info: info.clone(),
            reason: reason.to_string(),
        });
        Some(info)
    }

    pub fn get(&self, app_id: AppId, id: Uuid) -> Option<LiveStreamInfo> {
        let reg = self.reg.lock();
        reg.by_id
            .get(&id)
            .filter(|t| t.app_id == app_id.0)
            .map(Tap::info)
    }

    pub fn list(&self, app_id: AppId, channel_id: Option<ChannelId>) -> Vec<LiveStreamInfo> {
        let reg = self.reg.lock();
        let mut v: Vec<LiveStreamInfo> = reg
            .by_id
            .values()
            .filter(|t| t.app_id == app_id.0 && channel_id.is_none_or(|c| c == t.channel_id))
            .map(Tap::info)
            .collect();
        v.sort_by_key(|i| i.started_at);
        v
    }

    /// Ids of streams on `channel_id` (any tenant; used for client disclosure).
    pub fn active_in_channel(&self, channel_id: &ChannelId) -> Vec<Uuid> {
        self.reg
            .lock()
            .by_channel
            .get(channel_id)
            .cloned()
            .unwrap_or_default()
    }

    pub fn wants_channel(&self, channel_id: &ChannelId) -> bool {
        self.reg.lock().by_channel.contains_key(channel_id)
    }

    pub fn is_active(&self, id: &Uuid) -> bool {
        self.reg.lock().by_id.contains_key(id)
    }

    /// Consent decision of a channel member for a live stream. Returns `Ok(false)` when
    /// `id` is not a live stream on this node so callers can fall back to stored recordings.
    pub fn set_consent(
        &self,
        app_id: AppId,
        id: Uuid,
        user_id: UserId,
        consent: RecordingConsent,
    ) -> Result<bool> {
        let mut reg = self.reg.lock();
        let Some(tap) = reg.by_id.get_mut(&id) else {
            return Ok(false);
        };
        if tap.app_id != app_id.0 {
            return Ok(false);
        }
        if !tap.wants_user(&user_id) {
            return Err(AurixError::AuthorizationDenied(
                "This stream does not include you".into(),
            ));
        }
        tap.consent.insert(user_id, consent);
        let ssrc = tap.talkers.get(&user_id).map_or(0, |t| t.ssrc);
        if consent == RecordingConsent::Declined {
            tap.talkers.remove(&user_id);
        }
        tap.control(ControlFrame::Participant {
            user_id,
            ssrc,
            event: ParticipantEvent::Consent,
            consent: Some(consent),
        });
        Ok(true)
    }

    /// Feed one authenticated Opus packet into every stream of the channel.
    pub fn on_audio(
        &self,
        channel_id: ChannelId,
        user_id: UserId,
        ssrc: u32,
        rtp_timestamp: u32,
        payload: &[u8],
    ) {
        if payload.is_empty() {
            return;
        }
        let mut reg = self.reg.lock();
        let Some(ids) = reg.by_channel.get(&channel_id).cloned() else {
            return;
        };
        let now_ms = Utc::now().timestamp_millis().max(0) as u64;
        let require_consent = self.require_consent;
        for id in ids {
            let Some(tap) = reg.by_id.get_mut(&id) else {
                continue;
            };
            if !tap.wants_user(&user_id) {
                continue;
            }
            let consent = *tap.consent.entry(user_id).or_insert(if require_consent {
                RecordingConsent::Pending
            } else {
                RecordingConsent::Accepted
            });
            if consent != RecordingConsent::Accepted {
                continue;
            }
            let mut flags = 0u8;
            let talker = match tap.talkers.get_mut(&user_id) {
                Some(t) => {
                    if t.ssrc != ssrc {
                        t.ssrc = ssrc;
                        t.last_ts = None;
                    }
                    t
                }
                None => {
                    flags |= FLAG_FIRST;
                    let decoder = match tap.format {
                        StreamFormat::Opus => None,
                        StreamFormat::PcmS16le => {
                            match opus::Decoder::new(SAMPLE_RATE, opus::Channels::Mono) {
                                Ok(d) => Some(d),
                                Err(e) => {
                                    warn!("Live stream {}: opus decoder: {e}", id);
                                    continue;
                                }
                            }
                        }
                    };
                    tap.control(ControlFrame::Participant {
                        user_id,
                        ssrc,
                        event: ParticipantEvent::AudioStarted,
                        consent: None,
                    });
                    tap.talkers.entry(user_id).or_insert(Talker {
                        ssrc,
                        last_ts: None,
                        decoder,
                    })
                }
            };
            if let Some(last) = talker.last_ts {
                let delta = rtp_timestamp.wrapping_sub(last);
                if delta > CONTINUITY_SAMPLES && delta < u32::MAX - CONTINUITY_SAMPLES {
                    flags |= FLAG_GAP;
                }
            }
            talker.last_ts = Some(rtp_timestamp);
            let body: Vec<u8> = match talker.decoder.as_mut() {
                None => payload.to_vec(),
                Some(dec) => {
                    let mut pcm = vec![0i16; MAX_PCM_SAMPLES];
                    match dec.decode(payload, &mut pcm, false) {
                        Ok(n) => {
                            pcm.truncate(n);
                            pcm.iter().flat_map(|s| s.to_le_bytes()).collect()
                        }
                        Err(e) => {
                            warn!("Live stream {}: opus decode failed: {e}", id);
                            continue;
                        }
                    }
                }
            };
            let frame = encode_frame(
                tap.format.codec_byte(),
                flags,
                ssrc,
                rtp_timestamp,
                now_ms,
                &user_id,
                &body,
            );
            if tap.unannounced_drops > 0 && tap.tx.capacity() > 1 {
                let frames = tap.unannounced_drops;
                if tap.control(ControlFrame::Dropped { frames }) {
                    tap.unannounced_drops = 0;
                }
            }
            tap.enqueue(Outgoing::Audio(Bytes::from(frame)));
        }
    }

    /// The participant left the channel: forget their decoder state and tell consumers.
    pub fn on_participant_left(&self, channel_id: ChannelId, user_id: UserId) {
        let mut reg = self.reg.lock();
        let Some(ids) = reg.by_channel.get(&channel_id).cloned() else {
            return;
        };
        for id in ids {
            let Some(tap) = reg.by_id.get_mut(&id) else {
                continue;
            };
            let Some(t) = tap.talkers.remove(&user_id) else {
                continue;
            };
            tap.control(ControlFrame::Participant {
                user_id,
                ssrc: t.ssrc,
                event: ParticipantEvent::Left,
                consent: None,
            });
        }
    }

    /// Drop all per-user state (user erasure). Streams themselves keep running.
    pub fn purge_user(&self, user_id: UserId) {
        let mut reg = self.reg.lock();
        for tap in reg.by_id.values_mut() {
            tap.consent.remove(&user_id);
            tap.talkers.remove(&user_id);
        }
    }

    /// Close every stream of a channel (channel destroyed / operator stop).
    pub fn stop_channel(&self, app_id: AppId, channel_id: &ChannelId, reason: &str) -> usize {
        let ids: Vec<Uuid> = {
            let reg = self.reg.lock();
            reg.by_channel
                .get(channel_id)
                .map(|v| {
                    v.iter()
                        .copied()
                        .filter(|id| reg.by_id.get(id).is_some_and(|t| t.app_id == app_id.0))
                        .collect()
                })
                .unwrap_or_default()
        };
        ids.into_iter()
            .filter(|id| self.close(Some(app_id), *id, reason).is_some())
            .count()
    }

    /// Close streams past their duration limit. Intended to run periodically.
    pub fn enforce_duration_limit(&self) -> usize {
        let now = Utc::now();
        let expired: Vec<Uuid> = self
            .reg
            .lock()
            .by_id
            .values()
            .filter(|t| t.deadline.is_some_and(|d| now >= d))
            .map(|t| t.id)
            .collect();
        expired
            .into_iter()
            .filter(|id| self.close(None, *id, "duration_limit").is_some())
            .count()
    }

    /// Close all streams (shutdown).
    pub fn close_all(&self, reason: &str) -> usize {
        let ids: Vec<Uuid> = self.reg.lock().by_id.keys().copied().collect();
        ids.into_iter()
            .filter(|id| self.close(None, *id, reason).is_some())
            .count()
    }

    fn set_state(&self, id: Uuid, state: StreamState, count_reconnect: bool) {
        if let Some(tap) = self.reg.lock().by_id.get_mut(&id) {
            tap.state = state;
            if count_reconnect {
                tap.reconnects += 1;
            }
        }
    }

    /// Validate operator-supplied push headers (name/value syntax, count, size, and no
    /// hop-by-hop / handshake headers).
    pub fn validate_push_headers(headers: &[(String, String)]) -> Result<()> {
        if headers.len() > MAX_PUSH_HEADERS {
            return Err(AurixError::Validation(format!(
                "at most {MAX_PUSH_HEADERS} headers are allowed"
            )));
        }
        for (name, value) in headers {
            let n = HeaderName::from_bytes(name.as_bytes())
                .map_err(|_| AurixError::Validation(format!("invalid header name {name:?}")))?;
            if matches!(
                n.as_str(),
                "host"
                    | "connection"
                    | "upgrade"
                    | "content-length"
                    | "transfer-encoding"
                    | "sec-websocket-key"
                    | "sec-websocket-version"
                    | "sec-websocket-accept"
                    | "sec-websocket-extensions"
            ) {
                return Err(AurixError::Validation(format!(
                    "header {name:?} is managed by the node"
                )));
            }
            if value.len() > MAX_PUSH_HEADER_LEN {
                return Err(AurixError::Validation(format!(
                    "header {name:?} value exceeds {MAX_PUSH_HEADER_LEN} bytes"
                )));
            }
            HeaderValue::from_str(value).map_err(|_| {
                AurixError::Validation(format!("invalid value for header {name:?}"))
            })?;
        }
        Ok(())
    }

    /// Run the push connector for a registered stream until the stream is closed or the
    /// target stays unreachable for more than `max_reconnects` attempts.
    pub fn spawn_push(self: &Arc<Self>, receiver: StreamReceiver, url: Url, target: PushTarget) {
        let this = self.clone();
        tokio::spawn(async move {
            this.run_push(receiver, url, target).await;
        });
    }

    async fn run_push(self: Arc<Self>, receiver: StreamReceiver, url: Url, target: PushTarget) {
        let id = receiver.id;
        let mut rx = receiver.rx;
        let mut attempt: u32 = 0;
        // The `hello` frame must reach every (re)connected consumer, so it is re-sent on
        // reconnect instead of being consumed from the queue.
        let mut hello: Option<Message> = None;
        loop {
            if !self.is_active(&id) {
                return;
            }
            let ws = match self.connect_push(&url, &target).await {
                Ok(ws) => ws,
                Err(e) => {
                    attempt += 1;
                    warn!(
                        "Live stream {} push connect attempt {}/{} failed: {e}",
                        id, attempt, self.cfg.max_reconnects
                    );
                    if attempt > self.cfg.max_reconnects {
                        self.close(None, id, "push_unreachable");
                        // Drain so the End frame does not linger in memory.
                        while rx.try_recv().is_ok() {}
                        return;
                    }
                    tokio::time::sleep(backoff(attempt)).await;
                    continue;
                }
            };
            self.set_state(id, StreamState::Streaming, attempt > 0);
            let (mut sink, mut source) = ws.split();
            if let Some(h) = &hello {
                if sink.send(h.clone()).await.is_err() {
                    self.set_state(id, StreamState::Reconnecting, false);
                    attempt = 1;
                    continue;
                }
            }
            let outcome = loop {
                tokio::select! {
                    item = rx.recv() => match item {
                        None => break PushOutcome::Finished,
                        Some(out) => {
                            let is_hello = matches!(out, Outgoing::Control(ControlFrame::Hello { .. }));
                            let msg = out.into_ws_message();
                            if is_hello {
                                hello = Some(msg.clone());
                            }
                            if let Err(e) = sink.send(msg).await {
                                break PushOutcome::Lost(e.to_string());
                            }
                        }
                    },
                    incoming = source.next() => match incoming {
                        Some(Ok(Message::Close(_))) => break PushOutcome::Lost("peer closed".into()),
                        Some(Ok(_)) => {}
                        Some(Err(e)) => break PushOutcome::Lost(e.to_string()),
                        None => break PushOutcome::Lost("connection ended".into()),
                    },
                }
            };
            match outcome {
                PushOutcome::Finished => {
                    let _ = sink.close().await;
                    return;
                }
                PushOutcome::Lost(reason) => {
                    if !self.is_active(&id) {
                        return;
                    }
                    warn!("Live stream {} push connection lost: {reason}", id);
                    self.set_state(id, StreamState::Reconnecting, false);
                    attempt = 1;
                    tokio::time::sleep(backoff(attempt)).await;
                }
            }
        }
    }

    async fn connect_push(
        &self,
        url: &Url,
        target: &PushTarget,
    ) -> Result<
        tokio_tungstenite::WebSocketStream<
            tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
        >,
    > {
        let host = url
            .host_str()
            .ok_or_else(|| AurixError::Validation("push URL has no host".into()))?;
        let port = url
            .port_or_known_default()
            .ok_or_else(|| AurixError::Validation("push URL has no port".into()))?;
        let addrs = resolve_outbound(
            host,
            port,
            self.private_allowed(),
            "recording.live.allow_private_urls",
        )
        .await?;
        let timeout = Duration::from_millis(self.cfg.connect_timeout_ms);
        let mut last_err = None;
        for addr in addrs {
            match tokio::time::timeout(timeout, tokio::net::TcpStream::connect(addr)).await {
                Ok(Ok(tcp)) => {
                    let _ = tcp.set_nodelay(true);
                    let mut request = url
                        .as_str()
                        .into_client_request()
                        .map_err(|e| AurixError::Validation(format!("invalid push URL: {e}")))?;
                    for (name, value) in &target.headers {
                        let n = HeaderName::from_bytes(name.as_bytes())
                            .map_err(|_| AurixError::Validation("invalid header".into()))?;
                        let v = HeaderValue::from_str(value)
                            .map_err(|_| AurixError::Validation("invalid header".into()))?;
                        request.headers_mut().insert(n, v);
                    }
                    let handshake =
                        tokio_tungstenite::client_async_tls_with_config(request, tcp, None, None);
                    return match tokio::time::timeout(timeout, handshake).await {
                        Ok(Ok((ws, _resp))) => Ok(ws),
                        Ok(Err(e)) => {
                            Err(AurixError::Transport(format!("websocket handshake: {e}")))
                        }
                        Err(_) => Err(AurixError::Timeout("websocket handshake".into())),
                    };
                }
                Ok(Err(e)) => last_err = Some(AurixError::Transport(format!("connect: {e}"))),
                Err(_) => last_err = Some(AurixError::Timeout("tcp connect".into())),
            }
        }
        Err(last_err.unwrap_or_else(|| AurixError::Transport("no addresses".into())))
    }
}

enum PushOutcome {
    Finished,
    Lost(String),
}

fn backoff(attempt: u32) -> Duration {
    let secs = 1u64 << attempt.clamp(1, 5).saturating_sub(1);
    Duration::from_secs(secs).min(MAX_PUSH_BACKOFF)
}

/// Scheme, host, port and path only — query strings often carry tokens.
fn redact_url(url: &Url) -> String {
    let mut s = format!("{}://{}", url.scheme(), url.host_str().unwrap_or(""));
    if let Some(p) = url.port() {
        s.push_str(&format!(":{p}"));
    }
    s.push_str(url.path());
    s
}

#[allow(clippy::too_many_arguments)]
pub fn encode_frame(
    codec: u8,
    flags: u8,
    ssrc: u32,
    rtp_timestamp: u32,
    server_time_ms: u64,
    user_id: &UserId,
    payload: &[u8],
) -> Vec<u8> {
    let mut out = Vec::with_capacity(FRAME_HEADER_LEN + payload.len());
    out.push(FRAME_VERSION);
    out.push(codec);
    out.push(flags);
    out.push(0);
    out.extend_from_slice(&ssrc.to_be_bytes());
    out.extend_from_slice(&rtp_timestamp.to_be_bytes());
    out.extend_from_slice(&server_time_ms.to_be_bytes());
    out.extend_from_slice(user_id.0.as_bytes());
    out.extend_from_slice(payload);
    out
}

/// Parsed audio frame (consumer-side helper, also used by tests and the E2E receiver).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AudioFrame<'a> {
    pub codec: u8,
    pub flags: u8,
    pub ssrc: u32,
    pub rtp_timestamp: u32,
    pub server_time_ms: u64,
    pub user_id: UserId,
    pub payload: &'a [u8],
}

pub fn decode_frame(buf: &[u8]) -> Option<AudioFrame<'_>> {
    if buf.len() < FRAME_HEADER_LEN || buf[0] != FRAME_VERSION {
        return None;
    }
    let u32_at = |i: usize| u32::from_be_bytes([buf[i], buf[i + 1], buf[i + 2], buf[i + 3]]);
    let mut t = [0u8; 8];
    t.copy_from_slice(&buf[12..20]);
    Some(AudioFrame {
        codec: buf[1],
        flags: buf[2],
        ssrc: u32_at(4),
        rtp_timestamp: u32_at(8),
        server_time_ms: u64::from_be_bytes(t),
        user_id: UserId(Uuid::from_slice(&buf[20..36]).ok()?),
        payload: &buf[36..],
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(queue: usize) -> LiveStreamConfig {
        LiveStreamConfig {
            enabled: true,
            queue_frames: queue,
            ..LiveStreamConfig::default()
        }
    }

    fn opus_silence() -> Vec<u8> {
        // Minimal valid Opus packet: TOC for 20 ms CELT fullband mono, one frame, no data.
        vec![0xF8, 0xFF, 0xFE]
    }

    fn drain(rx: &mut mpsc::Receiver<Outgoing>) -> Vec<Outgoing> {
        let mut out = Vec::new();
        while let Ok(m) = rx.try_recv() {
            out.push(m);
        }
        out
    }

    fn audio_frames(items: &[Outgoing]) -> Vec<Vec<u8>> {
        items
            .iter()
            .filter_map(|o| match o {
                Outgoing::Audio(b) => Some(b.to_vec()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn frame_roundtrip() {
        let user = UserId(Uuid::new_v4());
        let f = encode_frame(
            CODEC_OPUS,
            FLAG_FIRST,
            7,
            960,
            1_700_000_000_123,
            &user,
            b"abc",
        );
        assert_eq!(f.len(), FRAME_HEADER_LEN + 3);
        let d = decode_frame(&f).unwrap();
        assert_eq!(d.codec, CODEC_OPUS);
        assert_eq!(d.flags, FLAG_FIRST);
        assert_eq!(d.ssrc, 7);
        assert_eq!(d.rtp_timestamp, 960);
        assert_eq!(d.server_time_ms, 1_700_000_000_123);
        assert_eq!(d.user_id, user);
        assert_eq!(d.payload, b"abc");
        assert!(decode_frame(&f[..10]).is_none());
    }

    #[test]
    fn consent_gates_frames_and_disclosure_order() {
        let live = LiveStreams::new(cfg(64), true, false, 0);
        let app = AppId(Uuid::new_v4());
        let ch = ChannelId(Uuid::new_v4());
        let alice = UserId(Uuid::new_v4());
        let (info, mut r) = live
            .open(app, ch, StreamSpec::default(), StreamMode::Pull, None)
            .unwrap();
        assert!(live.wants_channel(&ch));
        let first = drain(&mut r.rx);
        assert!(matches!(
            first.as_slice(),
            [Outgoing::Control(ControlFrame::Hello {
                consent_required: true,
                ..
            })]
        ));

        // Pending by default: nothing is emitted, but the user is now tracked.
        live.on_audio(ch, alice, 11, 960, &opus_silence());
        assert!(audio_frames(&drain(&mut r.rx)).is_empty());
        assert_eq!(
            live.get(app, info.id).unwrap().consent.get(&alice),
            Some(&RecordingConsent::Pending)
        );

        assert!(live
            .set_consent(app, info.id, alice, RecordingConsent::Accepted)
            .unwrap());
        live.on_audio(ch, alice, 11, 1920, &opus_silence());
        live.on_audio(ch, alice, 11, 2880, &opus_silence());
        let items = drain(&mut r.rx);
        assert!(matches!(
            items[0],
            Outgoing::Control(ControlFrame::Participant {
                event: ParticipantEvent::Consent,
                consent: Some(RecordingConsent::Accepted),
                ..
            })
        ));
        assert!(matches!(
            items[1],
            Outgoing::Control(ControlFrame::Participant {
                event: ParticipantEvent::AudioStarted,
                ssrc: 11,
                ..
            })
        ));
        let frames = audio_frames(&items);
        assert_eq!(frames.len(), 2);
        let f0 = decode_frame(&frames[0]).unwrap();
        assert_eq!(f0.flags & FLAG_FIRST, FLAG_FIRST);
        assert_eq!(f0.rtp_timestamp, 1920);
        assert_eq!(f0.payload, opus_silence().as_slice());
        let f1 = decode_frame(&frames[1]).unwrap();
        assert_eq!(f1.flags, 0);

        // A gap in the RTP clock is flagged.
        live.on_audio(ch, alice, 11, 2880 + 48_000, &opus_silence());
        let f = audio_frames(&drain(&mut r.rx));
        assert_eq!(decode_frame(&f[0]).unwrap().flags, FLAG_GAP);

        // Declining stops the frames immediately.
        live.set_consent(app, info.id, alice, RecordingConsent::Declined)
            .unwrap();
        live.on_audio(ch, alice, 11, 2880 + 96_000, &opus_silence());
        assert!(audio_frames(&drain(&mut r.rx)).is_empty());

        let closed = live.close(Some(app), info.id, "test").unwrap();
        assert_eq!(closed.frames_sent, 3);
        assert!(matches!(
            drain(&mut r.rx).last(),
            Some(Outgoing::Control(ControlFrame::End { .. }))
        ));
        assert!(!live.wants_channel(&ch));
    }

    #[test]
    fn no_consent_required_streams_immediately_and_filters_users() {
        let live = LiveStreams::new(cfg(64), false, false, 0);
        let app = AppId(Uuid::new_v4());
        let ch = ChannelId(Uuid::new_v4());
        let alice = UserId(Uuid::new_v4());
        let bob = UserId(Uuid::new_v4());
        let spec = StreamSpec {
            users: Some(vec![alice]),
            ..StreamSpec::default()
        };
        let (info, mut r) = live.open(app, ch, spec, StreamMode::Pull, None).unwrap();
        live.on_audio(ch, alice, 1, 0, &opus_silence());
        live.on_audio(ch, bob, 2, 0, &opus_silence());
        let frames = audio_frames(&drain(&mut r.rx));
        assert_eq!(frames.len(), 1);
        assert_eq!(decode_frame(&frames[0]).unwrap().user_id, alice);
        assert!(matches!(
            live.set_consent(app, info.id, bob, RecordingConsent::Accepted),
            Err(AurixError::AuthorizationDenied(_))
        ));
        live.on_participant_left(ch, alice);
        assert!(matches!(
            drain(&mut r.rx).as_slice(),
            [Outgoing::Control(ControlFrame::Participant {
                event: ParticipantEvent::Left,
                ..
            })]
        ));
    }

    #[test]
    fn tenant_isolation_and_limits() {
        let live = LiveStreams::new(
            LiveStreamConfig {
                max_per_channel: 1,
                max_per_app: 2,
                ..cfg(64)
            },
            false,
            false,
            0,
        );
        let app_a = AppId(Uuid::new_v4());
        let app_b = AppId(Uuid::new_v4());
        let ch = ChannelId(Uuid::new_v4());
        let (info, _r) = live
            .open(app_a, ch, StreamSpec::default(), StreamMode::Pull, None)
            .unwrap();
        assert!(matches!(
            live.open(app_a, ch, StreamSpec::default(), StreamMode::Pull, None),
            Err(AurixError::Conflict(_))
        ));
        assert!(live.get(app_b, info.id).is_none());
        assert!(live.list(app_b, None).is_empty());
        assert_eq!(live.list(app_a, Some(ch)).len(), 1);
        assert!(live.close(Some(app_b), info.id, "x").is_none());
        assert!(live.get(app_a, info.id).is_some());
        assert!(!live
            .set_consent(
                app_b,
                info.id,
                UserId(Uuid::new_v4()),
                RecordingConsent::Accepted
            )
            .unwrap());
        let (_i2, _r2) = live
            .open(
                app_a,
                ChannelId(Uuid::new_v4()),
                StreamSpec::default(),
                StreamMode::Pull,
                None,
            )
            .unwrap();
        assert!(matches!(
            live.open(
                app_a,
                ChannelId(Uuid::new_v4()),
                StreamSpec::default(),
                StreamMode::Pull,
                None
            ),
            Err(AurixError::Conflict(_))
        ));
        assert!(live.close(Some(app_a), info.id, "x").is_some());
        assert_eq!(live.close_all("shutdown"), 1);
    }

    #[test]
    fn slow_consumer_drops_and_announces() {
        let live = LiveStreams::new(cfg(16), false, false, 0);
        let app = AppId(Uuid::new_v4());
        let ch = ChannelId(Uuid::new_v4());
        let alice = UserId(Uuid::new_v4());
        let (info, mut r) = live
            .open(app, ch, StreamSpec::default(), StreamMode::Pull, None)
            .unwrap();
        for i in 0..40u32 {
            live.on_audio(ch, alice, 1, i * 960, &opus_silence());
        }
        let st = live.get(app, info.id).unwrap();
        // hello + audio_started + 14 frames fill the 16-slot queue.
        assert_eq!(st.frames_sent, 14);
        assert_eq!(st.frames_dropped, 26);
        drain(&mut r.rx);
        live.on_audio(ch, alice, 1, 40 * 960, &opus_silence());
        let items = drain(&mut r.rx);
        assert!(matches!(
            items[0],
            Outgoing::Control(ControlFrame::Dropped { frames: 26 })
        ));
        assert_eq!(audio_frames(&items).len(), 1);
    }

    #[test]
    fn pcm_format_decodes_to_s16le() {
        let live = LiveStreams::new(cfg(64), false, false, 0);
        let app = AppId(Uuid::new_v4());
        let ch = ChannelId(Uuid::new_v4());
        let alice = UserId(Uuid::new_v4());
        let mut enc =
            opus::Encoder::new(48_000, opus::Channels::Mono, opus::Application::Voip).unwrap();
        let pcm: Vec<i16> = (0..960)
            .map(|i| ((i as f32 * 0.05).sin() * 8000.0) as i16)
            .collect();
        let mut buf = vec![0u8; 4000];
        let n = enc.encode(&pcm, &mut buf).unwrap();
        let spec = StreamSpec {
            format: StreamFormat::PcmS16le,
            ..StreamSpec::default()
        };
        let (_info, mut r) = live.open(app, ch, spec, StreamMode::Pull, None).unwrap();
        live.on_audio(ch, alice, 1, 0, &buf[..n]);
        let frames = audio_frames(&drain(&mut r.rx));
        let f = decode_frame(&frames[0]).unwrap();
        assert_eq!(f.codec, CODEC_PCM_S16LE);
        assert_eq!(f.payload.len(), 960 * 2);
    }

    #[test]
    fn disabled_pcm_and_disabled_feature_are_rejected() {
        let live = LiveStreams::new(
            LiveStreamConfig {
                allow_pcm: false,
                ..cfg(64)
            },
            false,
            false,
            0,
        );
        let spec = StreamSpec {
            format: StreamFormat::PcmS16le,
            ..StreamSpec::default()
        };
        assert!(matches!(
            live.open(
                AppId(Uuid::new_v4()),
                ChannelId(Uuid::new_v4()),
                spec,
                StreamMode::Pull,
                None
            ),
            Err(AurixError::Validation(_))
        ));
        let off = LiveStreams::new(LiveStreamConfig::default(), false, false, 0);
        assert!(matches!(
            off.open(
                AppId(Uuid::new_v4()),
                ChannelId(Uuid::new_v4()),
                StreamSpec::default(),
                StreamMode::Pull,
                None
            ),
            Err(AurixError::InvalidConfiguration(_))
        ));
    }

    #[test]
    fn push_url_policy() {
        let prod = LiveStreams::new(cfg(64), false, true, 0);
        assert!(prod.validate_push_url("ws://example.com/in").is_err());
        assert!(prod.validate_push_url("wss://127.0.0.1/in").is_err());
        assert!(prod
            .validate_push_url("wss://user:pw@example.com/in")
            .is_err());
        assert!(prod.validate_push_url("https://example.com/in").is_err());
        assert!(prod
            .validate_push_url("wss://example.com/in?token=x")
            .is_ok());
        let dev = LiveStreams::new(cfg(64), false, false, 0);
        assert!(dev.validate_push_url("ws://127.0.0.1:9000/in").is_ok());
        let strict = LiveStreams::new(
            LiveStreamConfig {
                allow_private_urls: Some(false),
                ..cfg(64)
            },
            false,
            false,
            0,
        );
        assert!(strict.validate_push_url("ws://10.0.0.1/in").is_err());
        let off = LiveStreams::new(
            LiveStreamConfig {
                push_enabled: false,
                ..cfg(64)
            },
            false,
            false,
            0,
        );
        assert!(matches!(
            off.validate_push_url("wss://example.com/in"),
            Err(AurixError::InvalidConfiguration(_))
        ));
        assert_eq!(
            redact_url(&Url::parse("wss://h.example:8443/in?token=secret#f").unwrap()),
            "wss://h.example:8443/in"
        );
    }

    #[test]
    fn push_header_validation() {
        let ok = vec![("Authorization".to_string(), "Bearer x".to_string())];
        assert!(LiveStreams::validate_push_headers(&ok).is_ok());
        let bad_name = vec![("Bad Header".to_string(), "x".to_string())];
        assert!(LiveStreams::validate_push_headers(&bad_name).is_err());
        let managed = vec![("Host".to_string(), "evil".to_string())];
        assert!(LiveStreams::validate_push_headers(&managed).is_err());
        let many: Vec<_> = (0..9)
            .map(|i| (format!("x-h{i}"), "v".to_string()))
            .collect();
        assert!(LiveStreams::validate_push_headers(&many).is_err());
        let long = vec![("x-a".to_string(), "v".repeat(2000))];
        assert!(LiveStreams::validate_push_headers(&long).is_err());
    }

    #[test]
    fn duration_limit_closes_streams() {
        let live = LiveStreams::new(
            LiveStreamConfig {
                max_duration_secs: 1,
                ..cfg(64)
            },
            false,
            false,
            0,
        );
        let app = AppId(Uuid::new_v4());
        let (info, _r) = live
            .open(
                app,
                ChannelId(Uuid::new_v4()),
                StreamSpec::default(),
                StreamMode::Pull,
                None,
            )
            .unwrap();
        assert_eq!(live.enforce_duration_limit(), 0);
        live.reg.lock().by_id.get_mut(&info.id).unwrap().deadline =
            Some(Utc::now() - chrono::Duration::seconds(1));
        assert_eq!(live.enforce_duration_limit(), 1);
        assert!(live.get(app, info.id).is_none());
        let mut notices = live.take_notices().unwrap();
        assert!(matches!(notices.try_recv(), Ok(LiveNotice::Opened(_))));
        assert!(matches!(
            notices.try_recv(),
            Ok(LiveNotice::Closed { reason, .. }) if reason == "duration_limit"
        ));
    }

    /// Local ws receiver: accepts connections, records the text/binary frames of each one
    /// and hangs up after `drop_after` binary frames on the first connection.
    #[allow(clippy::result_large_err)]
    async fn ws_receiver(
        drop_after: usize,
    ) -> (
        u16,
        mpsc::UnboundedReceiver<(usize, Message)>,
        mpsc::UnboundedReceiver<Option<String>>,
    ) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let (frames_tx, frames_rx) = mpsc::unbounded_channel();
        let (auth_tx, auth_rx) = mpsc::unbounded_channel();
        tokio::spawn(async move {
            let mut conn = 0usize;
            loop {
                let (tcp, _) = listener.accept().await.unwrap();
                conn += 1;
                let frames_tx = frames_tx.clone();
                let auth_tx = auth_tx.clone();
                let this_conn = conn;
                tokio::spawn(async move {
                    let mut auth = None;
                    let ws = tokio_tungstenite::accept_hdr_async(
                        tcp,
                        |req: &tokio_tungstenite::tungstenite::handshake::server::Request, resp| {
                            auth = req
                                .headers()
                                .get("authorization")
                                .and_then(|v| v.to_str().ok())
                                .map(str::to_string);
                            Ok(resp)
                        },
                    )
                    .await
                    .unwrap();
                    let _ = auth_tx.send(auth);
                    let (_sink, mut source) = ws.split();
                    let mut binary = 0usize;
                    while let Some(Ok(msg)) = source.next().await {
                        if msg.is_binary() {
                            binary += 1;
                        }
                        let _ = frames_tx.send((this_conn, msg));
                        if this_conn == 1 && drop_after > 0 && binary >= drop_after {
                            return; // hang up without a Close frame
                        }
                    }
                });
            }
        });
        (port, frames_rx, auth_rx)
    }

    #[tokio::test]
    async fn push_connects_sends_headers_and_reconnects_with_fresh_hello() {
        let live = Arc::new(LiveStreams::new(
            LiveStreamConfig {
                allow_private_urls: Some(true),
                require_tls: Some(false),
                connect_timeout_ms: 2_000,
                ..cfg(64)
            },
            false,
            false,
            0,
        ));
        let (port, mut frames, mut auth) = ws_receiver(2).await;
        let raw = format!("ws://127.0.0.1:{port}/ingest?x=1");
        let url = live.validate_push_url(&raw).unwrap();
        let app = AppId(Uuid::new_v4());
        let ch = ChannelId(Uuid::new_v4());
        let alice = UserId(Uuid::new_v4());
        let (info, receiver) = live
            .open(app, ch, StreamSpec::default(), StreamMode::Push, Some(&url))
            .unwrap();
        assert_eq!(info.state, StreamState::Connecting);
        assert_eq!(
            info.push_url.as_deref(),
            Some(format!("ws://127.0.0.1:{port}/ingest").as_str())
        );
        live.spawn_push(
            receiver,
            url,
            PushTarget {
                url: raw,
                headers: vec![("Authorization".into(), "Bearer s3cret".into())],
            },
        );

        assert_eq!(
            tokio::time::timeout(Duration::from_secs(5), auth.recv())
                .await
                .unwrap()
                .unwrap()
                .as_deref(),
            Some("Bearer s3cret")
        );
        let (c, hello) = tokio::time::timeout(Duration::from_secs(5), frames.recv())
            .await
            .expect("hello in time")
            .expect("receiver alive");
        assert_eq!(c, 1);
        let hello: ControlFrame = serde_json::from_str(hello.to_text().unwrap()).unwrap();
        assert!(matches!(hello, ControlFrame::Hello { stream_id, .. } if stream_id == info.id));

        // Frames flow; the receiver hangs up after two binary frames.
        let mut ts = 0u32;
        let mut got_binary = 0;
        while got_binary < 2 {
            live.on_audio(ch, alice, 9, ts, &opus_silence());
            ts += FRAME_SAMPLES;
            tokio::time::sleep(Duration::from_millis(20)).await;
            while let Ok((c, m)) = frames.try_recv() {
                assert_eq!(c, 1);
                if m.is_binary() {
                    got_binary += 1;
                    let data = m.into_data();
                    assert_eq!(decode_frame(&data).unwrap().user_id, alice);
                }
            }
        }

        // Reconnect (1 s backoff): the new connection starts with a fresh hello, the status
        // reports the reconnect, and audio continues.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        let mut second_hello = None;
        while second_hello.is_none() && tokio::time::Instant::now() < deadline {
            live.on_audio(ch, alice, 9, ts, &opus_silence());
            ts += FRAME_SAMPLES;
            tokio::time::sleep(Duration::from_millis(20)).await;
            while let Ok((c, m)) = frames.try_recv() {
                if c == 2 && m.is_text() {
                    second_hello = Some(m);
                }
            }
        }
        let hello: ControlFrame =
            serde_json::from_str(second_hello.expect("reconnected").to_text().unwrap()).unwrap();
        assert!(matches!(hello, ControlFrame::Hello { .. }));
        let status = live.get(app, info.id).unwrap();
        assert_eq!(status.state, StreamState::Streaming);
        assert_eq!(status.reconnects, 1);

        // Closing delivers the End frame over the second connection.
        live.close(Some(app), info.id, "operator").unwrap();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        let mut ended = false;
        while !ended && tokio::time::Instant::now() < deadline {
            if let Ok(Some((2, m))) =
                tokio::time::timeout(Duration::from_millis(200), frames.recv()).await
            {
                if let Ok(ControlFrame::End { reason, .. }) =
                    serde_json::from_slice::<ControlFrame>(&m.into_data())
                {
                    assert_eq!(reason, "operator");
                    ended = true;
                }
            }
        }
        assert!(ended);
    }

    #[tokio::test]
    async fn push_gives_up_when_target_is_unreachable() {
        let live = Arc::new(LiveStreams::new(
            LiveStreamConfig {
                allow_private_urls: Some(true),
                require_tls: Some(false),
                connect_timeout_ms: 200,
                max_reconnects: 0,
                ..cfg(64)
            },
            false,
            false,
            0,
        ));
        // A bound-but-not-listening port refuses immediately.
        let sock = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = sock.local_addr().unwrap().port();
        drop(sock);
        let raw = format!("ws://127.0.0.1:{port}/");
        let url = live.validate_push_url(&raw).unwrap();
        let app = AppId(Uuid::new_v4());
        let (info, receiver) = live
            .open(
                app,
                ChannelId(Uuid::new_v4()),
                StreamSpec::default(),
                StreamMode::Push,
                Some(&url),
            )
            .unwrap();
        let mut notices = live.take_notices().unwrap();
        live.spawn_push(
            receiver,
            url,
            PushTarget {
                url: raw,
                headers: vec![],
            },
        );
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while live.get(app, info.id).is_some() && tokio::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert!(live.get(app, info.id).is_none());
        assert!(matches!(notices.try_recv(), Ok(LiveNotice::Opened(_))));
        assert!(matches!(
            notices.try_recv(),
            Ok(LiveNotice::Closed { reason, .. }) if reason == "push_unreachable"
        ));
    }
}
