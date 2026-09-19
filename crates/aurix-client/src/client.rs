//! `Client`: one voice session. Owns a small tokio runtime for the control plane, the UDP media
//! transport, the capture encoder and the remote mixer, and exposes a synchronous, thread-safe
//! API plus a poll-based event queue — the shape engines want (audio callbacks push/pull PCM,
//! the game thread pumps events once per tick).
//!
//! Reconnect policy: a dropped control socket (or a stalled one: no message for three ping
//! intervals) triggers exponential-backoff reconnects that first try to *resume* the session
//! (same session id, SSRC and media key; peers notice nothing). If the server issues a fresh
//! session instead, previously joined channels are re-joined with the session token and
//! receiver preferences (local mutes, volumes, transmission, focus) are replayed. A
//! `SessionClose` from the server (kick, ban, erasure, shutdown) is terminal.

use aurix_common::protocol::{
    channel_id_hash, ControlMessage, ParticipantBrief, TransmissionMode, TtsDestination, TtsState,
    UserPosition,
};
use aurix_common::types::{ActionKind, ChannelId, RecordingConsent, SessionId, UserId};
use parking_lot::{Condvar, Mutex, RwLock};
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicU8, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;

use crate::audio::{CaptureEncoder, RemoteMixer, StreamStats, FRAME_SAMPLES};
use crate::config::ClientConfig;
use crate::control::{token_identity, ws_host, ControlConnection, SessionAck, TokenIdentity};
use crate::error::{ClientError, Result};
use crate::events::{ConnectionState, Event, Participant, RequestId, SessionInfo};
use crate::media::{resolve_media_addr, IncomingAudio, MediaStats, MediaTransport};

const MAX_QUEUED_EVENTS: usize = 4096;
const REQUEST_TICK: Duration = Duration::from_millis(200);
const BIND_ATTEMPTS: u32 = 3;
const BIND_TIMEOUT: Duration = Duration::from_secs(2);
/// Client refs of chat/TTS requests are kept this long after the last status so late
/// `TtsStatus` updates still map to the request id.
const REF_RETENTION: Duration = Duration::from_secs(600);
const DISCONNECT_WAIT: Duration = Duration::from_secs(3);

/// Uplink counters.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct TransmitStats {
    /// 20 ms frames produced by the encoder.
    pub frames_encoded: u64,
    /// Frames handed to the media transport (× number of target channels).
    pub frames_sent: u64,
    /// Frames dropped because the client was muted, gated by VAD or had no target channel.
    pub frames_gated: u64,
}

/// Everything the network-quality UI needs, in one snapshot.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ClientStats {
    pub state: Option<ConnectionState>,
    pub media: MediaStats,
    pub transmit: TransmitStats,
    /// Inter-arrival jitter of downlink audio, RFC 3550 style, in milliseconds.
    pub jitter_ms: f32,
    /// Downlink frames the jitter buffers declared lost, as a percentage of frames expected.
    pub loss_percent: f32,
    pub streams: Vec<StreamStats>,
}

#[derive(Debug, Default)]
struct Prefs {
    transmission: TransmissionMode,
    focus: Option<ChannelId>,
    /// `(user, channel or None = everywhere)` → muted.
    local_mutes: HashMap<(UserId, Option<ChannelId>), bool>,
    volumes: HashMap<UserId, f32>,
    transcripts_enabled: bool,
}

#[derive(Debug, Clone)]
struct ChannelState {
    hash: u32,
    transcription: bool,
    participants: HashMap<UserId, Participant>,
}

/// RFC 3550 inter-arrival jitter over all downlink audio (one estimator is enough for a UI bar).
#[derive(Debug, Default)]
struct JitterEstimator {
    last_arrival: Option<(u32, Instant, u32)>,
    jitter_ms: f32,
}

impl JitterEstimator {
    fn observe(&mut self, ssrc: u32, rtp_ts: u32, now: Instant) {
        if let Some((last_ssrc, last_at, last_ts)) = self.last_arrival {
            if last_ssrc == ssrc {
                let expected_ms = rtp_ts.wrapping_sub(last_ts) as f32 / 48.0;
                let actual_ms = now.duration_since(last_at).as_secs_f32() * 1000.0;
                if (0.0..1000.0).contains(&expected_ms) {
                    let d = (actual_ms - expected_ms).abs();
                    self.jitter_ms += (d - self.jitter_ms) / 16.0;
                }
            }
        }
        self.last_arrival = Some((ssrc, now, rtp_ts));
    }
}

struct Inner {
    cfg: RwLock<ClientConfig>,
    state: AtomicU8,
    events: Mutex<VecDeque<Event>>,
    event_cv: Condvar,
    wake: Mutex<Option<Arc<dyn Fn() + Send + Sync>>>,
    media: RwLock<Option<Arc<MediaTransport>>>,
    mixer: Mutex<RemoteMixer>,
    encoder: Mutex<CaptureEncoder>,
    muted: AtomicBool,
    local_speaking: AtomicBool,
    channels: Mutex<HashMap<ChannelId, ChannelState>>,
    prefs: Mutex<Prefs>,
    session: Mutex<Option<SessionInfo>>,
    identity: Mutex<TokenIdentity>,
    rtp_ts: AtomicU32,
    tx_stats: Mutex<TransmitStats>,
    jitter: Mutex<JitterEstimator>,
    next_request: AtomicU64,
    closing: AtomicBool,
    resume_seq: Mutex<Option<u32>>,
}

impl Inner {
    fn state(&self) -> ConnectionState {
        match self.state.load(Ordering::Acquire) {
            0 => ConnectionState::Disconnected,
            1 => ConnectionState::Connecting,
            2 => ConnectionState::Connected,
            3 => ConnectionState::MediaBound,
            4 => ConnectionState::Reconnecting,
            _ => ConnectionState::Failed,
        }
    }

    fn set_state(&self, s: ConnectionState) {
        let v = match s {
            ConnectionState::Disconnected => 0,
            ConnectionState::Connecting => 1,
            ConnectionState::Connected => 2,
            ConnectionState::MediaBound => 3,
            ConnectionState::Reconnecting => 4,
            ConnectionState::Failed => 5,
        };
        if self.state.swap(v, Ordering::AcqRel) != v {
            self.emit(Event::StateChanged(s));
        }
    }

    fn emit(&self, ev: Event) {
        {
            let mut q = self.events.lock();
            if q.len() >= MAX_QUEUED_EVENTS {
                q.pop_front();
            }
            q.push_back(ev);
        }
        self.event_cv.notify_all();
        let hook = self.wake.lock().clone();
        if let Some(h) = hook {
            h();
        }
    }

    fn next_request_id(&self) -> RequestId {
        self.next_request.fetch_add(1, Ordering::Relaxed)
    }

    fn is_online(&self) -> bool {
        matches!(
            self.state(),
            ConnectionState::Connecting
                | ConnectionState::Connected
                | ConnectionState::MediaBound
                | ConnectionState::Reconnecting
        )
    }

    /// Hashes of the joined channels the microphone currently reaches.
    fn transmit_targets(&self) -> Vec<u32> {
        let prefs = self.prefs.lock();
        let channels = self.channels.lock();
        channels
            .iter()
            .filter(|(id, _)| prefs.transmission.allows(id))
            .map(|(_, c)| c.hash)
            .collect()
    }

    fn on_incoming_audio(&self, audio: IncomingAudio) {
        self.jitter
            .lock()
            .observe(audio.sender_ssrc, audio.timestamp, Instant::now());
        let _ = self.mixer.lock().push(
            audio.sender_ssrc,
            audio.sequence,
            audio.volume,
            audio.direction,
            audio.opus.to_vec(),
        );
    }
}

enum Command {
    Join {
        channel_id: ChannelId,
        token: Option<String>,
        request_id: RequestId,
    },
    Leave {
        channel_id: ChannelId,
    },
    /// Fire-and-forget control message (preferences, positions, typing…).
    Send(ControlMessage),
    /// Chat or TTS request correlated through `client_ref`.
    Tracked {
        msg: ControlMessage,
        client_ref: String,
        request_id: RequestId,
        kind: TrackedKind,
    },
    Moderate {
        channel_id: ChannelId,
        user_id: UserId,
        action: ActionKind,
        token: String,
        reason: Option<String>,
        request_id: RequestId,
    },
    Disconnect {
        reason: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TrackedKind {
    Chat,
    Tts,
}

struct JoinRequest {
    channel_id: ChannelId,
    token: Option<String>,
    request_id: RequestId,
}

struct ModerationRequest {
    channel_id: ChannelId,
    user_id: UserId,
    action: ActionKind,
    request_id: RequestId,
}

struct TrackedRequest {
    request_id: RequestId,
    kind: TrackedKind,
    /// Until when a missing answer counts as a timeout (`None` once answered).
    deadline: Option<Instant>,
    last_seen: Instant,
}

/// Request bookkeeping; survives reconnects so queued joins are sent on the new socket.
#[derive(Default)]
struct Pending {
    join_queue: VecDeque<JoinRequest>,
    active_join: Option<(JoinRequest, Instant)>,
    /// Channels the client wants to be in (for automatic re-join after a fresh session).
    desired: HashSet<ChannelId>,
    active_moderation: Option<(ModerationRequest, Instant)>,
    moderation_queue: VecDeque<Command>,
    tracked: HashMap<String, TrackedRequest>,
}

enum Exit {
    UserClose(String),
    ServerClose(String),
    Dropped(String),
}

/// A voice session. Cheap to share (`Arc` inside); all methods are thread-safe.
pub struct Client {
    inner: Arc<Inner>,
    runtime: Option<tokio::runtime::Runtime>,
    cmd_tx: Mutex<Option<mpsc::UnboundedSender<Command>>>,
    done_rx: Mutex<Option<std::sync::mpsc::Receiver<()>>>,
}

impl Client {
    pub fn new(cfg: ClientConfig) -> Result<Self> {
        if cfg.ws_url.is_empty() {
            return Err(ClientError::InvalidArgument("ws_url is empty".into()));
        }
        let encoder = CaptureEncoder::new(cfg.bitrate_bps)?;
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(cfg.worker_threads.max(1))
            .thread_name("aurix-client")
            .enable_all()
            .build()
            .map_err(|e| ClientError::Transport(format!("tokio runtime: {e}")))?;
        let identity = token_identity(&cfg.token);
        let inner = Arc::new(Inner {
            mixer: Mutex::new(RemoteMixer::new(
                cfg.jitter_target_frames,
                cfg.jitter_max_frames,
            )),
            encoder: Mutex::new(encoder),
            cfg: RwLock::new(cfg),
            state: AtomicU8::new(0),
            events: Mutex::new(VecDeque::new()),
            event_cv: Condvar::new(),
            wake: Mutex::new(None),
            media: RwLock::new(None),
            muted: AtomicBool::new(false),
            local_speaking: AtomicBool::new(false),
            channels: Mutex::new(HashMap::new()),
            prefs: Mutex::new(Prefs {
                transcripts_enabled: true,
                ..Prefs::default()
            }),
            session: Mutex::new(None),
            identity: Mutex::new(identity),
            rtp_ts: AtomicU32::new(rand::random()),
            tx_stats: Mutex::new(TransmitStats::default()),
            jitter: Mutex::new(JitterEstimator::default()),
            next_request: AtomicU64::new(1),
            closing: AtomicBool::new(false),
            resume_seq: Mutex::new(None),
        });
        Ok(Self {
            inner,
            runtime: Some(runtime),
            cmd_tx: Mutex::new(None),
            done_rx: Mutex::new(None),
        })
    }

    // ---------------------------------------------------------------- lifecycle

    /// Start connecting; progress arrives as events (`StateChanged`, `SessionReady`,
    /// `MediaBound` or `Disconnected`). Idempotent while a connection is alive.
    pub fn connect(&self) -> Result<()> {
        let mut slot = self.cmd_tx.lock();
        if slot.is_some() && self.inner.is_online() {
            return Ok(());
        }
        let (tx, rx) = mpsc::unbounded_channel();
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        self.inner.closing.store(false, Ordering::Release);
        let inner = Arc::clone(&self.inner);
        let runtime = self.runtime.as_ref().ok_or(ClientError::Closed)?;
        runtime.spawn(async move {
            run(inner, rx).await;
            let _ = done_tx.send(());
        });
        *slot = Some(tx);
        *self.done_rx.lock() = Some(done_rx);
        Ok(())
    }

    /// Close the session on the server (not resumable) and stop all threads. Blocks briefly
    /// until the control task acknowledges. Safe to call twice.
    pub fn disconnect(&self) {
        self.disconnect_with("client disconnect");
    }

    pub fn disconnect_with(&self, reason: &str) {
        self.inner.closing.store(true, Ordering::Release);
        let tx = self.cmd_tx.lock().take();
        let done = self.done_rx.lock().take();
        if let Some(tx) = tx {
            let _ = tx.send(Command::Disconnect {
                reason: reason.into(),
            });
            if let Some(done) = done {
                let _ = done.recv_timeout(DISCONNECT_WAIT);
            }
        }
        if let Some(m) = self.inner.media.write().take() {
            m.stop();
        }
        self.inner.mixer.lock().clear();
        self.inner.channels.lock().clear();
        *self.inner.session.lock() = None;
        if self.inner.state() != ConnectionState::Failed {
            self.inner.set_state(ConnectionState::Disconnected);
        }
    }

    /// Replace the session JWT used for the next (re)connect and for automatic re-joins.
    pub fn set_token(&self, token: &str) -> Result<()> {
        if token.is_empty() {
            return Err(ClientError::InvalidArgument("token is empty".into()));
        }
        self.inner.cfg.write().token = token.to_string();
        *self.inner.identity.lock() = token_identity(token);
        Ok(())
    }

    pub fn state(&self) -> ConnectionState {
        self.inner.state()
    }

    pub fn session(&self) -> Option<SessionInfo> {
        self.inner.session.lock().clone()
    }

    /// This client's user id as claimed by the token (unverified), or learned from a `Kick`.
    pub fn user_id(&self) -> Option<UserId> {
        self.inner.identity.lock().user_id
    }

    // ------------------------------------------------------------------- events

    pub fn poll_event(&self) -> Option<Event> {
        self.inner.events.lock().pop_front()
    }

    /// Block up to `timeout` for the next event.
    pub fn wait_event(&self, timeout: Duration) -> Option<Event> {
        let mut q = self.inner.events.lock();
        if let Some(e) = q.pop_front() {
            return Some(e);
        }
        let deadline = Instant::now() + timeout;
        while q.is_empty() {
            if self.inner.event_cv.wait_until(&mut q, deadline).timed_out() {
                break;
            }
        }
        q.pop_front()
    }

    /// Called (on an internal thread) every time an event is queued. Must return quickly and
    /// must not call back into the client; typically it signals the game thread to pump.
    pub fn set_wake_hook(&self, hook: Option<Arc<dyn Fn() + Send + Sync>>) {
        *self.inner.wake.lock() = hook;
    }

    pub fn pending_events(&self) -> usize {
        self.inner.events.lock().len()
    }

    // ----------------------------------------------------------------- channels

    fn send_cmd(&self, cmd: Command) -> Result<()> {
        let guard = self.cmd_tx.lock();
        let tx = guard.as_ref().ok_or(ClientError::NotConnected)?;
        if !self.inner.is_online() {
            return Err(ClientError::NotConnected);
        }
        tx.send(cmd).map_err(|_| ClientError::NotConnected)
    }

    /// Join `channel_id`; `join_token` is a one-time `join` action token (required when the
    /// server enforces action tokens), otherwise the session JWT's channel claims apply.
    /// Completes with `ChannelJoined` or `RequestFailed` carrying the returned id.
    pub fn join_channel(
        &self,
        channel_id: ChannelId,
        join_token: Option<&str>,
    ) -> Result<RequestId> {
        let request_id = self.inner.next_request_id();
        self.send_cmd(Command::Join {
            channel_id,
            token: join_token.map(str::to_string),
            request_id,
        })?;
        Ok(request_id)
    }

    pub fn leave_channel(&self, channel_id: ChannelId) -> Result<()> {
        self.send_cmd(Command::Leave { channel_id })
    }

    pub fn joined_channels(&self) -> Vec<ChannelId> {
        self.inner.channels.lock().keys().copied().collect()
    }

    /// Whether speech in a joined channel is transcribed server-side (`Transcript` events).
    pub fn channel_transcribes(&self, channel_id: ChannelId) -> bool {
        self.inner
            .channels
            .lock()
            .get(&channel_id)
            .is_some_and(|c| c.transcription)
    }

    pub fn participants(&self, channel_id: ChannelId) -> Vec<Participant> {
        self.inner
            .channels
            .lock()
            .get(&channel_id)
            .map(|c| c.participants.values().cloned().collect())
            .unwrap_or_default()
    }

    /// User owning `ssrc` (microphone or its synthesized TTS voice), searched across channels.
    pub fn user_for_ssrc(&self, ssrc: u32) -> Option<UserId> {
        let base = crate::source_ssrc(ssrc);
        self.inner.channels.lock().values().find_map(|c| {
            c.participants
                .values()
                .find(|p| p.ssrc == base)
                .map(|p| p.user_id)
        })
    }

    // --------------------------------------------------------------------- audio

    /// Feed interleaved f32 capture PCM at any rate / channel count. Encodes 20 ms Opus frames
    /// and sends them to every channel the transmission mode allows. Real-time safe apart from
    /// short uncontended locks; call from the audio thread.
    pub fn push_capture_f32(&self, pcm: &[f32], sample_rate: u32, channels: u8) {
        let media = self.inner.media.read().clone();
        let targets = self.inner.transmit_targets();
        let mut enc = self.inner.encoder.lock();
        let vad_gate = self.inner.cfg.read().vad_gate;
        let muted = self.inner.muted.load(Ordering::Relaxed);
        let mut speaking_flip: Option<bool> = None;
        let mut stats = TransmitStats::default();
        enc.push_f32(pcm, sample_rate, channels, |frame| {
            stats.frames_encoded += 1;
            let ts = self
                .inner
                .rtp_ts
                .fetch_add(FRAME_SAMPLES as u32, Ordering::Relaxed);
            let speaking = frame.speech && !muted;
            if self.inner.local_speaking.swap(speaking, Ordering::Relaxed) != speaking {
                speaking_flip = Some(speaking);
            }
            if muted || (vad_gate && !frame.speech) || targets.is_empty() {
                stats.frames_gated += 1;
                return;
            }
            if let Some(m) = &media {
                for hash in &targets {
                    m.send_audio(*hash, ts, Some(frame.level), &frame.opus);
                    stats.frames_sent += 1;
                }
            } else {
                stats.frames_gated += 1;
            }
        });
        drop(enc);
        {
            let mut s = self.inner.tx_stats.lock();
            s.frames_encoded += stats.frames_encoded;
            s.frames_sent += stats.frames_sent;
            s.frames_gated += stats.frames_gated;
        }
        if let Some(speaking) = speaking_flip {
            self.inner.emit(Event::LocalSpeaking(speaking));
        }
    }

    /// [`Self::push_capture_f32`] for i16 PCM.
    pub fn push_capture_i16(&self, pcm: &[i16], sample_rate: u32, channels: u8) {
        let f: Vec<f32> = pcm.iter().map(|&s| s as f32 / 32768.0).collect();
        self.push_capture_f32(&f, sample_rate, channels);
    }

    /// Send an already-encoded 20 ms Opus frame (engines with their own encoder). Bypasses
    /// the VAD; `level` is the RFC 6464 level byte (`None` = unknown).
    pub fn send_opus_frame(&self, opus: &[u8], level: Option<u8>) -> Result<()> {
        if self.inner.muted.load(Ordering::Relaxed) {
            return Ok(());
        }
        let media = self
            .inner
            .media
            .read()
            .clone()
            .ok_or(ClientError::NotConnected)?;
        let ts = self
            .inner
            .rtp_ts
            .fetch_add(FRAME_SAMPLES as u32, Ordering::Relaxed);
        let targets = self.inner.transmit_targets();
        let mut s = self.inner.tx_stats.lock();
        s.frames_encoded += 1;
        for hash in &targets {
            media.send_audio(*hash, ts, level, opus);
            s.frames_sent += 1;
        }
        if targets.is_empty() {
            s.frames_gated += 1;
        }
        Ok(())
    }

    /// Mix every remote participant into `output` (interleaved, `channels` wide, **added** to
    /// the buffer). Returns the number of contributing streams. Call from the playback callback.
    pub fn mix_output_f32(&self, output: &mut [f32], channels: u8) -> usize {
        self.inner.mixer.lock().mix(output, channels)
    }

    /// [`Self::mix_output_f32`] into i16 (overwrites `output`).
    pub fn mix_output_i16(&self, output: &mut [i16], channels: u8) -> usize {
        self.inner.mixer.lock().mix_i16(output, channels)
    }

    /// Microphone mute: frames are still encoded (VAD/level keep working) but not sent, and
    /// peers get `MuteStateChanged`.
    pub fn set_muted(&self, muted: bool) {
        if self.inner.muted.swap(muted, Ordering::AcqRel) != muted {
            if let Some(m) = self.inner.media.read().as_ref() {
                m.send_mute_state(muted);
            }
            if muted && self.inner.local_speaking.swap(false, Ordering::AcqRel) {
                self.inner.emit(Event::LocalSpeaking(false));
            }
        }
    }

    pub fn is_muted(&self) -> bool {
        self.inner.muted.load(Ordering::Relaxed)
    }

    pub fn is_speaking(&self) -> bool {
        self.inner.local_speaking.load(Ordering::Relaxed)
    }

    /// Software microphone gain `0..=4`.
    pub fn set_input_gain(&self, gain: f32) {
        self.inner.encoder.lock().set_gain(gain);
    }

    pub fn input_gain(&self) -> f32 {
        self.inner.encoder.lock().gain()
    }

    /// Current microphone energy `0..=1` (RMS of the last frame).
    pub fn input_energy(&self) -> f32 {
        self.inner.encoder.lock().vad.energy()
    }

    pub fn set_vad(&self, threshold: f32, hangover_frames: u32) {
        let mut enc = self.inner.encoder.lock();
        enc.vad.threshold = threshold.clamp(0.0, 1.0);
        enc.vad.hangover_frames = hangover_frames;
    }

    pub fn set_vad_gate(&self, enabled: bool) {
        self.inner.cfg.write().vad_gate = enabled;
    }

    pub fn set_bitrate(&self, bitrate_bps: u32) -> Result<()> {
        self.inner.encoder.lock().set_bitrate(bitrate_bps)?;
        self.inner.cfg.write().bitrate_bps = bitrate_bps;
        Ok(())
    }

    /// Master playback volume `0..=2`.
    pub fn set_output_volume(&self, volume: f32) {
        self.inner.mixer.lock().set_output_volume(volume);
    }

    pub fn set_output_muted(&self, muted: bool) {
        self.inner.mixer.lock().set_output_muted(muted);
    }

    /// Drop buffered capture (device switch) so stale samples are not sent.
    pub fn reset_capture(&self) {
        self.inner.encoder.lock().reset();
    }

    // ------------------------------------------------------------ preferences

    /// Stop hearing `user_id` in `channel_id` (or everywhere with `None`). Receiver-local.
    pub fn set_participant_mute(
        &self,
        user_id: UserId,
        channel_id: Option<ChannelId>,
        muted: bool,
    ) -> Result<()> {
        self.inner
            .prefs
            .lock()
            .local_mutes
            .insert((user_id, channel_id), muted);
        self.send_cmd(Command::Send(ControlMessage::SetParticipantMute {
            user_id,
            channel_id,
            muted,
        }))
    }

    /// Per-participant gain `0..=2`, applied server-side for this listener only.
    pub fn set_participant_volume(&self, user_id: UserId, volume: f32) -> Result<()> {
        if !volume.is_finite() || !(0.0..=2.0).contains(&volume) {
            return Err(ClientError::InvalidArgument(
                "volume must be in 0..=2".into(),
            ));
        }
        self.inner.prefs.lock().volumes.insert(user_id, volume);
        self.send_cmd(Command::Send(ControlMessage::SetParticipantVolume {
            user_id,
            volume,
        }))
    }

    /// Persistent mutual block (survives sessions); acked by `UserBlockChanged`.
    pub fn set_user_block(&self, user_id: UserId, blocked: bool) -> Result<()> {
        self.send_cmd(Command::Send(ControlMessage::SetUserBlock {
            user_id,
            blocked,
        }))
    }

    /// Which joined channels get the microphone. Applied locally at once and acked by
    /// `TransmissionChanged`.
    pub fn set_transmission(&self, mode: TransmissionMode) -> Result<()> {
        self.inner.prefs.lock().transmission = mode;
        self.send_cmd(Command::Send(ControlMessage::SetTransmission { mode }))
    }

    pub fn transmission(&self) -> TransmissionMode {
        self.inner.prefs.lock().transmission
    }

    pub fn set_channel_focus(&self, channel_id: Option<ChannelId>) -> Result<()> {
        self.inner.prefs.lock().focus = channel_id;
        self.send_cmd(Command::Send(ControlMessage::SetChannelFocus {
            channel_id,
        }))
    }

    /// Opt out of (or back into) `Transcript` events.
    pub fn set_transcripts(&self, enabled: bool) -> Result<()> {
        self.inner.prefs.lock().transcripts_enabled = enabled;
        self.send_cmd(Command::Send(ControlMessage::SetTranscripts { enabled }))
    }

    /// Positional channels: this player's (and, for hosts, other players') poses.
    pub fn update_positions(
        &self,
        channel_id: ChannelId,
        positions: Vec<UserPosition>,
    ) -> Result<()> {
        if positions.is_empty() || positions.len() > 64 {
            return Err(ClientError::InvalidArgument(
                "1..=64 positions per update".into(),
            ));
        }
        if positions
            .iter()
            .any(|p| !p.position.is_finite() || !p.orientation.is_finite())
        {
            return Err(ClientError::InvalidArgument(
                "position/orientation must be finite".into(),
            ));
        }
        self.send_cmd(Command::Send(ControlMessage::PositionUpdate {
            channel_id,
            positions,
        }))
    }

    pub fn respond_recording_consent(
        &self,
        recording_id: uuid::Uuid,
        consent: RecordingConsent,
    ) -> Result<()> {
        self.send_cmd(Command::Send(ControlMessage::RecordingConsentResponse {
            recording_id,
            consent,
        }))
    }

    /// Escape hatch: send any client→server control message verbatim.
    pub fn send_control(&self, msg: ControlMessage) -> Result<()> {
        self.send_cmd(Command::Send(msg))
    }

    // ------------------------------------------------------------ moderation

    /// Kick / mute / unmute `user_id` in `channel_id` with a one-time action token minted by
    /// the game backend. Completes with `ModerationApplied` or `RequestFailed`.
    pub fn moderate(
        &self,
        channel_id: ChannelId,
        user_id: UserId,
        action: ActionKind,
        token: &str,
        reason: Option<&str>,
    ) -> Result<RequestId> {
        if !action.needs_target() {
            return Err(ClientError::InvalidArgument(format!(
                "{action} is not a moderation action"
            )));
        }
        if token.is_empty() {
            return Err(ClientError::InvalidArgument("action token is empty".into()));
        }
        let request_id = self.inner.next_request_id();
        self.send_cmd(Command::Moderate {
            channel_id,
            user_id,
            action,
            token: token.to_string(),
            reason: reason.map(str::to_string),
            request_id,
        })?;
        Ok(request_id)
    }

    // ------------------------------------------------------------------ chat

    fn tracked(
        &self,
        kind: TrackedKind,
        build: impl FnOnce(String) -> ControlMessage,
    ) -> Result<RequestId> {
        let request_id = self.inner.next_request_id();
        let prefix = match kind {
            TrackedKind::Chat => 'm',
            TrackedKind::Tts => 't',
        };
        let client_ref = format!("{prefix}{request_id}-{:x}", rand::random::<u32>());
        self.send_cmd(Command::Tracked {
            msg: build(client_ref.clone()),
            client_ref,
            request_id,
            kind,
        })?;
        Ok(request_id)
    }

    /// Text message to a joined channel. The sender's own echo arrives as
    /// `ChatMessage { request_id: Some(id) }`; refusals as `RequestFailed`.
    pub fn send_chat(
        &self,
        channel_id: ChannelId,
        text: &str,
        metadata: Option<serde_json::Value>,
    ) -> Result<RequestId> {
        if text.trim().is_empty() {
            return Err(ClientError::InvalidArgument("text is empty".into()));
        }
        let text = text.to_string();
        self.tracked(TrackedKind::Chat, move |client_ref| {
            ControlMessage::ChatSend {
                channel_id,
                text,
                metadata,
                client_ref: Some(client_ref),
            }
        })
    }

    /// Directed message to one online user of the same application.
    pub fn send_direct_chat(
        &self,
        user_id: UserId,
        text: &str,
        metadata: Option<serde_json::Value>,
    ) -> Result<RequestId> {
        if text.trim().is_empty() {
            return Err(ClientError::InvalidArgument("text is empty".into()));
        }
        let text = text.to_string();
        self.tracked(TrackedKind::Chat, move |client_ref| {
            ControlMessage::ChatSendDirect {
                user_id,
                text,
                metadata,
                client_ref: Some(client_ref),
            }
        })
    }

    pub fn set_typing(&self, channel_id: ChannelId, typing: bool) -> Result<()> {
        self.send_cmd(Command::Send(ControlMessage::ChatTyping {
            channel_id,
            typing,
        }))
    }

    // ------------------------------------------------------------------- TTS

    /// Server-side text-to-speech played as this participant's voice. Lifecycle arrives as
    /// `TtsStatus { request_id: Some(id) }`.
    pub fn speak(
        &self,
        text: &str,
        channel_id: Option<ChannelId>,
        destination: TtsDestination,
        voice: Option<&str>,
    ) -> Result<RequestId> {
        if text.trim().is_empty() {
            return Err(ClientError::InvalidArgument("text is empty".into()));
        }
        let text = text.to_string();
        let voice = voice.map(str::to_string);
        self.tracked(TrackedKind::Tts, move |client_ref| {
            ControlMessage::TtsSpeak {
                channel_id,
                text,
                voice,
                destination,
                client_ref: Some(client_ref),
            }
        })
    }

    pub fn cancel_speech(&self) -> Result<()> {
        self.send_cmd(Command::Send(ControlMessage::TtsCancel))
    }

    // ------------------------------------------------------------------ stats

    pub fn stats(&self) -> ClientStats {
        let media = self
            .inner
            .media
            .read()
            .as_ref()
            .map(|m| m.stats())
            .unwrap_or_default();
        let streams = self.inner.mixer.lock().stream_stats();
        let lost: u64 = streams.iter().map(|s| s.lost).sum();
        let expected = media.audio_frames_received + lost;
        ClientStats {
            state: Some(self.inner.state()),
            media,
            transmit: *self.inner.tx_stats.lock(),
            jitter_ms: self.inner.jitter.lock().jitter_ms,
            loss_percent: if expected == 0 {
                0.0
            } else {
                lost as f32 * 100.0 / expected as f32
            },
            streams,
        }
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        self.disconnect();
        if let Some(rt) = self.runtime.take() {
            // The control task has already been told to stop (and waited for) by `disconnect`;
            // a background shutdown keeps `Drop` legal inside another async runtime.
            rt.shutdown_background();
        }
    }
}

// ======================================================================= control task

async fn run(inner: Arc<Inner>, mut cmd_rx: mpsc::UnboundedReceiver<Command>) {
    let mut resume: Option<(SessionId, String)> = None;
    let mut attempt: u32 = 0;
    let mut first = true;
    let mut pending = Pending::default();
    let mut cause: String;
    loop {
        let cfg = inner.cfg.read().clone();
        inner.set_state(if first {
            ConnectionState::Connecting
        } else {
            ConnectionState::Reconnecting
        });
        let connect = ControlConnection::connect(
            &cfg.ws_url,
            &cfg.token,
            resume.as_ref().map(|(s, t)| (*s, t.as_str())),
            cfg.request_timeout,
        );
        let connected = tokio::select! {
            r = connect => Some(r),
            // Let a disconnect during the handshake win.
            cmd = wait_for_disconnect(&mut cmd_rx, &inner, &mut pending) => {
                let _ = cmd;
                None
            }
        };
        let Some(connected) = connected else {
            finish(
                &inner,
                &mut pending,
                "closed by client",
                ConnectionState::Disconnected,
            );
            return;
        };
        match connected {
            Ok((conn, ack)) => {
                attempt = 0;
                let exit = session(
                    &inner,
                    &cfg,
                    conn,
                    ack,
                    &mut cmd_rx,
                    &mut pending,
                    &mut resume,
                    first,
                )
                .await;
                first = false;
                match exit {
                    Exit::UserClose(reason) => {
                        finish(&inner, &mut pending, &reason, ConnectionState::Disconnected);
                        return;
                    }
                    Exit::ServerClose(reason) => {
                        finish(&inner, &mut pending, &reason, ConnectionState::Disconnected);
                        return;
                    }
                    Exit::Dropped(c) => {
                        if !cfg.auto_reconnect {
                            finish(&inner, &mut pending, &c, ConnectionState::Disconnected);
                            return;
                        }
                        cause = c;
                    }
                }
            }
            Err(e) => {
                if first {
                    inner.emit(Event::Disconnected {
                        reason: e.to_string(),
                    });
                    finish(
                        &inner,
                        &mut pending,
                        &e.to_string(),
                        ConnectionState::Failed,
                    );
                    return;
                }
                if matches!(
                    e,
                    ClientError::Unauthorized(_) | ClientError::InvalidArgument(_)
                ) {
                    inner.emit(Event::FailedToRecover {
                        reason: e.to_string(),
                    });
                    finish(
                        &inner,
                        &mut pending,
                        &e.to_string(),
                        ConnectionState::Failed,
                    );
                    return;
                }
                cause = e.to_string();
            }
        }
        // Schedule the next attempt.
        attempt += 1;
        if attempt > cfg.reconnect.max_attempts {
            let reason = format!("gave up after {} attempts: {cause}", attempt - 1);
            inner.emit(Event::FailedToRecover {
                reason: reason.clone(),
            });
            finish(&inner, &mut pending, &reason, ConnectionState::Failed);
            return;
        }
        let delay = cfg.reconnect.delay(attempt);
        inner.set_state(ConnectionState::Reconnecting);
        inner.emit(Event::Recovering {
            attempt,
            delay,
            cause: cause.clone(),
        });
        // Media is dead until the new session binds (a resume keeps the sequence counter).
        let old = inner.media.write().take();
        if let Some(m) = old {
            let seq = m.next_sequence();
            m.stop();
            pending_sequence(&inner, seq);
        }
        let sleep = tokio::time::sleep(delay);
        tokio::select! {
            _ = sleep => {}
            _ = wait_for_disconnect(&mut cmd_rx, &inner, &mut pending) => {
                finish(&inner, &mut pending, "closed by client", ConnectionState::Disconnected);
                return;
            }
        }
    }
}

/// Remember where the uplink sequence stopped so a resumed session continues it (the server's
/// replay window would otherwise drop the first frames of the rebound socket).
fn pending_sequence(inner: &Inner, seq: u32) {
    *inner.resume_seq.lock() = Some(seq);
}

fn take_pending_sequence(inner: &Inner) -> u32 {
    inner
        .resume_seq
        .lock()
        .take()
        .unwrap_or_else(|| rand::random::<u32>() & 0x0fff_ffff)
}

/// Consume commands while offline: preference updates are already recorded in `Prefs` and
/// need no socket; joins are queued; tracked requests fail fast. Resolves on `Disconnect`.
async fn wait_for_disconnect(
    cmd_rx: &mut mpsc::UnboundedReceiver<Command>,
    inner: &Inner,
    pending: &mut Pending,
) -> String {
    loop {
        match cmd_rx.recv().await {
            None => return "client dropped".into(),
            Some(Command::Disconnect { reason }) => return reason,
            Some(Command::Join {
                channel_id,
                token,
                request_id,
            }) => {
                pending.desired.insert(channel_id);
                pending.join_queue.push_back(JoinRequest {
                    channel_id,
                    token,
                    request_id,
                });
            }
            Some(Command::Leave { channel_id }) => {
                pending.desired.remove(&channel_id);
                pending.join_queue.retain(|j| j.channel_id != channel_id);
                let removed = inner.channels.lock().remove(&channel_id);
                if removed.is_some() {
                    inner.emit(Event::ChannelLeft { channel_id });
                }
            }
            Some(Command::Send(_)) => {}
            Some(Command::Tracked { request_id, .. })
            | Some(Command::Moderate { request_id, .. }) => {
                inner.emit(Event::RequestFailed {
                    request_id,
                    code: "NOT_CONNECTED".into(),
                    message: "control connection is down".into(),
                });
            }
        }
    }
}

fn finish(inner: &Inner, pending: &mut Pending, reason: &str, state: ConnectionState) {
    fail_all_pending(inner, pending, "DISCONNECTED", reason);
    let channels: Vec<ChannelId> = inner.channels.lock().drain().map(|(id, _)| id).collect();
    for channel_id in channels {
        inner.emit(Event::ChannelLeft { channel_id });
    }
    if let Some(m) = inner.media.write().take() {
        m.stop();
    }
    inner.mixer.lock().clear();
    *inner.session.lock() = None;
    if inner.local_speaking.swap(false, Ordering::AcqRel) {
        inner.emit(Event::LocalSpeaking(false));
    }
    inner.set_state(state);
    inner.emit(Event::Disconnected {
        reason: reason.into(),
    });
}

fn fail_all_pending(inner: &Inner, pending: &mut Pending, code: &str, message: &str) {
    let mut ids: Vec<RequestId> = Vec::new();
    if let Some((j, _)) = pending.active_join.take() {
        ids.push(j.request_id);
    }
    ids.extend(pending.join_queue.drain(..).map(|j| j.request_id));
    if let Some((m, _)) = pending.active_moderation.take() {
        ids.push(m.request_id);
    }
    ids.extend(pending.moderation_queue.drain(..).filter_map(|c| match c {
        Command::Moderate { request_id, .. } => Some(request_id),
        _ => None,
    }));
    ids.extend(
        pending
            .tracked
            .drain()
            .filter(|(_, t)| t.deadline.is_some())
            .map(|(_, t)| t.request_id),
    );
    for request_id in ids {
        inner.emit(Event::RequestFailed {
            request_id,
            code: code.into(),
            message: message.into(),
        });
    }
}

fn participant_from_brief(b: &ParticipantBrief) -> Participant {
    Participant {
        user_id: b.user_id,
        display_name: b.display_name.clone(),
        ssrc: b.ssrc,
        role: b.role,
        muted: b.is_muted,
        server_muted: false,
        speaking: b.is_speaking,
        energy: 0.0,
    }
}

#[allow(clippy::too_many_arguments)]
async fn session(
    inner: &Arc<Inner>,
    cfg: &ClientConfig,
    mut conn: ControlConnection,
    ack: SessionAck,
    cmd_rx: &mut mpsc::UnboundedReceiver<Command>,
    pending: &mut Pending,
    resume: &mut Option<(SessionId, String)>,
    first: bool,
) -> Exit {
    let info = SessionInfo {
        session_id: ack.session_id,
        ssrc: ack.ssrc,
        media_addr: ack.media_addr.clone(),
        resume_grace: ack.resume_grace,
        resumed: ack.resumed,
    };
    *inner.session.lock() = Some(info.clone());
    *resume = Some((ack.session_id, ack.resume_token.clone()));
    inner.set_state(ConnectionState::Connected);
    inner.emit(Event::SessionReady(info));

    // Fresh session after a reconnect: the old memberships are gone on the server.
    if !first && !ack.resumed {
        let old: Vec<ChannelId> = inner.channels.lock().drain().map(|(id, _)| id).collect();
        inner.mixer.lock().clear();
        for channel_id in old {
            inner.emit(Event::ChannelLeft { channel_id });
        }
    }

    // UDP bind off the async threads (blocking socket I/O with retries).
    let media_res = {
        let host = ws_host(&cfg.ws_url).unwrap_or_else(|| "127.0.0.1".into());
        let addr = resolve_media_addr(&ack.media_addr, &host);
        let initial_sequence = take_pending_sequence(inner);
        let key = ack.media_key.clone();
        let (sid, ssrc) = (ack.session_id, ack.ssrc);
        match addr {
            Ok(addr) => tokio::task::spawn_blocking(move || {
                MediaTransport::bind(
                    addr,
                    sid,
                    ssrc,
                    &key,
                    initial_sequence,
                    BIND_ATTEMPTS,
                    BIND_TIMEOUT,
                )
            })
            .await
            .map_err(|e| ClientError::Transport(format!("bind task: {e}")))
            .and_then(|r| r),
            Err(e) => Err(e),
        }
    };
    let media = match media_res {
        Ok(m) => Arc::new(m),
        Err(e) => {
            let reason = format!("media bind failed: {e}");
            conn.abort();
            return Exit::Dropped(reason);
        }
    };
    {
        let sink_inner = Arc::clone(inner);
        media.start(Arc::new(move |audio| sink_inner.on_incoming_audio(audio)));
        if inner.muted.load(Ordering::Relaxed) {
            media.send_mute_state(true);
        }
        *inner.media.write() = Some(Arc::clone(&media));
    }
    inner.set_state(ConnectionState::MediaBound);
    inner.emit(Event::MediaBound);

    if !first {
        inner.emit(Event::Recovered {
            resumed: ack.resumed,
        });
    }
    if !ack.resumed {
        // Global preferences first; channel-scoped ones follow each re-join ack.
        let msgs = replay_global_prefs(inner);
        for m in msgs {
            if conn.send(&m).await.is_err() {
                return Exit::Dropped("send failed".into());
            }
        }
        if !first {
            for channel_id in pending.desired.iter().copied().collect::<Vec<_>>() {
                if !pending
                    .join_queue
                    .iter()
                    .any(|j| j.channel_id == channel_id)
                {
                    pending.join_queue.push_back(JoinRequest {
                        channel_id,
                        token: None,
                        request_id: 0,
                    });
                }
            }
        }
    }

    let mut ping = tokio::time::interval(cfg.ping_interval);
    ping.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut heartbeat = tokio::time::interval(cfg.heartbeat_interval);
    heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut requests = tokio::time::interval(REQUEST_TICK);
    let mut last_rx = Instant::now();
    let mut ping_nonce: u64 = rand::random();

    if let Err(exit) = pump_joins(inner, cfg, &mut conn, pending).await {
        return exit;
    }

    loop {
        tokio::select! {
            msg = conn.recv() => {
                last_rx = Instant::now();
                match msg {
                    Ok(Some(m)) => {
                        if let Some(exit) = handle_message(inner, cfg, &mut conn, pending, resume, m).await {
                            return exit;
                        }
                    }
                    Ok(None) => return Exit::Dropped("server closed the socket".into()),
                    Err(ClientError::Protocol(p)) => {
                        tracing::warn!("ignoring malformed control message: {p}");
                    }
                    Err(e) => return Exit::Dropped(e.to_string()),
                }
            }
            cmd = cmd_rx.recv() => {
                match cmd {
                    None => {
                        conn.close(ack.session_id, "client dropped").await;
                        return Exit::UserClose("client dropped".into());
                    }
                    Some(Command::Disconnect { reason }) => {
                        conn.close(ack.session_id, &reason).await;
                        return Exit::UserClose(reason);
                    }
                    Some(cmd) => {
                        if let Err(exit) = handle_command(inner, cfg, &mut conn, pending, cmd).await {
                            return exit;
                        }
                    }
                }
            }
            _ = ping.tick() => {
                if last_rx.elapsed() > cfg.ping_interval * 3 {
                    return Exit::Dropped("control connection timed out".into());
                }
                ping_nonce = ping_nonce.wrapping_add(1);
                if conn.send(&ControlMessage::Ping { nonce: ping_nonce }).await.is_err() {
                    return Exit::Dropped("ping failed".into());
                }
            }
            _ = heartbeat.tick() => {
                media.send_heartbeat();
                let stats = inner_stats(inner, &media);
                media.send_quality_report(stats.0, stats.1, stats.2);
            }
            _ = requests.tick() => {
                expire_requests(inner, pending);
                if let Err(exit) = pump_joins(inner, cfg, &mut conn, pending).await {
                    return exit;
                }
            }
        }
    }
}

fn inner_stats(inner: &Inner, media: &MediaTransport) -> (f32, f32, f32) {
    let m = media.stats();
    let streams = inner.mixer.lock().stream_stats();
    let lost: u64 = streams.iter().map(|s| s.lost).sum();
    let expected = m.audio_frames_received + lost;
    let loss = if expected == 0 {
        0.0
    } else {
        lost as f32 * 100.0 / expected as f32
    };
    (m.rtt_ms, inner.jitter.lock().jitter_ms, loss)
}

fn replay_global_prefs(inner: &Inner) -> Vec<ControlMessage> {
    let prefs = inner.prefs.lock();
    let mut msgs = Vec::new();
    if !prefs.transcripts_enabled {
        msgs.push(ControlMessage::SetTranscripts { enabled: false });
    }
    for (&(user_id, channel_id), &muted) in &prefs.local_mutes {
        if channel_id.is_none() && muted {
            msgs.push(ControlMessage::SetParticipantMute {
                user_id,
                channel_id: None,
                muted,
            });
        }
    }
    for (&user_id, &volume) in &prefs.volumes {
        if volume != 1.0 {
            msgs.push(ControlMessage::SetParticipantVolume { user_id, volume });
        }
    }
    if matches!(prefs.transmission, TransmissionMode::None) {
        msgs.push(ControlMessage::SetTransmission {
            mode: prefs.transmission,
        });
    }
    msgs
}

/// Preferences that reference `channel_id`, replayed once its automatic re-join is acked.
fn replay_channel_prefs(inner: &Inner, channel_id: ChannelId) -> Vec<ControlMessage> {
    let prefs = inner.prefs.lock();
    let mut msgs = Vec::new();
    for (&(user_id, ch), &muted) in &prefs.local_mutes {
        if ch == Some(channel_id) && muted {
            msgs.push(ControlMessage::SetParticipantMute {
                user_id,
                channel_id: ch,
                muted,
            });
        }
    }
    if prefs.focus == Some(channel_id) {
        msgs.push(ControlMessage::SetChannelFocus {
            channel_id: Some(channel_id),
        });
    }
    if let TransmissionMode::Single { channel_id: target } = prefs.transmission {
        if target == channel_id {
            msgs.push(ControlMessage::SetTransmission {
                mode: prefs.transmission,
            });
        }
    }
    msgs
}

/// Send the next queued join if none is in flight (joins are serialized because join errors
/// carry no channel id).
async fn pump_joins(
    inner: &Inner,
    cfg: &ClientConfig,
    conn: &mut ControlConnection,
    pending: &mut Pending,
) -> std::result::Result<(), Exit> {
    while pending.active_join.is_none() {
        let Some(job) = pending.join_queue.pop_front() else {
            break;
        };
        if inner.channels.lock().contains_key(&job.channel_id) && job.request_id == 0 {
            continue;
        }
        let token = job
            .token
            .clone()
            .unwrap_or_else(|| inner.cfg.read().token.clone());
        let msg = ControlMessage::ChannelJoin {
            channel_id: job.channel_id,
            token,
        };
        conn.send(&msg)
            .await
            .map_err(|e| Exit::Dropped(e.to_string()))?;
        pending.active_join = Some((job, Instant::now() + cfg.request_timeout));
    }
    if pending.active_moderation.is_none() {
        if let Some(Command::Moderate {
            channel_id,
            user_id,
            action,
            token,
            reason,
            request_id,
        }) = pending.moderation_queue.pop_front()
        {
            conn.send(&ControlMessage::ModerateParticipant {
                channel_id,
                user_id,
                action,
                token,
                reason,
            })
            .await
            .map_err(|e| Exit::Dropped(e.to_string()))?;
            pending.active_moderation = Some((
                ModerationRequest {
                    channel_id,
                    user_id,
                    action,
                    request_id,
                },
                Instant::now() + cfg.request_timeout,
            ));
        }
    }
    Ok(())
}

fn expire_requests(inner: &Inner, pending: &mut Pending) {
    let now = Instant::now();
    if pending
        .active_join
        .as_ref()
        .is_some_and(|(_, deadline)| now >= *deadline)
    {
        let (job, _) = pending.active_join.take().unwrap();
        if job.request_id != 0 {
            inner.emit(Event::RequestFailed {
                request_id: job.request_id,
                code: "TIMEOUT".into(),
                message: format!("no ChannelJoinAck for {}", job.channel_id.0),
            });
        }
    }
    if pending
        .active_moderation
        .as_ref()
        .is_some_and(|(_, deadline)| now >= *deadline)
    {
        let (job, _) = pending.active_moderation.take().unwrap();
        inner.emit(Event::RequestFailed {
            request_id: job.request_id,
            code: "TIMEOUT".into(),
            message: format!("no ModerateParticipantAck for {}", job.action),
        });
    }
    let mut timed_out = Vec::new();
    pending.tracked.retain(|client_ref, t| {
        if let Some(deadline) = t.deadline {
            if now >= deadline {
                timed_out.push((client_ref.clone(), t.request_id));
                return false;
            }
            return true;
        }
        now.duration_since(t.last_seen) < REF_RETENTION
    });
    for (_, request_id) in timed_out {
        inner.emit(Event::RequestFailed {
            request_id,
            code: "TIMEOUT".into(),
            message: "no answer from the server".into(),
        });
    }
}

async fn handle_command(
    inner: &Inner,
    cfg: &ClientConfig,
    conn: &mut ControlConnection,
    pending: &mut Pending,
    cmd: Command,
) -> std::result::Result<(), Exit> {
    match cmd {
        Command::Join {
            channel_id,
            token,
            request_id,
        } => {
            pending.desired.insert(channel_id);
            pending.join_queue.push_back(JoinRequest {
                channel_id,
                token,
                request_id,
            });
            pump_joins(inner, cfg, conn, pending).await
        }
        Command::Leave { channel_id } => {
            pending.desired.remove(&channel_id);
            pending.join_queue.retain(|j| j.channel_id != channel_id);
            conn.send(&ControlMessage::ChannelLeave { channel_id })
                .await
                .map_err(|e| Exit::Dropped(e.to_string()))?;
            forget_channel(inner, channel_id);
            Ok(())
        }
        Command::Send(msg) => conn
            .send(&msg)
            .await
            .map_err(|e| Exit::Dropped(e.to_string())),
        Command::Tracked {
            msg,
            client_ref,
            request_id,
            kind,
        } => {
            pending.tracked.insert(
                client_ref,
                TrackedRequest {
                    request_id,
                    kind,
                    deadline: Some(Instant::now() + cfg.request_timeout),
                    last_seen: Instant::now(),
                },
            );
            conn.send(&msg)
                .await
                .map_err(|e| Exit::Dropped(e.to_string()))
        }
        cmd @ Command::Moderate { .. } => {
            pending.moderation_queue.push_back(cmd);
            pump_joins(inner, cfg, conn, pending).await
        }
        Command::Disconnect { .. } => Ok(()),
    }
}

/// Remove a channel locally (left, kicked, or the server dropped it) and stop decoding its
/// exclusive senders.
fn forget_channel(inner: &Inner, channel_id: ChannelId) {
    let removed = inner.channels.lock().remove(&channel_id);
    if let Some(ch) = removed {
        let still_heard: HashSet<u32> = inner
            .channels
            .lock()
            .values()
            .flat_map(|c| c.participants.values().map(|p| p.ssrc))
            .collect();
        let media = inner.media.read().clone();
        let mut mixer = inner.mixer.lock();
        for p in ch.participants.values() {
            if !still_heard.contains(&p.ssrc) {
                mixer.remove(p.ssrc);
                mixer.remove(p.ssrc | crate::SYNTH_SSRC_FLAG);
                if let Some(m) = &media {
                    m.forget_sender(p.ssrc);
                    m.forget_sender(p.ssrc | crate::SYNTH_SSRC_FLAG);
                }
            }
        }
        drop(mixer);
        inner.emit(Event::ChannelLeft { channel_id });
    }
}

async fn handle_message(
    inner: &Inner,
    cfg: &ClientConfig,
    conn: &mut ControlConnection,
    pending: &mut Pending,
    resume: &mut Option<(SessionId, String)>,
    msg: ControlMessage,
) -> Option<Exit> {
    match msg {
        ControlMessage::Ping { nonce } => {
            if conn.send(&ControlMessage::Pong { nonce }).await.is_err() {
                return Some(Exit::Dropped("pong failed".into()));
            }
        }
        ControlMessage::Pong { .. } => {}
        ControlMessage::SessionInitAck { resume_token, .. } => {
            // Token rotation on an already-open session (defensive; the server sends it once).
            if let Some((_, tok)) = resume.as_mut() {
                if !resume_token.is_empty() {
                    *tok = resume_token;
                }
            }
        }
        ControlMessage::MediaBound { .. } => {}
        ControlMessage::SessionClose { reason, .. } => {
            return Some(Exit::ServerClose(reason));
        }
        ControlMessage::ChannelJoinAck {
            channel_id,
            participants,
            transcription,
        } => {
            let roster: HashMap<UserId, Participant> = participants
                .iter()
                .map(|b| (b.user_id, participant_from_brief(b)))
                .collect();
            let request_id = match &pending.active_join {
                Some((j, _)) if j.channel_id == channel_id => {
                    let (j, _) = pending.active_join.take().unwrap();
                    j.request_id
                }
                _ => 0,
            };
            let existed = inner
                .channels
                .lock()
                .insert(
                    channel_id,
                    ChannelState {
                        hash: channel_id_hash(&channel_id),
                        transcription,
                        participants: roster,
                    },
                )
                .is_some();
            pending.desired.insert(channel_id);
            // A resume replays acks for channels we already track: refresh silently.
            if !(existed && request_id == 0) {
                inner.emit(Event::ChannelJoined {
                    request_id,
                    channel_id,
                    participants: participants.iter().map(participant_from_brief).collect(),
                    transcription,
                });
            }
            if request_id == 0 && !existed {
                for m in replay_channel_prefs(inner, channel_id) {
                    if conn.send(&m).await.is_err() {
                        return Some(Exit::Dropped("send failed".into()));
                    }
                }
            }
            if let Err(exit) = pump_joins(inner, cfg, conn, pending).await {
                return Some(exit);
            }
        }
        ControlMessage::ParticipantJoined {
            channel_id,
            user_id,
            display_name,
            ssrc,
        } => {
            let participant = Participant {
                user_id,
                display_name,
                ssrc,
                role: aurix_common::types::ChannelRole::Speaker,
                muted: false,
                server_muted: false,
                speaking: false,
                energy: 0.0,
            };
            let known = {
                let mut channels = inner.channels.lock();
                match channels.get_mut(&channel_id) {
                    Some(c) => {
                        c.participants.insert(user_id, participant.clone());
                        true
                    }
                    None => false,
                }
            };
            if known {
                inner.emit(Event::ParticipantJoined {
                    channel_id,
                    participant,
                });
            }
        }
        ControlMessage::ParticipantLeft {
            channel_id,
            user_id,
        } => {
            let ssrc = {
                let mut channels = inner.channels.lock();
                channels
                    .get_mut(&channel_id)
                    .and_then(|c| c.participants.remove(&user_id))
                    .map(|p| p.ssrc)
            };
            if let Some(ssrc) = ssrc {
                let still_heard = inner
                    .channels
                    .lock()
                    .values()
                    .any(|c| c.participants.values().any(|p| p.ssrc == ssrc));
                if !still_heard {
                    let mut mixer = inner.mixer.lock();
                    mixer.remove(ssrc);
                    mixer.remove(ssrc | crate::SYNTH_SSRC_FLAG);
                    drop(mixer);
                    if let Some(m) = inner.media.read().as_ref() {
                        m.forget_sender(ssrc);
                        m.forget_sender(ssrc | crate::SYNTH_SSRC_FLAG);
                    }
                }
                inner.emit(Event::ParticipantLeft {
                    channel_id,
                    user_id,
                });
            }
        }
        ControlMessage::MuteStateChanged {
            channel_id,
            user_id,
            muted,
            server_muted,
        } => {
            if let Some(p) = inner
                .channels
                .lock()
                .get_mut(&channel_id)
                .and_then(|c| c.participants.get_mut(&user_id))
            {
                p.muted = muted;
                p.server_muted = server_muted;
            }
            inner.emit(Event::ParticipantMuteChanged {
                channel_id,
                user_id,
                muted,
                server_muted,
            });
        }
        ControlMessage::SpeakingStateChanged {
            channel_id,
            user_id,
            speaking,
        } => {
            if let Some(p) = inner
                .channels
                .lock()
                .get_mut(&channel_id)
                .and_then(|c| c.participants.get_mut(&user_id))
            {
                p.speaking = speaking;
                if !speaking {
                    p.energy = 0.0;
                }
            }
            inner.emit(Event::ParticipantSpeaking {
                channel_id,
                user_id,
                speaking,
            });
        }
        ControlMessage::ChannelEnergy { channel_id, levels } => {
            {
                let mut channels = inner.channels.lock();
                if let Some(c) = channels.get_mut(&channel_id) {
                    for l in &levels {
                        if let Some(p) = c.participants.get_mut(&l.user_id) {
                            p.energy = l.energy;
                        }
                    }
                }
            }
            inner.emit(Event::ChannelEnergy { channel_id, levels });
        }
        ControlMessage::PositionUpdate {
            channel_id,
            positions,
        } => inner.emit(Event::Positions {
            channel_id,
            positions,
        }),
        ControlMessage::UserBlockChanged { user_id, blocked } => {
            inner.emit(Event::UserBlockChanged { user_id, blocked })
        }
        ControlMessage::ReceiverPreferences {
            transmission,
            focus_channel,
            local_mutes,
            volumes,
            ..
        } => {
            let mut prefs = inner.prefs.lock();
            prefs.transmission = transmission;
            prefs.focus = focus_channel;
            for m in local_mutes {
                prefs.local_mutes.insert((m.user_id, m.channel_id), true);
            }
            for v in volumes {
                prefs.volumes.insert(v.user_id, v.volume);
            }
            drop(prefs);
            inner.emit(Event::TransmissionChanged(transmission));
            inner.emit(Event::ChannelFocusChanged(focus_channel));
        }
        ControlMessage::TransmissionChanged { mode } => {
            inner.prefs.lock().transmission = mode;
            inner.emit(Event::TransmissionChanged(mode));
        }
        ControlMessage::ChannelFocusChanged { channel_id } => {
            inner.prefs.lock().focus = channel_id;
            inner.emit(Event::ChannelFocusChanged(channel_id));
        }
        ControlMessage::BitrateCommand {
            target_bitrate_kbps,
            reason,
        } => {
            let bps = target_bitrate_kbps.saturating_mul(1000);
            let _ = inner.encoder.lock().set_bitrate(bps);
            inner.emit(Event::BitrateChanged {
                bitrate_bps: bps,
                reason,
            });
        }
        ControlMessage::RecordingNotification {
            channel_id,
            recording_id,
            active,
            initiated_by,
            live,
        } => inner.emit(Event::Recording {
            channel_id,
            recording_id,
            active,
            initiated_by,
            live,
        }),
        ControlMessage::Kick {
            channel_id,
            user_id,
            reason,
        } => {
            inner.identity.lock().user_id.get_or_insert(user_id);
            pending.desired.remove(&channel_id);
            forget_channel(inner, channel_id);
            inner.emit(Event::Kicked { channel_id, reason });
        }
        ControlMessage::ModerateParticipantAck {
            channel_id,
            user_id,
            action,
        } => {
            let request_id = match &pending.active_moderation {
                Some((m, _))
                    if m.channel_id == channel_id && m.user_id == user_id && m.action == action =>
                {
                    pending.active_moderation.take().unwrap().0.request_id
                }
                _ => 0,
            };
            inner.emit(Event::ModerationApplied {
                request_id,
                channel_id,
                user_id,
                action,
            });
            if let Err(exit) = pump_joins(inner, cfg, conn, pending).await {
                return Some(exit);
            }
        }
        ControlMessage::ChatMessageReceived { message } => {
            let request_id = message.client_ref.as_ref().and_then(|r| {
                let t = pending.tracked.get_mut(r)?;
                if t.kind != TrackedKind::Chat {
                    return None;
                }
                t.deadline = None;
                t.last_seen = Instant::now();
                Some(t.request_id)
            });
            if let Some(r) = message.client_ref.as_ref() {
                if request_id.is_some() {
                    pending.tracked.remove(r);
                }
            }
            inner.emit(Event::ChatMessage {
                request_id,
                message,
            });
        }
        ControlMessage::ParticipantTyping {
            channel_id,
            user_id,
            typing,
        } => inner.emit(Event::ParticipantTyping {
            channel_id,
            user_id,
            typing,
        }),
        ControlMessage::Transcript { transcript } => inner.emit(Event::Transcript(transcript)),
        ControlMessage::TtsStatus {
            request_id: server_request_id,
            client_ref,
            state,
            duration_ms,
            message,
        } => {
            let request_id = client_ref.as_ref().and_then(|r| {
                let t = pending.tracked.get_mut(r)?;
                if t.kind != TrackedKind::Tts {
                    return None;
                }
                t.deadline = None;
                t.last_seen = Instant::now();
                Some(t.request_id)
            });
            if matches!(
                state,
                TtsState::Finished | TtsState::Cancelled | TtsState::Failed
            ) {
                if let Some(r) = client_ref.as_ref() {
                    pending.tracked.remove(r);
                }
            }
            inner.emit(Event::TtsStatus {
                request_id,
                server_request_id,
                state,
                duration_ms,
                message,
            });
        }
        ControlMessage::Error {
            code,
            message,
            client_ref,
        } => {
            if let Some(r) = client_ref {
                if let Some(t) = pending.tracked.remove(&r) {
                    inner.emit(Event::RequestFailed {
                        request_id: t.request_id,
                        code,
                        message,
                    });
                    return None;
                }
            }
            if let Some((job, _)) = pending.active_join.take() {
                pending.desired.remove(&job.channel_id);
                if job.request_id != 0 {
                    inner.emit(Event::RequestFailed {
                        request_id: job.request_id,
                        code,
                        message,
                    });
                } else {
                    inner.emit(Event::RejoinFailed {
                        channel_id: job.channel_id,
                        code,
                        message,
                    });
                }
                if let Err(exit) = pump_joins(inner, cfg, conn, pending).await {
                    return Some(exit);
                }
                return None;
            }
            if let Some((job, _)) = pending.active_moderation.take() {
                inner.emit(Event::RequestFailed {
                    request_id: job.request_id,
                    code,
                    message,
                });
                if let Err(exit) = pump_joins(inner, cfg, conn, pending).await {
                    return Some(exit);
                }
                return None;
            }
            inner.emit(Event::ServerError { code, message });
        }
        // Client→server only, or browser signalling; nothing to do if echoed.
        ControlMessage::SessionInit { .. }
        | ControlMessage::ChannelJoin { .. }
        | ControlMessage::ChannelLeave { .. }
        | ControlMessage::SetParticipantMute { .. }
        | ControlMessage::SetParticipantVolume { .. }
        | ControlMessage::SetUserBlock { .. }
        | ControlMessage::SetTransmission { .. }
        | ControlMessage::SetChannelFocus { .. }
        | ControlMessage::OcclusionUpdate { .. }
        | ControlMessage::ReverbZoneUpdate { .. }
        | ControlMessage::QualityReport { .. }
        | ControlMessage::RecordingConsentResponse { .. }
        | ControlMessage::ModerateParticipant { .. }
        | ControlMessage::ChatSend { .. }
        | ControlMessage::ChatSendDirect { .. }
        | ControlMessage::ChatTyping { .. }
        | ControlMessage::SetTranscripts { .. }
        | ControlMessage::TtsSpeak { .. }
        | ControlMessage::TtsCancel
        | ControlMessage::WebRtcOffer { .. }
        | ControlMessage::WebRtcAnswer { .. } => {}
    }
    None
}
