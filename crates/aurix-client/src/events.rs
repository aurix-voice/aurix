use aurix_common::protocol::{
    ChatMessage, ParticipantEnergy, Transcript, TransmissionMode, TtsState, UserPosition,
};
use aurix_common::types::{
    ActionKind, AudioCodec, AudioPolicy, ChannelId, ChannelRole, DownlinkMode, NetworkQuality,
    SessionId, UserId,
};
use serde::Serialize;
use std::time::Duration;

use crate::media::MediaPath;

/// Lifecycle of the control connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ConnectionState {
    Disconnected,
    Connecting,
    /// WebSocket up, session opened, media link not yet authenticated.
    Connected,
    /// `SessionBind` acknowledged: audio flows.
    MediaBound,
    Reconnecting,
    Failed,
}

/// Correlates a request with its terminal event.
pub type RequestId = u64;

/// Snapshot of a channel member.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Participant {
    pub user_id: UserId,
    pub display_name: String,
    pub ssrc: u32,
    pub role: ChannelRole,
    pub muted: bool,
    pub server_muted: bool,
    pub speaking: bool,
    /// Last reported linear energy, `0..=1`.
    pub energy: f32,
}

/// How far presence and text reach in a positional channel (`PositionalConfig.roster_radius` /
/// `text_radius`, from `ChannelJoinAck`). `None` = the whole channel.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize)]
pub struct ChannelScope {
    /// The roster only lists members within this distance of us (once both positions are
    /// known); `ParticipantJoined`/`ParticipantLeft` also fire when someone moves in or out of
    /// range (leaving uses a 10 % wider radius so the edge does not flicker).
    pub roster_radius: Option<f32>,
    /// Channel chat, typing and transcripts reach only members within this distance.
    pub text_radius: Option<f32>,
}

/// Session facts handed out by `SessionInitAck`.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SessionInfo {
    pub session_id: SessionId,
    pub ssrc: u32,
    pub media_addr: String,
    pub resume_grace: Duration,
    pub resumed: bool,
    /// The node accepts AURX media as binary frames on the control WebSocket.
    pub media_tunnel: bool,
    /// The node can deliver one server-mixed stream per channel instead of one stream per
    /// speaker (`Client::set_downlink_mode`).
    pub downlink_mix: bool,
    /// The session was resumed on a different node than the one that opened it: same session
    /// id and SSRC, but a new media key and endpoint (the client rebinds transparently).
    pub migrated: bool,
    /// The WebSocket URL this session is served from (differs from `ClientConfig::ws_url`
    /// after a failover).
    pub endpoint: String,
    /// Other healthy nodes the client will try, in order, when this one stops answering.
    pub failover: Vec<String>,
}

/// Everything the integration observes. Poll with `Client::poll_event`.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum Event {
    StateChanged(ConnectionState),
    /// The session is open (fresh or resumed); media binding follows.
    SessionReady(SessionInfo),
    /// The media link is authenticated; audio may be sent and received. Followed by
    /// `MediaPathChanged` naming the link.
    MediaBound,
    /// Media now travels over `path`: on every bind and on each mid-session switch (UDP
    /// heartbeats stopped → tunnel; a UDP re-probe succeeded → back to UDP).
    MediaPathChanged {
        path: MediaPath,
        reason: String,
    },
    ChannelJoined {
        request_id: RequestId,
        channel_id: ChannelId,
        participants: Vec<Participant>,
        /// Speech is transcribed and captions delivered.
        transcription: bool,
        /// Speech is analysed by the server's content-safety classifier.
        safety_voice: bool,
        /// Presence / text range (both `None` for a whole-channel roster).
        scope: ChannelScope,
        /// This session's role; `Listener` cannot transmit here.
        role: ChannelRole,
        /// Members of the channel (all nodes), including listeners hidden from `participants`.
        participant_count: u32,
        /// Receive-only listeners are absent from the roster and never announced.
        hidden_listeners: bool,
    },
    ChannelLeft {
        channel_id: ChannelId,
    },
    ParticipantJoined {
        channel_id: ChannelId,
        participant: Participant,
    },
    ParticipantLeft {
        channel_id: ChannelId,
        user_id: UserId,
    },
    ParticipantMuteChanged {
        channel_id: ChannelId,
        user_id: UserId,
        muted: bool,
        server_muted: bool,
    },
    ParticipantSpeaking {
        channel_id: ChannelId,
        user_id: UserId,
        speaking: bool,
    },
    ChannelEnergy {
        channel_id: ChannelId,
        levels: Vec<ParticipantEnergy>,
    },
    Positions {
        channel_id: ChannelId,
        positions: Vec<UserPosition>,
    },
    /// The local VAD flipped (only when capture runs through `push_capture`).
    LocalSpeaking(bool),
    TransmissionChanged(TransmissionMode),
    ChannelFocusChanged(Option<ChannelId>),
    /// The server acknowledged a session codec change; capture and playback already follow it.
    AudioCodecChanged(AudioCodec),
    /// The server acknowledged a downlink mode (`Client::set_downlink_mode`); a fresh
    /// session starts in `Streams` and the requested mode is re-applied automatically.
    DownlinkModeChanged(DownlinkMode),
    UserBlockChanged {
        user_id: UserId,
        blocked: bool,
    },
    /// `live` = real-time stream to an operator service rather than a stored file.
    Recording {
        channel_id: ChannelId,
        recording_id: uuid::Uuid,
        active: bool,
        initiated_by: UserId,
        live: bool,
    },
    /// Server asked for a different uplink bitrate; applied automatically to the encoder.
    BitrateChanged {
        bitrate_bps: u32,
        reason: String,
    },
    /// The merged audio policy of the joined channels changed (join/leave or an operator
    /// edited a channel). Already applied to the encoder when
    /// `ClientConfig::follow_channel_policy` is on.
    AudioPolicyChanged(AudioPolicy),
    /// Periodic server-side view of both directions (bars 1–5, R-factor, MOS, RTT,
    /// jitter/loss per direction). Also available any time via `Client::stats().server`.
    NetworkQuality(NetworkQuality),
    Kicked {
        channel_id: ChannelId,
        reason: String,
    },
    ModerationApplied {
        request_id: RequestId,
        channel_id: ChannelId,
        user_id: UserId,
        action: ActionKind,
    },
    ChatMessage {
        /// Set on the sender's own echo of a message sent from this client.
        request_id: Option<RequestId>,
        message: ChatMessage,
    },
    ParticipantTyping {
        channel_id: ChannelId,
        user_id: UserId,
        typing: bool,
    },
    Transcript(Transcript),
    TtsStatus {
        request_id: Option<RequestId>,
        server_request_id: uuid::Uuid,
        state: TtsState,
        duration_ms: Option<u64>,
        message: Option<String>,
    },
    /// An automatic re-join after a fresh session was refused (e.g. the server requires a new
    /// `join` action token); the channel is no longer joined.
    RejoinFailed {
        channel_id: ChannelId,
        code: String,
        message: String,
    },
    /// A request (join, moderation, chat, TTS) was refused or timed out.
    RequestFailed {
        request_id: RequestId,
        code: String,
        message: String,
    },
    /// Unsolicited server error (not tied to a request).
    ServerError {
        code: String,
        message: String,
    },
    /// The connection dropped; a reconnect attempt is scheduled in `delay`.
    Recovering {
        attempt: u32,
        delay: Duration,
        cause: String,
    },
    /// Back online. `resumed == false` means the server issued a fresh session and the
    /// previous channels were re-joined (integrations see `ChannelLeft`/`ChannelJoined`).
    /// `migrated` means the session now lives on another node (see `SessionInfo::migrated`).
    Recovered {
        resumed: bool,
        migrated: bool,
    },
    /// The control connection moved to another node's WebSocket URL (a failover endpoint
    /// answered while the previous node did not). Fires before the `SessionReady` of that
    /// connection.
    EndpointChanged {
        url: String,
    },
    FailedToRecover {
        reason: String,
    },
    /// Terminal: the server closed the session (kick/ban/erasure/shutdown) or all reconnect
    /// attempts failed. The client is back in `Disconnected`/`Failed`.
    Disconnected {
        reason: String,
    },
}
