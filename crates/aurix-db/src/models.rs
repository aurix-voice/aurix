use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::FromRow;
use uuid::Uuid;

#[derive(Debug, Clone, FromRow, Serialize, Deserialize)]
pub struct AppRow {
    pub id: Uuid,
    pub name: String,
    pub description: Option<String>,
    pub owner_id: Uuid,
    pub api_key_hash: String,
    pub api_secret_hash: String,
    pub active: bool,
    pub max_channels: i32,
    pub max_participants_per_channel: i32,
    /// Fleet-wide cap on simultaneously connected sessions; `0` = unlimited.
    pub max_concurrent_sessions: i32,
    /// Cap on channel participant-minutes per calendar month (UTC); `0` = unlimited.
    pub monthly_participant_minutes: i64,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, FromRow, Serialize, Deserialize)]
pub struct UserRow {
    pub id: Uuid,
    pub app_id: Uuid,
    pub external_id: String,
    pub display_name: String,
    pub metadata: Option<serde_json::Value>,
    pub is_banned: bool,
    pub ban_reason: Option<String>,
    pub ban_expires_at: Option<DateTime<Utc>>,
    pub device_ids: Vec<String>,
    pub total_session_minutes: i64,
    pub last_seen_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, FromRow, Serialize, Deserialize)]
pub struct ChannelRow {
    pub id: Uuid,
    pub app_id: Uuid,
    pub name: String,
    pub channel_type: String,
    pub config: serde_json::Value,
    pub max_participants: i32,
    pub is_persistent: bool,
    pub ad_hoc: bool,
    pub active_participants: i32,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub deleted_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, FromRow, Serialize, Deserialize)]
pub struct SessionRow {
    pub id: Uuid,
    pub user_id: Uuid,
    pub app_id: Uuid,
    pub media_node_id: Uuid,
    pub ip_address: String,
    pub user_agent: Option<String>,
    pub connected_at: DateTime<Utc>,
    pub disconnected_at: Option<DateTime<Utc>>,
    pub disconnect_reason: Option<String>,
    pub quality_stats: Option<serde_json::Value>,
}

#[derive(Debug, Clone, FromRow, Serialize, Deserialize)]
pub struct ChannelMembershipRow {
    pub id: Uuid,
    pub channel_id: Uuid,
    pub user_id: Uuid,
    pub session_id: Uuid,
    pub role: String,
    pub is_muted: bool,
    pub is_server_muted: bool,
    pub ssrc: i64,
    pub joined_at: DateTime<Utc>,
    pub left_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, FromRow, Serialize, Deserialize)]
pub struct ChannelRosterRow {
    pub user_id: Uuid,
    pub session_id: Uuid,
    pub media_node_id: Uuid,
    pub display_name: String,
    pub role: String,
    pub is_muted: bool,
    pub is_server_muted: bool,
    pub ssrc: i64,
}

/// A membership closed by the lost-node reaper (see `queries::close_memberships_for_lost_node`).
#[derive(Debug, Clone, FromRow)]
pub struct LostMembershipRow {
    pub app_id: Uuid,
    pub channel_id: Uuid,
    pub user_id: Uuid,
    pub session_id: Uuid,
    pub role: String,
}

#[derive(Debug, Clone, FromRow, Serialize, Deserialize)]
pub struct BanRow {
    pub id: Uuid,
    pub app_id: Uuid,
    pub user_id: Option<Uuid>,
    pub device_id: Option<String>,
    pub ip_address: Option<String>,
    pub scope: String,
    pub reason: String,
    pub issued_by: Uuid,
    pub expires_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
    pub revoked_at: Option<DateTime<Utc>>,
    pub revoked_by: Option<Uuid>,
}

#[derive(Debug, Clone, FromRow, Serialize, Deserialize)]
pub struct ModerationEventRow {
    pub id: Uuid,
    pub app_id: Uuid,
    pub channel_id: Option<Uuid>,
    pub target_user_id: Uuid,
    pub reporter_user_id: Option<Uuid>,
    pub moderator_user_id: Option<Uuid>,
    pub event_type: String,
    pub reason: String,
    pub evidence: Option<serde_json::Value>,
    pub recording_id: Option<Uuid>,
    pub status: String,
    pub resolution: Option<String>,
    pub created_at: DateTime<Utc>,
    pub resolved_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, FromRow, Serialize, Deserialize)]
pub struct RecordingRow {
    pub id: Uuid,
    pub app_id: Uuid,
    pub channel_id: Uuid,
    pub session_id: Uuid,
    pub user_id: Uuid,
    pub file_path: String,
    pub file_size_bytes: i64,
    pub duration_secs: f64,
    pub format: String,
    pub encrypted: bool,
    pub encryption_key_id: Option<String>,
    pub started_at: DateTime<Utc>,
    pub ended_at: Option<DateTime<Utc>>,
    pub expires_at: DateTime<Utc>,
    pub created_at: DateTime<Utc>,
    /// `recording` for operator-started captures, `evidence` for safety clips, `mixdown` for
    /// channel renders derived from tracks.
    pub kind: String,
    /// `recording` (still capturing), `processing` (mixdown being rendered), `ready`, `failed`.
    pub status: String,
    /// When the first audio was written; consent can delay it past `started_at`.
    pub audio_started_at: Option<DateTime<Utc>>,
    /// Tracks a mixdown was rendered from.
    pub sources: Option<Vec<Uuid>>,
    /// Node holding the file (and running the job while `processing`).
    pub node_id: Option<Uuid>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, FromRow, Serialize, Deserialize)]
pub struct RecordingTranscriptRow {
    pub recording_id: Uuid,
    pub app_id: Uuid,
    /// `queued`, `running`, `ready`, `failed`.
    pub status: String,
    pub provider: Option<String>,
    pub language: Option<String>,
    pub text: String,
    pub segments: serde_json::Value,
    pub duration_ms: i64,
    pub node_id: Option<Uuid>,
    pub error: Option<String>,
    pub requested_at: DateTime<Utc>,
    pub finished_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, FromRow, Serialize, Deserialize)]
pub struct ChatMessageRow {
    pub id: Uuid,
    pub app_id: Uuid,
    pub channel_id: Option<Uuid>,
    pub from_user_id: Uuid,
    pub display_name: String,
    pub to_user_id: Option<Uuid>,
    pub text: String,
    pub metadata: Option<serde_json::Value>,
    pub sent_at: DateTime<Utc>,
    /// Directed message accepted while the recipient had no active session.
    pub offline: bool,
}

/// Position in a `(sent_at, id)`-ordered message stream; both cursor bounds of a history page
/// and the inbox/read positions are of this shape.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MessageCursor {
    pub sent_at: DateTime<Utc>,
    pub id: Uuid,
}

impl MessageCursor {
    pub fn of(m: &ChatMessageRow) -> Self {
        Self {
            sent_at: m.sent_at,
            id: m.id,
        }
    }
}

/// Which stored conversation a read marker or history page refers to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChatConversation {
    Channel(Uuid),
    /// Directed messages between the acting user and this peer (both directions).
    Direct(Uuid),
}

impl ChatConversation {
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Channel(_) => "channel",
            Self::Direct(_) => "direct",
        }
    }

    pub fn id(&self) -> Uuid {
        match self {
            Self::Channel(id) | Self::Direct(id) => *id,
        }
    }
}

#[derive(Debug, Clone, FromRow, Serialize, Deserialize)]
pub struct ChatReadMarkerRow {
    pub app_id: Uuid,
    pub user_id: Uuid,
    pub kind: String,
    pub conversation_id: Uuid,
    pub message_id: Uuid,
    pub message_sent_at: DateTime<Utc>,
    pub read_at: DateTime<Utc>,
}

#[derive(Debug, Clone, FromRow, Serialize, Deserialize)]
pub struct WebhookSubscriptionRow {
    pub id: Uuid,
    pub app_id: Uuid,
    pub url: String,
    /// HMAC-SHA256 signing key; never serialized into API responses (see `WebhookView`).
    #[serde(skip_serializing)]
    pub secret: String,
    pub events: Vec<String>,
    pub description: Option<String>,
    pub enabled: bool,
    pub consecutive_failures: i32,
    pub last_delivery_at: Option<DateTime<Utc>>,
    pub last_status: Option<i16>,
    pub last_error: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl WebhookSubscriptionRow {
    /// `true` when the subscription wants `event_type` (`"*"` matches everything).
    pub fn wants(&self, event_type: &str) -> bool {
        self.events.iter().any(|e| e == "*" || e == event_type)
    }
}

#[derive(Debug, Clone, FromRow, Serialize, Deserialize)]
pub struct WebhookDeliveryRow {
    pub id: Uuid,
    pub subscription_id: Uuid,
    pub app_id: Uuid,
    pub event_id: Uuid,
    pub event_type: String,
    pub payload: serde_json::Value,
    pub status: String,
    pub attempts: i32,
    pub next_attempt_at: DateTime<Utc>,
    pub leased_until: Option<DateTime<Utc>>,
    pub last_status: Option<i16>,
    pub last_error: Option<String>,
    pub created_at: DateTime<Utc>,
    pub delivered_at: Option<DateTime<Utc>>,
}

/// One open channel membership joined with its channel type and the member's display name.
#[derive(Debug, Clone, FromRow, Serialize, Deserialize)]
pub struct ActiveMemberRow {
    pub channel_id: Uuid,
    pub channel_type: String,
    pub user_id: Uuid,
    pub display_name: String,
    pub session_id: Uuid,
    pub role: String,
    pub is_muted: bool,
    pub is_server_muted: bool,
    pub ssrc: i64,
    pub joined_at: DateTime<Utc>,
}

#[derive(Debug, Clone, FromRow, Serialize, Deserialize)]
pub struct MediaNodeRow {
    pub id: Uuid,
    pub region: String,
    pub address: String,
    #[sqlx(default)]
    pub address_ipv6: Option<String>,
    pub media_port: i32,
    pub api_port: i32,
    pub cascade_port: Option<i32>,
    pub ws_url: Option<String>,
    pub api_url: Option<String>,
    pub latitude: Option<f64>,
    pub longitude: Option<f64>,
    pub capacity: i32,
    pub active_channels: i32,
    pub active_participants: i32,
    pub cpu_usage: f64,
    pub memory_usage: f64,
    pub bandwidth_in_mbps: f64,
    pub bandwidth_out_mbps: f64,
    pub healthy: bool,
    /// Pure inter-regional cascade hub: hosts no clients, only forwards relay traffic.
    #[sqlx(default)]
    pub relay_only: bool,
    pub version: String,
    pub last_heartbeat: DateTime<Utc>,
    pub registered_at: DateTime<Utc>,
}

#[derive(Debug, Clone, FromRow, Serialize, Deserialize)]
pub struct AuditLogRow {
    pub id: Uuid,
    pub app_id: Option<Uuid>,
    pub actor_id: Uuid,
    pub action: String,
    pub target_type: String,
    pub target_id: String,
    pub details: serde_json::Value,
    pub ip_address: Option<String>,
    pub previous_hash: String,
    pub hash: String,
    pub created_at: DateTime<Utc>,
}

/// Rows removed by a user erasure, per table.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct UserErasureCounts {
    pub channel_memberships: u64,
    pub sessions: u64,
    pub chat_messages: u64,
    pub user_blocks: u64,
    /// Rows the recording service had not already removed together with their files.
    pub recordings: u64,
    pub moderation_events: u64,
    pub bans: u64,
    pub users: u64,
}

#[derive(Debug, Clone, FromRow, Serialize, Deserialize)]
pub struct ApiKeyRow {
    pub id: Uuid,
    pub app_id: Uuid,
    pub name: String,
    pub key_prefix: String,
    pub key_hash: String,
    pub permissions: serde_json::Value,
    pub rate_limit: i32,
    pub active: bool,
    pub last_used_at: Option<DateTime<Utc>>,
    pub expires_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
    pub revoked_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, FromRow, Serialize, Deserialize)]
pub struct AdminUserRow {
    pub id: Uuid,
    pub email: String,
    pub password_hash: String,
    pub display_name: String,
    pub role: String,
    pub active: bool,
    pub last_login_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub auth_source: String,
    pub sso_issuer: Option<String>,
    pub sso_subject: Option<String>,
    pub token_generation: i64,
    pub tokens_revoked_at: Option<DateTime<Utc>>,
}
