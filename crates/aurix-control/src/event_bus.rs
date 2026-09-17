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
}

#[derive(Clone)]
pub struct EventBus {
    sender: broadcast::Sender<ServerEvent>,
}

impl EventBus {
    pub fn new(capacity: usize) -> Self {
        let (sender, _) = broadcast::channel(capacity);
        Self { sender }
    }

    pub fn publish(&self, event: ServerEvent) {
        if let Err(e) = self.sender.send(event) {
            warn!("No event subscribers, event dropped");
            let _ = e;
        }
    }

    pub fn subscribe(&self) -> broadcast::Receiver<ServerEvent> {
        self.sender.subscribe()
    }

    pub fn subscriber_count(&self) -> usize {
        self.sender.receiver_count()
    }
}