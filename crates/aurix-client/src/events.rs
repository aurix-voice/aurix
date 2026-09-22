use aurix_common::protocol::{
    ChatMessage, ChatReadMarker, ParticipantEnergy, Transcript, TranslationInfo, TransmissionMode,
    TtsState, UserPosition,
};
use aurix_common::types::{
    ActionKind, AudioCodec, AudioPolicy, ChannelId, ChannelRole, DownlinkMode, DuckingConfig,
    NetworkQuality, SessionId, UserId,
};
use serde::Serialize;
use std::time::Duration;

use crate::media::MediaPath;
use crate::resilience::LossProfile;

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

/// A stored text conversation: a channel, or the direct exchange with one user.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ChatScope {
    Channel(ChannelId),
    Direct(UserId),
}

impl ChatScope {
    pub fn split(self) -> (Option<ChannelId>, Option<UserId>) {
        match self {
            Self::Channel(c) => (Some(c), None),
            Self::Direct(u) => (None, Some(u)),
        }
    }

    /// Server payloads carry `channel_id` / `user_id`; a channel wins when both are present.
    pub fn from_parts(channel_id: Option<ChannelId>, user_id: Option<UserId>) -> Self {
        match (channel_id, user_id) {
            (Some(c), _) => Self::Channel(c),
            (None, Some(u)) => Self::Direct(u),
            (None, None) => Self::Direct(UserId::from_uuid(uuid::Uuid::nil())),
        }
    }
}

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
    /// Priority speaker: their speech ducks everyone else (`ChannelConfig.ducking`).
    pub priority: bool,
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
    /// Primary media endpoint (`host:port`; IPv6 hosts bracketed).
    pub media_addr: String,
    /// Every public media endpoint of the node (IPv4 first, then IPv6). Contains at least
    /// `media_addr`; the client races them when binding UDP.
    pub media_addrs: Vec<String>,
    pub resume_grace: Duration,
    pub resumed: bool,
    /// The node accepts AURX media as binary frames on the control WebSocket.
    pub media_tunnel: bool,
    /// The node accepts AURX media as QUIC datagrams on its media port (0-RTT reconnects,
    /// connection migration); `Auto` sessions prefer it when `ClientConfig::quic` is on.
    pub media_quic: bool,
    /// The node accepts AURX media as frames on a dedicated TLS tunnel port (normally 443);
    /// `Auto` sessions use it before the WebSocket tunnel when UDP and QUIC are blocked.
    pub media_tls: bool,
    /// The node can deliver one server-mixed stream per channel instead of one stream per
    /// speaker (`Client::set_downlink_mode`).
    pub downlink_mix: bool,
    /// The node can denoise this session's uplink on request
    /// (`Client::set_server_noise_suppression`).
    pub noise_suppression: bool,
    /// The session was resumed on a different node than the one that opened it: same session
    /// id and SSRC, but a new media key and endpoint (the client rebinds transparently).
    pub migrated: bool,
    /// The WebSocket URL this session is served from (differs from `ClientConfig::ws_url`
    /// after a failover).
    pub endpoint: String,
    /// Other healthy nodes the client will try, in order, when this one stops answering.
    pub failover: Vec<String>,
    /// The node translates transcripts into a listener's language (`Client::set_translation`);
    /// `None` when the operator has not configured translation.
    pub translation: Option<TranslationInfo>,
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
        /// Priority-speaker ducking the server applies here (`None`: off).
        ducking: Option<DuckingConfig>,
        /// We are a priority speaker in this channel.
        priority: bool,
        /// Our grant allows speaking but every `max_speakers` slot is taken: `role` is
        /// `Listener` until a `ParticipantRoleChanged` promotes us.
        waiting_to_speak: bool,
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
    /// A member (possibly us) became or stopped being a priority speaker.
    ParticipantPriorityChanged {
        channel_id: ChannelId,
        user_id: UserId,
        priority: bool,
    },
    /// A member (possibly us) gained (`admitted`) or lost a speaker slot: `role` is its
    /// effective role now (`Listener` while demoted / waiting). The grant is unchanged.
    ParticipantRoleChanged {
        channel_id: ChannelId,
        user_id: UserId,
        role: ChannelRole,
        admitted: bool,
    },
    /// Game-audio hook: another member's priority speech started (`active`) or stopped
    /// ducking `channel_id`. Lower the game's music/SFX bus by `config.gain` with the
    /// config's attack/hold/release; the voice mix itself is ducked by the server already.
    DuckingChanged {
        channel_id: ChannelId,
        active: bool,
        config: DuckingConfig,
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
    /// The server acknowledged `Client::set_server_noise_suppression`; a fresh session
    /// starts without it and the request is re-applied automatically.
    NoiseSuppressionChanged(bool),
    /// The server applied `Client::set_translation` (tags normalised); a fresh session starts
    /// without translation and the requested preferences are re-applied automatically.
    TranslationChanged {
        language: Option<String>,
        spoken_language: Option<String>,
        speech: bool,
    },
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
    /// The uplink loss profile (FEC tuning / DRED tier chosen from the loss the server
    /// measures on our packets or the worst of our receivers reports on its downlink,
    /// whichever is higher) moved to another tier; already applied to the encoder.
    LossProfileChanged {
        profile: LossProfile,
        /// Loss the new profile protects against, percent (`0` when a session ended).
        uplink_loss_percent: f32,
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
    /// Answer to `Client::chat_history`: one page, newest first. `next_before` pages further
    /// into the past, `next_after` towards the present (absent when there is nothing there).
    ChatHistory {
        request_id: Option<RequestId>,
        scope: ChatScope,
        messages: Vec<ChatMessage>,
        next_before: Option<String>,
        next_after: Option<String>,
    },
    /// A message of a conversation this client takes part in was edited (`edited_at`) or
    /// deleted (`deleted_at`, empty text): replace the copy with the same `id`. `request_id`
    /// is set on the echo of this client's own `edit_chat` / `delete_chat`.
    ChatMessageUpdated {
        request_id: Option<RequestId>,
        message: ChatMessage,
    },
    /// `user_id` added or removed `reaction` on `message_id`; `count` is the number of users
    /// now carrying that reaction. `channel_id` / `message_*_user_id` locate the conversation.
    ChatReactionChanged {
        message_id: uuid::Uuid,
        channel_id: Option<ChannelId>,
        message_from_user_id: UserId,
        message_to_user_id: Option<UserId>,
        user_id: UserId,
        reaction: String,
        added: bool,
        count: u32,
        timestamp: chrono::DateTime<chrono::Utc>,
    },
    /// Answer to `Client::search_chat`: matches newest first; `next_before` pages further
    /// back. `scope` is `None` for a search across every direct conversation.
    ChatSearchResult {
        request_id: Option<RequestId>,
        scope: Option<ChatScope>,
        query: String,
        messages: Vec<ChatMessage>,
        next_before: Option<String>,
    },
    /// A read marker moved: this user's own (any device) or, with server-side read receipts,
    /// another participant's.
    ChatReadMarker(ChatReadMarker),
    /// Answer to `Client::chat_read_markers`.
    ChatReadMarkers {
        scope: ChatScope,
        markers: Vec<ChatReadMarker>,
        unread_count: u32,
    },
    /// Sent once after connecting, after the directed messages that arrived while this user
    /// was offline were replayed as `ChatMessage { message.offline: true }`. `truncated`: older
    /// unread ones exist beyond the server's replay limit (page them with `chat_history`).
    /// `per_device`: the replay followed this device's own acknowledged cursor
    /// (`ClientConfig::device_id`) rather than the user-wide read markers.
    ChatInboxSynced {
        delivered: u32,
        truncated: bool,
        per_device: bool,
    },
    Transcript(Transcript),
    /// A member of an end-to-end encrypted channel announced its identity key. Show the
    /// fingerprint for out-of-band verification; `previous_fingerprint` is set when a user
    /// we already trusted now presents a different key (a reinstall — or an impostor).
    E2eePeerKey {
        user_id: UserId,
        fingerprint: String,
        previous_fingerprint: Option<String>,
    },
    /// We can (or can no longer) decrypt `user_id`: their sender key arrived, or they left
    /// every encrypted channel we share.
    E2eePeerDecryptable {
        user_id: UserId,
        decryptable: bool,
    },
    /// Our sender key rotated (a member joined or left, or the counter wrapped); peers get
    /// the new key over the control channel.
    E2eeKeyRotated {
        generation: u8,
    },
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
