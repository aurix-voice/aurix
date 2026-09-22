//! Real-time audio streaming of a channel to an operator-controlled service.
//!
//! A *live stream* is a tap on the authenticated, non-E2EE Opus packets of one channel,
//! owned by the node that accepted the request. Frames are either pulled by the operator
//! over a WebSocket on that node (`GET /v1/channels/:id/audio/streams/pull`) or pushed by the
//! node to an operator WebSocket endpoint (`POST /v1/channels/:id/audio/streams`). Nothing is
//! written to disk; the same consent rules as for stored recordings apply, and a
//! participant's frames are never emitted before that participant accepted.
//!
//! The owning node need not host any participant of the channel: the cascade forwards the
//! channel's audio to every node with a live stream on it (the stream is registered in the
//! `live_streams` table, which the topology planner treats like a hosting node), so a stream
//! can be opened through any node of the fleet.
//!
//! Two shapes of stream:
//!
//! * per participant (default): every Opus packet is forwarded (or decoded to PCM) with the
//!   sender's user id and SSRC;
//! * mixed (`mix = true`): the node decodes the consenting participants, sums them on a
//!   20 ms clock through Opus' soft clipper and emits **one** mono track (re-encoded Opus or
//!   PCM) with a nil user id and SSRC 0 — the channel as a listener would hear it, minus
//!   E2EE audio (which the server cannot decode) and non-consenting participants.
//!
//! Consumer outages: while a push target is unreachable the most recent
//! `recording.live.outage_buffer_ms` of frames are held and replayed after the reconnect; a
//! pull consumer that drops off keeps its stream alive for the same window and may resume it
//! (`…/streams/:id/pull`), receiving the buffered frames first. Everything beyond the window
//! is dropped and accounted like a slow consumer.
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
//!  4..8  u32  SSRC of the participant on this node (0 for a mixed stream)
//!  8..12 u32  RTP timestamp (48 kHz clock)
//! 12..20 u64  server receive time, Unix milliseconds
//! 20..36 [16] participant user id (RFC 4122 byte order; nil for a mixed stream)
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
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{mpsc, oneshot};
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
/// Mixed streams: frames a talker may queue ahead of the mix clock (120 ms of jitter).
const MIX_QUEUE_FRAMES: usize = 6;
/// Mixed streams: consecutive missing frames of a talker concealed by the decoder.
const MIX_PLC_FRAMES: u64 = 3;
/// Mixed streams: silence frames emitted after the last talker before the track pauses.
const MIX_HANGOVER_TICKS: u64 = 25;
/// Mixed streams: an idle talker's decoder is reclaimed after this many ticks (30 s).
const MIX_TALKER_IDLE_TICKS: u64 = 1500;
/// Mixed streams: time between two mix frames.
pub const MIX_TICK: Duration = Duration::from_millis(20);

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
    /// Push: the target is being re-dialled; pull: the consumer dropped off and the stream
    /// waits for it to resume. Frames are buffered up to `outage_buffer_ms`.
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
    /// One server-side mixed track instead of per-participant frames.
    pub mix: bool,
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
        /// Frames carry one mixed track (nil user id, SSRC 0) instead of per-participant audio.
        #[serde(default)]
        mix: bool,
        /// Node that owns the stream.
        #[serde(default)]
        node_id: Option<Uuid>,
        /// 0 on the first connection of a consumer; incremented for every push reconnect
        /// or pull resume, so a consumer can tell a replayed `hello` from a new stream.
        #[serde(default)]
        reconnects: u32,
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
/// URL is reduced to scheme/host/path. For a stream owned by another node this is the
/// status that node last published to the fleet directory.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LiveStreamInfo {
    pub id: Uuid,
    pub app_id: Uuid,
    pub channel_id: ChannelId,
    /// Node that owns the stream (its media tap and consumer connection).
    pub node_id: Uuid,
    pub mode: StreamMode,
    pub format: StreamFormat,
    pub mix: bool,
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

struct MixTalker {
    decoder: opus::Decoder,
    /// Decoded 20 ms frames waiting for the mix clock (oldest first).
    queue: VecDeque<Vec<i16>>,
    fed_tick: u64,
    concealed: u64,
}

/// Server-side mix of one stream: consenting talkers are decoded as their packets arrive and
/// summed on a 20 ms clock ([`LiveStreams::mix_tick`]). The RTP clock advances every tick, so
/// the pause of a silent channel shows up as a timestamp gap (and the `gap` flag) on the next
/// frame rather than as a stream of silence.
struct Mixer {
    format: StreamFormat,
    encoder: Option<opus::Encoder>,
    clip: opus::SoftClip,
    talkers: HashMap<UserId, MixTalker>,
    tick: u64,
    rtp_timestamp: u32,
    last_active_tick: Option<u64>,
    started: bool,
    gap: bool,
    acc: Vec<f32>,
    pcm: Vec<i16>,
    packet: Vec<u8>,
    /// Talker frames discarded because they ran ahead of the mix clock (stats only).
    overruns: u64,
}

impl Mixer {
    fn new(format: StreamFormat, bitrate: u32) -> Result<Self> {
        let encoder = match format {
            StreamFormat::PcmS16le => None,
            StreamFormat::Opus => {
                let mut enc =
                    opus::Encoder::new(SAMPLE_RATE, opus::Channels::Mono, opus::Application::Voip)
                        .map_err(|e| AurixError::Recording(format!("mix encoder: {e}")))?;
                enc.set_bitrate(opus::Bitrate::Bits(bitrate as i32))
                    .map_err(|e| AurixError::Recording(format!("mix encoder bitrate: {e}")))?;
                let _ = enc.set_inband_fec(true);
                let _ = enc.set_packet_loss_perc(5);
                Some(enc)
            }
        };
        Ok(Self {
            format,
            encoder,
            clip: opus::SoftClip::new(opus::Channels::Mono),
            talkers: HashMap::new(),
            tick: 0,
            rtp_timestamp: rand::random(),
            last_active_tick: None,
            started: false,
            gap: false,
            acc: vec![0.0; FRAME_SAMPLES as usize],
            pcm: vec![0; MAX_PCM_SAMPLES],
            packet: vec![0; 1500],
            overruns: 0,
        })
    }

    /// Decode one packet of `user` into their queue. Returns `false` when the packet could
    /// not be decoded (the talker keeps its state).
    fn feed(&mut self, user: UserId, payload: &[u8]) -> bool {
        let tick = self.tick;
        let talker = match self.talkers.entry(user) {
            std::collections::hash_map::Entry::Occupied(e) => e.into_mut(),
            std::collections::hash_map::Entry::Vacant(v) => {
                let Ok(decoder) = opus::Decoder::new(SAMPLE_RATE, opus::Channels::Mono) else {
                    return false;
                };
                v.insert(MixTalker {
                    decoder,
                    queue: VecDeque::with_capacity(MIX_QUEUE_FRAMES),
                    fed_tick: tick,
                    concealed: 0,
                })
            }
        };
        let n = match talker.decoder.decode(payload, &mut self.pcm, false) {
            Ok(n) => n,
            Err(_) => return false,
        };
        talker.fed_tick = tick;
        talker.concealed = 0;
        // Packets may carry 10/40/60 ms; the mix clock works in 20 ms frames.
        let frame = FRAME_SAMPLES as usize;
        let mut start = 0;
        while start < n {
            let end = (start + frame).min(n);
            let mut chunk = self.pcm[start..end].to_vec();
            chunk.resize(frame, 0);
            if talker.queue.len() >= MIX_QUEUE_FRAMES {
                talker.queue.pop_front();
                self.overruns += 1;
            }
            talker.queue.push_back(chunk);
            start = end;
        }
        true
    }

    fn remove(&mut self, user: &UserId) {
        self.talkers.remove(user);
    }

    /// Advance the clock by one frame and produce the mixed frame, or `None` while the
    /// channel is silent (after the hangover).
    fn tick(&mut self, now_ms: u64) -> Option<Vec<u8>> {
        self.tick += 1;
        let tick = self.tick;
        let ts = self.rtp_timestamp;
        self.rtp_timestamp = ts.wrapping_add(FRAME_SAMPLES);
        self.acc.fill(0.0);
        let mut any = false;
        let frame = FRAME_SAMPLES as usize;
        let pcm = &mut self.pcm[..frame];
        self.talkers
            .retain(|_, t| tick.saturating_sub(t.fed_tick) <= MIX_TALKER_IDLE_TICKS);
        for talker in self.talkers.values_mut() {
            let contributed = match talker.queue.pop_front() {
                Some(chunk) => {
                    pcm.copy_from_slice(&chunk);
                    true
                }
                None => {
                    let recent = tick.saturating_sub(talker.fed_tick) <= MIX_PLC_FRAMES;
                    if recent
                        && talker.concealed < MIX_PLC_FRAMES
                        && talker.decoder.decode(&[], pcm, false).is_ok()
                    {
                        talker.concealed += 1;
                        true
                    } else {
                        false
                    }
                }
            };
            if contributed {
                any = true;
                for (a, s) in self.acc.iter_mut().zip(pcm.iter()) {
                    *a += f32::from(*s) / 32_768.0;
                }
            }
        }
        if any {
            self.last_active_tick = Some(tick);
        }
        let active = self
            .last_active_tick
            .is_some_and(|t| tick - t <= MIX_HANGOVER_TICKS);
        if !active {
            self.gap = self.started;
            return None;
        }
        self.clip.apply(&mut self.acc);
        for (o, a) in pcm.iter_mut().zip(self.acc.iter()) {
            *o = (a * 32_767.0).round().clamp(-32_768.0, 32_767.0) as i16;
        }
        let body: &[u8] = match self.encoder.as_mut() {
            None => {
                self.packet.clear();
                self.packet.extend(pcm.iter().flat_map(|s| s.to_le_bytes()));
                &self.packet
            }
            Some(enc) => {
                self.packet.resize(1500, 0);
                match enc.encode(pcm, &mut self.packet) {
                    Ok(n) => &self.packet[..n],
                    Err(e) => {
                        warn!("Live mix: opus encode failed: {e}");
                        return None;
                    }
                }
            }
        };
        let mut flags = 0u8;
        if !self.started {
            flags |= FLAG_FIRST;
        }
        if self.gap {
            flags |= FLAG_GAP;
        }
        self.started = true;
        self.gap = false;
        Some(encode_frame(
            self.format.codec_byte(),
            flags,
            0,
            ts,
            now_ms,
            &UserId(Uuid::nil()),
            body,
        ))
    }
}

/// A pull stream whose consumer dropped off: a task holds the receiver, keeps the most recent
/// frames and hands both back on resume (or closes the stream when the window expires).
struct Parked {
    stop: oneshot::Sender<()>,
    handle: tokio::task::JoinHandle<Option<(mpsc::Receiver<Outgoing>, Vec<Outgoing>)>>,
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
    mixer: Option<Arc<Mutex<Mixer>>>,
    parked: Option<Parked>,
    frames_sent: u64,
    frames_dropped: u64,
    /// Drops not yet announced to the consumer with a `dropped` frame.
    unannounced_drops: u64,
    reconnects: u32,
}

impl Tap {
    fn info(&self, node_id: Uuid) -> LiveStreamInfo {
        LiveStreamInfo {
            id: self.id,
            app_id: self.app_id,
            channel_id: self.channel_id,
            node_id,
            mode: self.mode,
            format: self.format,
            mix: self.mixer.is_some(),
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

    fn hello(&self, node_id: Uuid, consent_required: bool) -> ControlFrame {
        ControlFrame::Hello {
            stream_id: self.id,
            app_id: self.app_id,
            channel_id: self.channel_id,
            format: self.format,
            sample_rate: SAMPLE_RATE,
            channels: 1,
            frame_ms: 20,
            frame_version: FRAME_VERSION,
            users: self.users_order.clone(),
            consent_required,
            label: self.label.clone(),
            started_at: self.started_at,
            mix: self.mixer.is_some(),
            node_id: Some(node_id),
            reconnects: self.reconnects,
        }
    }

    /// Announce drops the consumer has not heard about yet, leaving room for the frame
    /// that follows.
    fn announce_drops(&mut self) {
        if self.unannounced_drops > 0 && self.tx.capacity() > 1 {
            let frames = self.unannounced_drops;
            if self.control(ControlFrame::Dropped { frames }) {
                self.unannounced_drops = 0;
            }
        }
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
    node_id: Uuid,
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
        node_id: Uuid,
    ) -> Self {
        let (notices, notice_rx) = mpsc::unbounded_channel();
        Self {
            cfg,
            require_consent,
            production,
            max_recording_duration_secs,
            node_id,
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

    pub fn node_id(&self) -> Uuid {
        self.node_id
    }

    /// Whether disconnected consumers are waited for (`recording.live.outage_buffer_ms > 0`).
    pub fn outage_buffering(&self) -> bool {
        self.cfg.outage_buffer_frames() > 0
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
        if spec.mix && self.cfg.max_mix_streams == 0 {
            return Err(AurixError::Validation(
                "Mixed streams are disabled (recording.live.max_mix_streams)".into(),
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
        let mixer = spec
            .mix
            .then(|| Mixer::new(spec.format, self.cfg.mix_bitrate))
            .transpose()?
            .map(|m| Arc::new(Mutex::new(m)));
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
            mixer,
            parked: None,
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
            if spec.mix {
                let mixing = reg.by_id.values().filter(|t| t.mixer.is_some()).count();
                if mixing >= self.cfg.max_mix_streams as usize {
                    return Err(AurixError::Conflict(format!(
                        "Node already runs {} mixed live streams (recording.live.max_mix_streams)",
                        mixing
                    )));
                }
            }
            let hello = tap.hello(self.node_id, self.require_consent);
            tap.control(hello);
            let info = tap.info(self.node_id);
            reg.by_channel.entry(channel_id).or_default().push(id);
            reg.by_id.insert(id, tap);
            info
        };
        info!(
            "Live stream {} opened: {:?}/{:?}{} for channel {} (app {})",
            id,
            mode,
            spec.format,
            if spec.mix { " mixed" } else { "" },
            channel_id,
            app_id.0
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
        if let Some(parked) = tap.parked.take() {
            // The parked drain task ends on its own once `tx` is dropped below; the stop
            // signal only shortens the wait.
            let _ = parked.stop.send(());
            parked.handle.abort();
        }
        let info = tap.info(self.node_id);
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
            .map(|t| t.info(self.node_id))
    }

    pub fn list(&self, app_id: AppId, channel_id: Option<ChannelId>) -> Vec<LiveStreamInfo> {
        let reg = self.reg.lock();
        let mut v: Vec<LiveStreamInfo> = reg
            .by_id
            .values()
            .filter(|t| t.app_id == app_id.0 && channel_id.is_none_or(|c| c == t.channel_id))
            .map(|t| t.info(self.node_id))
            .collect();
        v.sort_by_key(|i| i.started_at);
        v
    }

    /// Status of every stream on this node (all tenants; for the fleet directory).
    pub fn list_all(&self) -> Vec<LiveStreamInfo> {
        let reg = self.reg.lock();
        reg.by_id.values().map(|t| t.info(self.node_id)).collect()
    }

    /// Channels with at least one stream on this node.
    pub fn channels(&self) -> Vec<ChannelId> {
        self.reg.lock().by_channel.keys().copied().collect()
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
            if let Some(m) = &tap.mixer {
                m.lock().remove(&user_id);
            }
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
        let mut mix_jobs: Vec<Arc<Mutex<Mixer>>> = Vec::new();
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
                    let decoder = match (tap.format, tap.mixer.is_some()) {
                        (StreamFormat::Opus, _) | (_, true) => None,
                        (StreamFormat::PcmS16le, false) => {
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
            if let Some(mixer) = &tap.mixer {
                mix_jobs.push(mixer.clone());
                continue;
            }
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
            tap.announce_drops();
            tap.enqueue(Outgoing::Audio(Bytes::from(frame)));
        }
        drop(reg);
        // Decoding for mixed streams happens outside the registry lock so it never stalls
        // the other streams' hot path.
        for mixer in mix_jobs {
            mixer.lock().feed(user_id, payload);
        }
    }

    /// Advance every mixed stream by one 20 ms frame. Driven by [`Self::run_mixer`] (or
    /// directly by tests).
    pub fn mix_tick(&self) {
        let mixers: Vec<(Uuid, Arc<Mutex<Mixer>>)> = self
            .reg
            .lock()
            .by_id
            .values()
            .filter_map(|t| t.mixer.as_ref().map(|m| (t.id, m.clone())))
            .collect();
        if mixers.is_empty() {
            return;
        }
        let now_ms = Utc::now().timestamp_millis().max(0) as u64;
        for (id, mixer) in mixers {
            let Some(frame) = mixer.lock().tick(now_ms) else {
                continue;
            };
            let mut reg = self.reg.lock();
            if let Some(tap) = reg.by_id.get_mut(&id) {
                tap.announce_drops();
                tap.enqueue(Outgoing::Audio(Bytes::from(frame)));
            }
        }
    }

    /// The mix clock: one [`Self::mix_tick`] every 20 ms until `cancel` fires.
    pub async fn run_mixer(self: Arc<Self>, cancel: tokio_util::sync::CancellationToken) {
        let mut interval = tokio::time::interval(MIX_TICK);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Burst);
        loop {
            tokio::select! {
                _ = interval.tick() => {}
                _ = cancel.cancelled() => return,
            }
            self.mix_tick();
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
            if let Some(m) = &tap.mixer {
                m.lock().remove(&user_id);
            }
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
            if let Some(m) = &tap.mixer {
                m.lock().remove(&user_id);
            }
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

    /// Frames lost while the consumer was away (outage buffer overflow). `announced` frames
    /// were already reported to the consumer with a `dropped` frame.
    fn note_outage_drops(&self, id: Uuid, frames: u64, announced: bool) {
        if frames == 0 {
            return;
        }
        if let Some(tap) = self.reg.lock().by_id.get_mut(&id) {
            tap.frames_dropped += frames;
            tap.frames_sent = tap.frames_sent.saturating_sub(frames);
            if !announced {
                tap.unannounced_drops += frames;
            }
        }
    }

    fn hello_for(&self, id: Uuid) -> Option<ControlFrame> {
        self.reg
            .lock()
            .by_id
            .get(&id)
            .map(|t| t.hello(self.node_id, self.require_consent))
    }

    /// The pull consumer of `receiver`'s stream went away. With outage buffering the stream
    /// stays registered as `reconnecting`, its most recent frames are kept and the receiver
    /// is parked until [`Self::resume_pull`] or the window expires (`consumer_timeout`);
    /// without it the stream is closed with `reason` (and its final status returned).
    pub fn detach_pull(
        self: &Arc<Self>,
        receiver: StreamReceiver,
        reason: &str,
    ) -> Option<LiveStreamInfo> {
        let id = receiver.id;
        let capacity = self.cfg.outage_buffer_frames();
        if capacity == 0 {
            return self.close(None, id, reason);
        }
        let (stop_tx, mut stop_rx) = oneshot::channel();
        let window = Duration::from_millis(self.cfg.outage_buffer_ms);
        let this = self.clone();
        let mut rx = receiver.rx;
        let handle = tokio::spawn(async move {
            let mut backlog = Backlog::new(capacity);
            let deadline = tokio::time::sleep(window);
            tokio::pin!(deadline);
            loop {
                tokio::select! {
                    item = rx.recv() => match item {
                        None => return None,
                        Some(out) => backlog.push(out),
                    },
                    _ = &mut stop_rx => {
                        this.note_outage_drops(id, backlog.dropped, false);
                        return Some((rx, backlog.into_items()));
                    }
                    _ = &mut deadline => {
                        this.close(None, id, "consumer_timeout");
                        return None;
                    }
                }
            }
        });
        let mut reg = self.reg.lock();
        match reg.by_id.get_mut(&id) {
            Some(tap) if tap.mode == StreamMode::Pull => {
                tap.state = StreamState::Reconnecting;
                tap.parked = Some(Parked {
                    stop: stop_tx,
                    handle,
                });
                info!(
                    "Live stream {} consumer {reason}; holding {} ms for a resume",
                    id, self.cfg.outage_buffer_ms
                );
            }
            _ => handle.abort(),
        }
        None
    }

    /// Reattach a consumer to a parked pull stream of `app_id`. Returns the receiver plus the
    /// frames to send first: a fresh `hello`, the control frames and the audio buffered while
    /// the consumer was away. Refused while the previous consumer is still connected.
    pub async fn resume_pull(
        &self,
        app_id: AppId,
        id: Uuid,
    ) -> Result<(LiveStreamInfo, StreamReceiver, Vec<Outgoing>)> {
        let parked = {
            let mut reg = self.reg.lock();
            let tap = reg
                .by_id
                .get_mut(&id)
                .filter(|t| t.app_id == app_id.0)
                .ok_or_else(|| AurixError::NotFound("Live stream not found".into()))?;
            if tap.mode != StreamMode::Pull {
                return Err(AurixError::Conflict(
                    "Only pull streams can be resumed".into(),
                ));
            }
            tap.parked.take().ok_or_else(|| {
                AurixError::Conflict("The stream's consumer is still connected".into())
            })?
        };
        let _ = parked.stop.send(());
        let Some((rx, backlog)) = parked.handle.await.ok().flatten() else {
            return Err(AurixError::NotFound("Live stream not found".into()));
        };
        let mut reg = self.reg.lock();
        let tap = reg
            .by_id
            .get_mut(&id)
            .ok_or_else(|| AurixError::NotFound("Live stream not found".into()))?;
        tap.state = StreamState::Streaming;
        tap.reconnects += 1;
        let mut replay = Vec::with_capacity(backlog.len() + 2);
        replay.push(Outgoing::Control(
            tap.hello(self.node_id, self.require_consent),
        ));
        if tap.unannounced_drops > 0 {
            replay.push(Outgoing::Control(ControlFrame::Dropped {
                frames: tap.unannounced_drops,
            }));
            tap.unannounced_drops = 0;
        }
        replay.extend(backlog);
        info!(
            "Live stream {} resumed by its consumer ({} buffered frames)",
            id,
            replay.len() - 1
        );
        Ok((tap.info(self.node_id), StreamReceiver { id, rx }, replay))
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
        // Frames produced while the target is down are kept here (most recent
        // `outage_buffer_ms`) and replayed after the reconnect.
        let mut backlog = Backlog::new(self.cfg.outage_buffer_frames());
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
                    if !backlog.fill_for(backoff(attempt), &mut rx).await {
                        return;
                    }
                    continue;
                }
            };
            self.set_state(id, StreamState::Streaming, attempt > 0);
            let (mut sink, mut source) = ws.split();
            // Every (re)connected consumer starts with a `hello` describing the stream as it
            // is now (the queued one from `open` is skipped below), then a `dropped` frame for
            // what the outage buffer could not hold, then the buffered frames.
            let Some(hello) = self.hello_for(id) else {
                return;
            };
            let mut opening = vec![Outgoing::Control(hello)];
            let lost = backlog.dropped;
            if lost > 0 {
                opening.push(Outgoing::Control(ControlFrame::Dropped { frames: lost }));
            }
            opening.extend(backlog.take_items());
            self.note_outage_drops(id, lost, true);
            let mut replay_failed = None;
            for out in opening {
                if let Err(e) = sink.send(out.into_ws_message()).await {
                    replay_failed = Some(e.to_string());
                    break;
                }
            }
            if let Some(reason) = replay_failed {
                warn!("Live stream {} push replay failed: {reason}", id);
                self.set_state(id, StreamState::Reconnecting, false);
                attempt = 1;
                if !backlog.fill_for(backoff(attempt), &mut rx).await {
                    return;
                }
                continue;
            }
            let outcome = loop {
                tokio::select! {
                    item = rx.recv() => match item {
                        None => break PushOutcome::Finished,
                        Some(out) => {
                            if matches!(out, Outgoing::Control(ControlFrame::Hello { .. })) {
                                continue;
                            }
                            if let Err(e) = sink.send(out.into_ws_message()).await {
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
                    if !backlog.fill_for(backoff(attempt), &mut rx).await {
                        return;
                    }
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

/// Frames kept for a consumer that is away, in their original order: the most recent
/// `capacity` audio frames plus the control frames of the period (bounded separately; they are
/// few and cheap). Audio beyond the capacity is dropped oldest-first and counted.
struct Backlog {
    capacity: usize,
    items: VecDeque<Outgoing>,
    audio: usize,
    control: usize,
    dropped: u64,
}

const BACKLOG_CONTROL_FRAMES: usize = 256;

impl Backlog {
    fn new(capacity: usize) -> Self {
        Self {
            capacity,
            items: VecDeque::with_capacity(capacity.min(4096)),
            audio: 0,
            control: 0,
            dropped: 0,
        }
    }

    fn push(&mut self, out: Outgoing) {
        match out {
            // The stream's own `hello`/`end` are produced by the consumer side; `dropped`
            // counts are folded into ours.
            Outgoing::Control(ControlFrame::Hello { .. }) => {}
            Outgoing::Control(ControlFrame::End { .. }) => {}
            Outgoing::Control(ControlFrame::Dropped { frames }) => self.dropped += frames,
            Outgoing::Control(c) => {
                if self.control >= BACKLOG_CONTROL_FRAMES {
                    self.evict(|o| matches!(o, Outgoing::Control(_)));
                    self.control -= 1;
                }
                self.control += 1;
                self.items.push_back(Outgoing::Control(c));
            }
            Outgoing::Audio(b) => {
                if self.capacity == 0 {
                    self.dropped += 1;
                    return;
                }
                if self.audio >= self.capacity {
                    self.evict(|o| matches!(o, Outgoing::Audio(_)));
                    self.audio -= 1;
                    self.dropped += 1;
                }
                self.audio += 1;
                self.items.push_back(Outgoing::Audio(b));
            }
        }
    }

    /// Remove the oldest item matching `kind` (control frames are rare, so the scan for the
    /// oldest audio frame stops almost immediately).
    fn evict(&mut self, kind: impl Fn(&Outgoing) -> bool) {
        if let Some(pos) = self.items.iter().position(kind) {
            self.items.remove(pos);
        }
    }

    /// Buffered frames in send order and a reset buffer.
    fn take_items(&mut self) -> Vec<Outgoing> {
        let items = self.drain_items();
        self.dropped = 0;
        items
    }

    fn into_items(mut self) -> Vec<Outgoing> {
        self.drain_items()
    }

    fn drain_items(&mut self) -> Vec<Outgoing> {
        self.audio = 0;
        self.control = 0;
        self.items.drain(..).collect()
    }

    /// Buffer everything the stream produces for `wait` (a reconnect backoff). Returns
    /// `false` when the stream was closed meanwhile (the sender side is gone).
    async fn fill_for(&mut self, wait: Duration, rx: &mut mpsc::Receiver<Outgoing>) -> bool {
        let deadline = tokio::time::sleep(wait);
        tokio::pin!(deadline);
        loop {
            tokio::select! {
                item = rx.recv() => match item {
                    None => return false,
                    Some(out) => self.push(out),
                },
                _ = &mut deadline => return true,
            }
        }
    }
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
        let live = LiveStreams::new(cfg(64), true, false, 0, Uuid::nil());
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
        let live = LiveStreams::new(cfg(64), false, false, 0, Uuid::nil());
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
            Uuid::nil(),
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
        let live = LiveStreams::new(cfg(16), false, false, 0, Uuid::nil());
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
        let live = LiveStreams::new(cfg(64), false, false, 0, Uuid::nil());
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
            Uuid::nil(),
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
        let off = LiveStreams::new(LiveStreamConfig::default(), false, false, 0, Uuid::nil());
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
        let prod = LiveStreams::new(cfg(64), false, true, 0, Uuid::nil());
        assert!(prod.validate_push_url("ws://example.com/in").is_err());
        assert!(prod.validate_push_url("wss://127.0.0.1/in").is_err());
        assert!(prod
            .validate_push_url("wss://user:pw@example.com/in")
            .is_err());
        assert!(prod.validate_push_url("https://example.com/in").is_err());
        assert!(prod
            .validate_push_url("wss://example.com/in?token=x")
            .is_ok());
        let dev = LiveStreams::new(cfg(64), false, false, 0, Uuid::nil());
        assert!(dev.validate_push_url("ws://127.0.0.1:9000/in").is_ok());
        let strict = LiveStreams::new(
            LiveStreamConfig {
                allow_private_urls: Some(false),
                ..cfg(64)
            },
            false,
            false,
            0,
            Uuid::nil(),
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
            Uuid::nil(),
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
            Uuid::nil(),
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
            Uuid::nil(),
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
            Uuid::nil(),
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

    fn opus_tone(enc: &mut opus::Encoder, amplitude: f32, phase: &mut f32) -> Vec<u8> {
        let mut pcm = vec![0i16; FRAME_SAMPLES as usize];
        for s in pcm.iter_mut() {
            *s = (amplitude * (*phase).sin() * 32_767.0) as i16;
            *phase += 2.0 * std::f32::consts::PI * 440.0 / SAMPLE_RATE as f32;
        }
        let mut out = vec![0u8; 1500];
        let n = enc.encode(&pcm, &mut out).unwrap();
        out.truncate(n);
        out
    }

    fn tone_encoder() -> opus::Encoder {
        let mut enc =
            opus::Encoder::new(SAMPLE_RATE, opus::Channels::Mono, opus::Application::Voip).unwrap();
        enc.set_bitrate(opus::Bitrate::Bits(64_000)).unwrap();
        enc
    }

    fn pcm_peak(frame: &[u8]) -> i16 {
        let d = decode_frame(frame).unwrap();
        d.payload
            .as_chunks::<2>()
            .0
            .iter()
            .map(|c| i16::from_le_bytes(*c).unsigned_abs())
            .max()
            .unwrap_or(0)
            .min(i16::MAX as u16) as i16
    }

    fn mixed_frames(
        live: &LiveStreams,
        rx: &mut mpsc::Receiver<Outgoing>,
        ticks: u32,
    ) -> Vec<Vec<u8>> {
        for _ in 0..ticks {
            live.mix_tick();
        }
        audio_frames(&drain(rx))
    }

    #[test]
    fn mixed_stream_sums_consenting_talkers_and_stays_silent_otherwise() {
        let live = LiveStreams::new(
            LiveStreamConfig {
                allow_pcm: true,
                ..cfg(256)
            },
            true,
            false,
            0,
            Uuid::nil(),
        );
        let app = AppId(Uuid::new_v4());
        let ch = ChannelId(Uuid::new_v4());
        let alice = UserId(Uuid::new_v4());
        let bob = UserId(Uuid::new_v4());
        let carol = UserId(Uuid::new_v4());
        let spec = StreamSpec {
            format: StreamFormat::PcmS16le,
            users: Some(vec![alice, bob]),
            mix: true,
            ..StreamSpec::default()
        };
        let (info, mut r) = live.open(app, ch, spec, StreamMode::Pull, None).unwrap();
        assert!(info.mix);
        assert!(matches!(
            drain(&mut r.rx).as_slice(),
            [Outgoing::Control(ControlFrame::Hello { mix: true, .. })]
        ));

        // Silent channel: the clock runs, no frames are produced.
        assert!(mixed_frames(&live, &mut r.rx, 10).is_empty());

        let mut enc_a = tone_encoder();
        let mut enc_b = tone_encoder();
        let mut enc_c = tone_encoder();
        let (mut pa, mut pb, mut pc) = (0.0f32, 0.0f32, 0.0f32);
        let mut ts = 0u32;
        // One 20 ms step of the channel: the listed talkers send a packet, the clock ticks.
        let mut step =
            |live: &LiveStreams, rx: &mut mpsc::Receiver<Outgoing>, a: bool, b: bool, c: bool| {
                ts += 960;
                if a {
                    live.on_audio(ch, alice, 1, ts, &opus_tone(&mut enc_a, 0.5, &mut pa));
                }
                if b {
                    live.on_audio(ch, bob, 2, ts, &opus_tone(&mut enc_b, 0.5, &mut pb));
                }
                if c {
                    live.on_audio(ch, carol, 3, ts, &opus_tone(&mut enc_c, 0.9, &mut pc));
                }
                mixed_frames(live, rx, 1)
            };

        // Pending consent: audio is not mixed.
        for _ in 0..3 {
            assert!(step(&live, &mut r.rx, true, false, false).is_empty());
        }

        // Alice alone: mixed frames are hers (user id nil, ssrc 0, first flag).
        live.set_consent(app, info.id, alice, RecordingConsent::Accepted)
            .unwrap();
        let mut frames = Vec::new();
        for _ in 0..8 {
            frames.extend(step(&live, &mut r.rx, true, false, false));
        }
        assert_eq!(frames.len(), 8);
        let d = decode_frame(&frames[0]).unwrap();
        assert_eq!(d.user_id, UserId(Uuid::nil()));
        assert_eq!(d.ssrc, 0);
        assert_eq!(d.flags & FLAG_FIRST, FLAG_FIRST);
        assert_eq!(d.payload.len(), FRAME_SAMPLES as usize * 2);
        assert_eq!(decode_frame(&frames[1]).unwrap().flags, 0);
        // The decoder joined mid-stream (the pending packets were never decoded) and needs a
        // few frames to converge; measure a settled one, expect ~0.5 full scale.
        let solo = pcm_peak(&frames[7]);
        assert!((12_000..=20_000).contains(&solo), "solo peak {solo}");

        // Alice + Bob: louder than Alice alone. Carol is not selected and never counts.
        live.set_consent(app, info.id, bob, RecordingConsent::Accepted)
            .unwrap();
        let mut frames = Vec::new();
        for _ in 0..8 {
            frames.extend(step(&live, &mut r.rx, true, true, true));
        }
        assert_eq!(frames.len(), 8);
        let duo = pcm_peak(&frames[7]);
        assert!(duo > solo + 4_000, "duo peak {duo} vs solo {solo}");
        assert!(!live.get(app, info.id).unwrap().consent.contains_key(&carol));

        // Bob leaves: his queue is dropped, the mix continues with Alice alone.
        live.on_participant_left(ch, bob);
        let mut frames = Vec::new();
        for _ in 0..3 {
            frames.extend(step(&live, &mut r.rx, true, false, false));
        }
        assert_eq!(frames.len(), 3);
        assert!(pcm_peak(&frames[2]) < duo - 4_000);

        // A garbage packet is ignored without breaking the talker.
        live.on_audio(ch, alice, 1, 1, &[0xFF, 0x00, 0x11, 0x22]);
        let frames = step(&live, &mut r.rx, true, false, false);
        assert_eq!(frames.len(), 1);
        assert!(pcm_peak(&frames[0]) > 8_000);

        // After the hangover the stream pauses; the next frame carries the gap flag.
        let hangover = mixed_frames(
            &live,
            &mut r.rx,
            (MIX_HANGOVER_TICKS + MIX_PLC_FRAMES) as u32 + 5,
        );
        assert!(hangover.len() <= (MIX_HANGOVER_TICKS + MIX_PLC_FRAMES) as usize + 1);
        assert!(mixed_frames(&live, &mut r.rx, 5).is_empty());
        let frames = step(&live, &mut r.rx, true, false, false);
        assert_eq!(decode_frame(&frames[0]).unwrap().flags & FLAG_GAP, FLAG_GAP);
    }

    #[test]
    fn mixed_stream_clips_softly_and_encodes_opus() {
        let live = LiveStreams::new(cfg(256), false, false, 0, Uuid::nil());
        let app = AppId(Uuid::new_v4());
        let ch = ChannelId(Uuid::new_v4());
        let spec = StreamSpec {
            mix: true,
            ..StreamSpec::default()
        };
        let (_info, mut r) = live.open(app, ch, spec, StreamMode::Pull, None).unwrap();
        drain(&mut r.rx);
        let users: Vec<UserId> = (0..4).map(|_| UserId(Uuid::new_v4())).collect();
        let mut encs: Vec<opus::Encoder> = users.iter().map(|_| tone_encoder()).collect();
        let mut phases = vec![0.0f32; users.len()];
        for i in 0..4u32 {
            for (k, u) in users.iter().enumerate() {
                let pkt = opus_tone(&mut encs[k], 0.9, &mut phases[k]);
                live.on_audio(ch, *u, k as u32 + 1, i * 960, &pkt);
            }
        }
        let frames = mixed_frames(&live, &mut r.rx, 4);
        assert_eq!(frames.len(), 4);
        let mut dec = opus::Decoder::new(SAMPLE_RATE, opus::Channels::Mono).unwrap();
        let mut pcm = vec![0i16; MAX_PCM_SAMPLES];
        for f in &frames {
            let d = decode_frame(f).unwrap();
            assert_eq!(d.codec, CODEC_OPUS);
            let n = dec.decode(d.payload, &mut pcm, false).unwrap();
            assert_eq!(n, FRAME_SAMPLES as usize);
        }
        // Four talkers at 0.9 sum to 3.6 full scale before clipping; the decoded frame is
        // loud but never wrapped (a wrap would show up as a sign flip between neighbours).
        let last = &pcm[..FRAME_SAMPLES as usize];
        let peak = last.iter().map(|s| s.unsigned_abs()).max().unwrap();
        assert!(peak > 20_000, "peak {peak}");
        let wraps = last
            .windows(2)
            .filter(|w| (i32::from(w[0]) - i32::from(w[1])).abs() > 40_000)
            .count();
        assert_eq!(wraps, 0);
    }

    #[test]
    fn mixed_stream_limits() {
        let live = LiveStreams::new(
            LiveStreamConfig {
                max_mix_streams: 1,
                max_per_channel: 4,
                max_per_app: 8,
                ..cfg(64)
            },
            false,
            false,
            0,
            Uuid::nil(),
        );
        let app = AppId(Uuid::new_v4());
        let ch = ChannelId(Uuid::new_v4());
        let spec = StreamSpec {
            mix: true,
            ..StreamSpec::default()
        };
        let (a, _ra) = live
            .open(app, ch, spec.clone(), StreamMode::Pull, None)
            .unwrap();
        assert!(matches!(
            live.open(app, ch, spec.clone(), StreamMode::Pull, None),
            Err(AurixError::Conflict(_))
        ));
        // Per-participant streams are not counted against the mixer cap.
        live.open(app, ch, StreamSpec::default(), StreamMode::Pull, None)
            .unwrap();
        live.close(Some(app), a.id, "test").unwrap();
        live.open(app, ch, spec, StreamMode::Pull, None).unwrap();

        let off = LiveStreams::new(
            LiveStreamConfig {
                max_mix_streams: 0,
                ..cfg(64)
            },
            false,
            false,
            0,
            Uuid::nil(),
        );
        assert!(matches!(
            off.open(
                app,
                ch,
                StreamSpec {
                    mix: true,
                    ..StreamSpec::default()
                },
                StreamMode::Pull,
                None
            ),
            Err(AurixError::Validation(_))
        ));
    }

    #[test]
    fn backlog_keeps_order_and_bounds() {
        let mut b = Backlog::new(3);
        let user = UserId(Uuid::new_v4());
        let ctl = |ev| {
            Outgoing::Control(ControlFrame::Participant {
                user_id: user,
                ssrc: 1,
                event: ev,
                consent: None,
            })
        };
        b.push(ctl(ParticipantEvent::AudioStarted));
        for i in 0..5u8 {
            b.push(Outgoing::Audio(Bytes::from(vec![i])));
        }
        b.push(ctl(ParticipantEvent::Left));
        b.push(Outgoing::Control(ControlFrame::Dropped { frames: 4 }));
        b.push(Outgoing::Control(ControlFrame::Hello {
            stream_id: Uuid::nil(),
            app_id: Uuid::nil(),
            channel_id: ChannelId(Uuid::nil()),
            format: StreamFormat::Opus,
            sample_rate: SAMPLE_RATE,
            channels: 1,
            frame_ms: 20,
            frame_version: FRAME_VERSION,
            users: None,
            consent_required: false,
            label: None,
            started_at: Utc::now(),
            mix: false,
            node_id: None,
            reconnects: 0,
        }));
        assert_eq!(b.dropped, 2 + 4);
        let items = b.take_items();
        assert_eq!(b.dropped, 0);
        assert_eq!(items.len(), 5);
        assert!(matches!(
            items[0],
            Outgoing::Control(ControlFrame::Participant {
                event: ParticipantEvent::AudioStarted,
                ..
            })
        ));
        assert_eq!(audio_frames(&items), vec![vec![2], vec![3], vec![4]]);
        assert!(matches!(
            items[4],
            Outgoing::Control(ControlFrame::Participant {
                event: ParticipantEvent::Left,
                ..
            })
        ));

        // Control frames are bounded on their own and never evict audio.
        let mut b = Backlog::new(2);
        b.push(Outgoing::Audio(Bytes::from_static(b"a")));
        for _ in 0..BACKLOG_CONTROL_FRAMES + 10 {
            b.push(ctl(ParticipantEvent::AudioStarted));
        }
        b.push(Outgoing::Audio(Bytes::from_static(b"b")));
        let items = b.into_items();
        assert_eq!(items.len(), BACKLOG_CONTROL_FRAMES + 2);
        assert_eq!(audio_frames(&items), vec![b"a".to_vec(), b"b".to_vec()]);
    }

    #[tokio::test]
    async fn pull_outage_buffers_and_resumes_with_drop_accounting() {
        let live = Arc::new(LiveStreams::new(
            LiveStreamConfig {
                outage_buffer_ms: 100,
                ..cfg(64)
            },
            false,
            false,
            0,
            Uuid::nil(),
        ));
        // 100 ms window = 5 frames of buffer.
        assert_eq!(live.cfg.outage_buffer_frames(), 5);
        let app = AppId(Uuid::new_v4());
        let other = AppId(Uuid::new_v4());
        let ch = ChannelId(Uuid::new_v4());
        let alice = UserId(Uuid::new_v4());
        let (info, mut r) = live
            .open(app, ch, StreamSpec::default(), StreamMode::Pull, None)
            .unwrap();
        drain(&mut r.rx);
        live.on_audio(ch, alice, 1, 0, &opus_silence());
        assert_eq!(audio_frames(&drain(&mut r.rx)).len(), 1);

        // While connected a resume is refused; another tenant never sees the stream.
        assert!(matches!(
            live.resume_pull(app, info.id).await,
            Err(AurixError::Conflict(_))
        ));
        assert!(matches!(
            live.resume_pull(other, info.id).await,
            Err(AurixError::NotFound(_))
        ));

        // The consumer drops: the stream is parked, not closed.
        assert!(live.detach_pull(r, "consumer_disconnected").is_none());
        let parked = live.get(app, info.id).unwrap();
        assert_eq!(parked.state, StreamState::Reconnecting);
        assert!(live.wants_channel(&ch));

        // Eight frames arrive while nobody listens: five are kept, three are dropped.
        for i in 1..=8u32 {
            live.on_audio(ch, alice, 1, i * 960, &opus_silence());
        }
        tokio::task::yield_now().await;
        // Let the parking task observe the frames.
        tokio::time::sleep(Duration::from_millis(20)).await;

        let (resumed, mut r2, replay) = live.resume_pull(app, info.id).await.unwrap();
        assert_eq!(resumed.state, StreamState::Streaming);
        assert_eq!(resumed.reconnects, 1);
        assert_eq!(resumed.frames_dropped, 3);
        assert_eq!(resumed.frames_sent, 1 + 5);
        assert!(matches!(
            replay[0],
            Outgoing::Control(ControlFrame::Hello { reconnects: 1, .. })
        ));
        assert!(matches!(
            replay[1],
            Outgoing::Control(ControlFrame::Dropped { frames: 3 })
        ));
        let frames = audio_frames(&replay);
        assert_eq!(frames.len(), 5);
        assert_eq!(decode_frame(&frames[0]).unwrap().rtp_timestamp, 4 * 960);
        assert_eq!(decode_frame(&frames[4]).unwrap().rtp_timestamp, 8 * 960);

        // Live frames continue on the fresh receiver.
        live.on_audio(ch, alice, 1, 9 * 960, &opus_silence());
        assert_eq!(audio_frames(&drain(&mut r2.rx)).len(), 1);

        // A second outage that outlives the window closes the stream.
        assert!(live.detach_pull(r2, "consumer_disconnected").is_none());
        tokio::time::sleep(Duration::from_millis(250)).await;
        assert!(live.get(app, info.id).is_none());
        assert!(!live.wants_channel(&ch));
        assert!(matches!(
            live.resume_pull(app, info.id).await,
            Err(AurixError::NotFound(_))
        ));
    }

    #[test]
    fn pull_without_outage_buffer_closes_on_detach() {
        let live = Arc::new(LiveStreams::new(
            LiveStreamConfig {
                outage_buffer_ms: 0,
                ..cfg(64)
            },
            false,
            false,
            0,
            Uuid::nil(),
        ));
        let app = AppId(Uuid::new_v4());
        let ch = ChannelId(Uuid::new_v4());
        let (info, r) = live
            .open(app, ch, StreamSpec::default(), StreamMode::Pull, None)
            .unwrap();
        let closed = live.detach_pull(r, "consumer_disconnected").unwrap();
        assert_eq!(closed.id, info.id);
        assert!(live.get(app, info.id).is_none());
    }
}
