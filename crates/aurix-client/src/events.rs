use aurix_common::protocol::{
    ChatMessage, ParticipantEnergy, Transcript, TransmissionMode, TtsState, UserPosition,
};
use aurix_common::types::{
    ActionKind, AudioCodec, AudioPolicy, ChannelId, ChannelRole, NetworkQuality, SessionId, UserId,
};
use serde::Serialize;
use std::time::Duration;

/// Lifecycle of the control connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ConnectionState {
    Disconnected,
    Connecting,
    /// WebSocket up, session opened, UDP not yet authenticated.
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

/// Session facts handed out by `SessionInitAck`.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SessionInfo {
    pub session_id: SessionId,
    pub ssrc: u32,
    pub media_addr: String,
    pub resume_grace: Duration,
    pub resumed: bool,
}

/// Everything the integration observes. Poll with `Client::poll_event`.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum Event {
    StateChanged(ConnectionState),
    /// The session is open (fresh or resumed); media binding follows.
    SessionReady(SessionInfo),
    /// UDP authenticated; audio may be sent and received.
    MediaBound,
    ChannelJoined {
        request_id: RequestId,
        channel_id: ChannelId,
        participants: Vec<Participant>,
        /// Speech is transcribed and captions delivered.
        transcription: bool,
        /// Speech is analysed by the server's content-safety classifier.
        safety_voice: bool,
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
    Recovered {
        resumed: bool,
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
