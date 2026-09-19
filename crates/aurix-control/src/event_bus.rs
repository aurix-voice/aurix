use aurix_common::types::*;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;
use tracing::warn;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "event", content = "payload")]
pub enum ServerEvent {
    ParticipantJoined {
        app_id: AppId,
        channel_id: ChannelId,
        user_id: UserId,
        display_name: String,
        session_id: SessionId,
        #[serde(default)]
        ssrc: u32,
        timestamp: DateTime<Utc>,
    },
    ParticipantLeft {
        app_id: AppId,
        channel_id: ChannelId,
        user_id: UserId,
        session_id: SessionId,
        reason: String,
        timestamp: DateTime<Utc>,
    },
    ChannelCreated {
        app_id: AppId,
        channel_id: ChannelId,
        channel_type: ChannelType,
        timestamp: DateTime<Utc>,
    },
    ChannelDestroyed {
        app_id: AppId,
        channel_id: ChannelId,
        timestamp: DateTime<Utc>,
    },
    /// First participant joined an otherwise empty channel.
    ChannelActivated {
        app_id: AppId,
        channel_id: ChannelId,
        timestamp: DateTime<Utc>,
    },
    /// Last participant left the channel.
    ChannelDeactivated {
        app_id: AppId,
        channel_id: ChannelId,
        timestamp: DateTime<Utc>,
    },
    UserMuted {
        app_id: AppId,
        channel_id: ChannelId,
        user_id: UserId,
        muted_by: UserId,
        server_mute: bool,
        timestamp: DateTime<Utc>,
    },
    UserUnmuted {
        app_id: AppId,
        channel_id: ChannelId,
        user_id: UserId,
        unmuted_by: UserId,
        timestamp: DateTime<Utc>,
    },
    UserBanned {
        app_id: AppId,
        user_id: UserId,
        reason: String,
        banned_by: UserId,
        timestamp: DateTime<Utc>,
    },
    /// A user was erased (`DELETE /v1/users/:id` or the inactivity sweep). Every node closes
    /// the user's live sessions; tenants drop cached profile data.
    UserDeleted {
        app_id: AppId,
        user_id: UserId,
        deleted_by: UserId,
        automatic: bool,
        timestamp: DateTime<Utc>,
    },
    UserKicked {
        app_id: AppId,
        channel_id: ChannelId,
        user_id: UserId,
        kicked_by: UserId,
        reason: String,
        timestamp: DateTime<Utc>,
    },
    QualityAlert {
        app_id: AppId,
        session_id: SessionId,
        user_id: UserId,
        metric: String,
        value: f64,
        threshold: f64,
        timestamp: DateTime<Utc>,
    },
    NodeHealthChanged {
        node_id: MediaNodeId,
        healthy: bool,
        timestamp: DateTime<Utc>,
    },
    /// A tenant's webhook subscriptions changed; every node drops its cached copy. Internal,
    /// never exported.
    WebhooksChanged { app_id: AppId },
    ModerationEvent {
        app_id: AppId,
        event_type: String,
        target_user_id: UserId,
        details: serde_json::Value,
        timestamp: DateTime<Utc>,
    },
    RecordingStarted {
        app_id: AppId,
        channel_id: ChannelId,
        recording_id: uuid::Uuid,
        timestamp: DateTime<Utc>,
    },
    RecordingStopped {
        app_id: AppId,
        channel_id: ChannelId,
        recording_id: uuid::Uuid,
        duration_secs: f64,
        timestamp: DateTime<Utc>,
    },
    RecordingConsentRequired {
        app_id: AppId,
        channel_id: ChannelId,
        recording_id: uuid::Uuid,
        initiated_by: UserId,
        timestamp: DateTime<Utc>,
    },
    /// A live audio stream (WebSocket pull or push) of a channel was opened on a node.
    LiveStreamStarted {
        app_id: AppId,
        channel_id: ChannelId,
        stream_id: uuid::Uuid,
        mode: String,
        format: String,
        users: Option<Vec<UserId>>,
        timestamp: DateTime<Utc>,
    },
    LiveStreamStopped {
        app_id: AppId,
        channel_id: ChannelId,
        stream_id: uuid::Uuid,
        reason: String,
        duration_secs: f64,
        frames_sent: u64,
        frames_dropped: u64,
        timestamp: DateTime<Utc>,
    },
    /// A participant answered a recording/stream consent prompt on a node that does not host
    /// the capture (cascaded channel); the hosting node applies it. Node-scoped.
    RecordingConsentGiven {
        app_id: AppId,
        recording_id: uuid::Uuid,
        user_id: UserId,
        consent: aurix_common::types::RecordingConsent,
    },
    /// Persistent cross-mute between two players changed; every node applies it to the live
    /// sessions of both parties.
    UserBlockChanged {
        app_id: AppId,
        user_id: UserId,
        blocked_user_id: UserId,
        blocked: bool,
        timestamp: DateTime<Utc>,
    },
    /// Text chat message accepted by the origin node (already filtered and, if enabled,
    /// stored). Every node delivers it to its local recipients: channel members for channel
    /// messages, the target user's sessions for directed ones, plus the sender's own session.
    ChatMessage {
        app_id: AppId,
        message: aurix_common::protocol::ChatMessage,
        /// Session that sent it (`None` for REST/system messages); receives the echo with
        /// `client_ref` and is excluded from nothing else.
        from_session_id: Option<SessionId>,
    },
    ParticipantTyping {
        app_id: AppId,
        channel_id: ChannelId,
        user_id: UserId,
        session_id: SessionId,
        typing: bool,
    },
    /// Voice activity edge detected by the media node hosting the participant.
    ParticipantSpeaking {
        app_id: AppId,
        channel_id: ChannelId,
        user_id: UserId,
        speaking: bool,
    },
    /// Periodic audio levels (`0.0..=1.0`) of channel members hosted on the origin node whose
    /// level changed since the previous report.
    ChannelEnergy {
        app_id: AppId,
        channel_id: ChannelId,
        levels: Vec<aurix_common::protocol::ParticipantEnergy>,
    },
    /// Speech-to-text segment produced by the media node hosting the speaker, for a channel
    /// with `transcription` enabled. Every node delivers it to its local channel members that
    /// opted in; it is not stored.
    Transcript {
        app_id: AppId,
        transcript: aurix_common::protocol::Transcript,
    },
    /// Operator announcement accepted by the REST API; every node with local participants of
    /// the channel synthesizes and plays it to them.
    TtsAnnouncement {
        app_id: AppId,
        channel_id: ChannelId,
        request_id: uuid::Uuid,
        text: String,
        voice: String,
    },
    /// Progress of a text-to-speech request on the node playing it (`session_id` is the
    /// requesting player, `None` for announcements).
    TtsStatus {
        app_id: AppId,
        channel_id: ChannelId,
        request_id: uuid::Uuid,
        session_id: Option<SessionId>,
        user_id: Option<UserId>,
        client_ref: Option<String>,
        state: aurix_common::protocol::TtsState,
        duration_ms: Option<u64>,
        message: Option<String>,
    },
}

impl ServerEvent {
    /// Tenant the event belongs to (`None` for node-scoped events).
    pub fn app_id(&self) -> Option<AppId> {
        match self {
            Self::ParticipantJoined { app_id, .. }
            | Self::ParticipantLeft { app_id, .. }
            | Self::ChannelCreated { app_id, .. }
            | Self::ChannelDestroyed { app_id, .. }
            | Self::ChannelActivated { app_id, .. }
            | Self::ChannelDeactivated { app_id, .. }
            | Self::UserMuted { app_id, .. }
            | Self::UserUnmuted { app_id, .. }
            | Self::UserBanned { app_id, .. }
            | Self::UserDeleted { app_id, .. }
            | Self::UserKicked { app_id, .. }
            | Self::QualityAlert { app_id, .. }
            | Self::ModerationEvent { app_id, .. }
            | Self::RecordingStarted { app_id, .. }
            | Self::RecordingStopped { app_id, .. }
            | Self::RecordingConsentRequired { app_id, .. }
            | Self::LiveStreamStarted { app_id, .. }
            | Self::LiveStreamStopped { app_id, .. }
            | Self::RecordingConsentGiven { app_id, .. }
            | Self::UserBlockChanged { app_id, .. }
            | Self::ChatMessage { app_id, .. }
            | Self::ParticipantTyping { app_id, .. }
            | Self::ParticipantSpeaking { app_id, .. }
            | Self::ChannelEnergy { app_id, .. }
            | Self::Transcript { app_id, .. }
            | Self::TtsAnnouncement { app_id, .. }
            | Self::TtsStatus { app_id, .. } => Some(*app_id),
            Self::NodeHealthChanged { .. } | Self::WebhooksChanged { .. } => None,
        }
    }

    /// High-frequency client UX signals that are not meaningful to a backend event consumer.
    pub fn is_realtime_noise(&self) -> bool {
        matches!(
            self,
            Self::ParticipantTyping { .. }
                | Self::ParticipantSpeaking { .. }
                | Self::ChannelEnergy { .. }
        )
    }

    /// Public dotted name used by webhooks and the SSE stream (`participant.joined`, …).
    /// Node-scoped events have no public name and are never exported.
    pub fn public_type(&self) -> Option<&'static str> {
        Some(match self {
            Self::ParticipantJoined { .. } => "participant.joined",
            Self::ParticipantLeft { .. } => "participant.left",
            Self::ChannelCreated { .. } => "channel.created",
            Self::ChannelDestroyed { .. } => "channel.destroyed",
            Self::ChannelActivated { .. } => "channel.activated",
            Self::ChannelDeactivated { .. } => "channel.deactivated",
            Self::UserMuted { .. } => "participant.muted",
            Self::UserUnmuted { .. } => "participant.unmuted",
            Self::UserBanned { .. } => "user.banned",
            Self::UserDeleted { .. } => "user.deleted",
            Self::UserKicked { .. } => "participant.kicked",
            Self::QualityAlert { .. } => "quality.alert",
            Self::ModerationEvent { .. } => "moderation.event",
            Self::RecordingStarted { .. } => "recording.started",
            Self::RecordingStopped { .. } => "recording.stopped",
            Self::RecordingConsentRequired { .. } => "recording.consent_required",
            Self::LiveStreamStarted { .. } => "audio_stream.started",
            Self::LiveStreamStopped { .. } => "audio_stream.stopped",
            Self::UserBlockChanged { .. } => "user.block_changed",
            Self::ChatMessage { .. } => "chat.message",
            Self::ParticipantTyping { .. } => "participant.typing",
            Self::ParticipantSpeaking { .. } => "participant.speaking",
            Self::ChannelEnergy { .. } => "channel.energy",
            Self::Transcript { .. } => "channel.transcript",
            Self::TtsStatus { .. } => "tts.status",
            Self::NodeHealthChanged { .. }
            | Self::WebhooksChanged { .. }
            | Self::RecordingConsentGiven { .. }
            | Self::TtsAnnouncement { .. } => return None,
        })
    }

    /// Every exportable event type, in the order shown by `GET /v1/webhooks/events`.
    pub const PUBLIC_TYPES: &'static [&'static str] = &[
        "channel.created",
        "channel.destroyed",
        "channel.activated",
        "channel.deactivated",
        "participant.joined",
        "participant.left",
        "participant.muted",
        "participant.unmuted",
        "participant.kicked",
        "user.banned",
        "user.deleted",
        "user.block_changed",
        "moderation.event",
        "recording.started",
        "recording.stopped",
        "recording.consent_required",
        "audio_stream.started",
        "audio_stream.stopped",
        "quality.alert",
        "chat.message",
        "participant.typing",
        "participant.speaking",
        "channel.energy",
        "channel.transcript",
        "tts.status",
    ];

    /// Types a subscription may name (the real-time noise is SSE-only).
    pub fn webhook_type_allowed(ty: &str) -> bool {
        ty == "*"
            || Self::PUBLIC_TYPES.contains(&ty)
                && !matches!(
                    ty,
                    "participant.typing" | "participant.speaking" | "channel.energy"
                )
    }

    /// Tenant-facing JSON body (`payload` of the tagged representation, without `app_id`
    /// which travels in the envelope).
    pub fn public_data(&self) -> serde_json::Value {
        let mut v = serde_json::to_value(self).unwrap_or(serde_json::Value::Null);
        match v.get_mut("payload").map(serde_json::Value::take) {
            Some(mut payload) => {
                if let Some(obj) = payload.as_object_mut() {
                    obj.remove("app_id");
                }
                payload
            }
            None => serde_json::Value::Null,
        }
    }
}

/// Wire format for cross-node event fan-out. `origin` lets receivers drop their own
/// republished events; `id` allows deduplication if a message is delivered twice.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EventEnvelope {
    pub id: uuid::Uuid,
    pub origin: MediaNodeId,
    pub event: ServerEvent,
}

/// Process-local event bus.
///
/// `publish` is for events that originate on this node: they are delivered to local
/// subscribers *and* queued on the `outbound` channel for cross-node replication.
/// `deliver_remote` is for events received from other nodes: local delivery only, so a
/// local→Redis→local loop cannot form.
#[derive(Clone)]
pub struct EventBus {
    local: broadcast::Sender<ServerEvent>,
    outbound: broadcast::Sender<ServerEvent>,
}

impl EventBus {
    pub fn new(capacity: usize) -> Self {
        let (local, _) = broadcast::channel(capacity);
        let (outbound, _) = broadcast::channel(capacity);
        Self { local, outbound }
    }

    pub fn publish(&self, event: ServerEvent) {
        let _ = self.outbound.send(event.clone());
        if self.local.send(event).is_err() {
            warn!("No local event subscribers, event dropped");
        }
    }

    pub fn deliver_remote(&self, event: ServerEvent) {
        let _ = self.local.send(event);
    }

    pub fn subscribe(&self) -> broadcast::Receiver<ServerEvent> {
        self.local.subscribe()
    }

    /// Events originating on this node that should be replicated to other nodes.
    pub fn subscribe_outbound(&self) -> broadcast::Receiver<ServerEvent> {
        self.outbound.subscribe()
    }

    pub fn subscriber_count(&self) -> usize {
        self.local.receiver_count()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn remote_events_do_not_reenter_outbound() {
        let bus = EventBus::new(8);
        let mut local = bus.subscribe();
        let mut outbound = bus.subscribe_outbound();
        let ev = ServerEvent::NodeHealthChanged {
            node_id: MediaNodeId::new(),
            healthy: true,
            timestamp: Utc::now(),
        };
        bus.deliver_remote(ev.clone());
        assert!(local.try_recv().is_ok());
        assert!(outbound.try_recv().is_err());

        bus.publish(ev);
        assert!(local.try_recv().is_ok());
        assert!(outbound.try_recv().is_ok());
    }
}
