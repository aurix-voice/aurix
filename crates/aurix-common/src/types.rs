use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;
use std::collections::HashMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct UserId(pub Uuid);

impl UserId {
    pub fn new() -> Self { Self(Uuid::now_v7()) }
    pub fn from_uuid(u: Uuid) -> Self { Self(u) }
}

impl std::fmt::Display for UserId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl Default for UserId {
    fn default() -> Self { Self::new() }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ChannelId(pub Uuid);

impl ChannelId {
    pub fn new() -> Self { Self(Uuid::now_v7()) }
    pub fn from_uuid(u: Uuid) -> Self { Self(u) }
}

impl std::fmt::Display for ChannelId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SessionId(pub Uuid);

impl SessionId {
    pub fn new() -> Self { Self(Uuid::now_v7()) }
    pub fn from_uuid(u: Uuid) -> Self { Self(u) }
}

impl std::fmt::Display for SessionId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct MediaNodeId(pub Uuid);

impl MediaNodeId {
    pub fn new() -> Self { Self(Uuid::now_v7()) }
    pub fn from_uuid(u: Uuid) -> Self { Self(u) }
}

impl std::fmt::Display for MediaNodeId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct AppId(pub Uuid);

impl AppId {
    pub fn new() -> Self { Self(Uuid::now_v7()) }
    pub fn from_uuid(u: Uuid) -> Self { Self(u) }
}

impl std::fmt::Display for AppId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChannelType {
    Positional,
    Team,
    Command,
    Whisper,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChannelRole {
    Listener,
    Speaker,
    Moderator,
    Administrator,
}

impl ChannelRole {
    pub fn can_speak(&self) -> bool {
        matches!(self, Self::Speaker | Self::Moderator | Self::Administrator)
    }

    pub fn can_moderate(&self) -> bool {
        matches!(self, Self::Moderator | Self::Administrator)
    }

    pub fn can_administrate(&self) -> bool {
        matches!(self, Self::Administrator)
    }

    pub fn precedence(&self) -> u8 {
        match self {
            Self::Listener => 0,
            Self::Speaker => 1,
            Self::Moderator => 2,
            Self::Administrator => 3,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConnectionState {
    Connecting,
    Connected,
    Reconnecting,
    Disconnected,
    Failed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AudioCodec {
    Opus,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BanScope {
    Account,
    Device,
    IpAddress,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BanDuration {
    Temporary,
    Permanent,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MuteScope {
    Local,
    Server,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Region {
    UsEast,
    UsWest,
    EuWest,
    EuCentral,
    AsiaPacific,
    SouthAmerica,
    Australia,
    MiddleEast,
    Africa,
}

impl Region {
    pub fn all() -> &'static [Region] {
        &[
            Region::UsEast, Region::UsWest, Region::EuWest, Region::EuCentral,
            Region::AsiaPacific, Region::SouthAmerica, Region::Australia,
            Region::MiddleEast, Region::Africa,
        ]
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Self::UsEast => "us-east",
            Self::UsWest => "us-west",
            Self::EuWest => "eu-west",
            Self::EuCentral => "eu-central",
            Self::AsiaPacific => "asia-pacific",
            Self::SouthAmerica => "south-america",
            Self::Australia => "australia",
            Self::MiddleEast => "middle-east",
            Self::Africa => "africa",
        }
    }

    pub fn from_str_loose(s: &str) -> Self {
        match s.to_lowercase().replace('-', "_").as_str() {
            "us_east" | "useast" => Self::UsEast,
            "us_west" | "uswest" => Self::UsWest,
            "eu_west" | "euwest" => Self::EuWest,
            "eu_central" | "eucentral" => Self::EuCentral,
            "asia_pacific" | "asiapacific" => Self::AsiaPacific,
            "south_america" | "southamerica" => Self::SouthAmerica,
            "australia" => Self::Australia,
            "middle_east" | "middleeast" => Self::MiddleEast,
            "africa" => Self::Africa,
            _ => Self::UsEast,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Position3D {
    pub x: f32,
    pub y: f32,
    pub z: f32,
}

impl Position3D {
    pub fn new(x: f32, y: f32, z: f32) -> Self { Self { x, y, z } }
    pub fn zero() -> Self { Self { x: 0.0, y: 0.0, z: 0.0 } }

    pub fn distance_to(&self, other: &Position3D) -> f32 {
        let dx = self.x - other.x;
        let dy = self.y - other.y;
        let dz = self.z - other.z;
        (dx * dx + dy * dy + dz * dz).sqrt()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Orientation3D {
    pub forward_x: f32,
    pub forward_y: f32,
    pub forward_z: f32,
    pub up_x: f32,
    pub up_y: f32,
    pub up_z: f32,
}

impl Default for Orientation3D {
    fn default() -> Self {
        Self {
            forward_x: 0.0, forward_y: 0.0, forward_z: 1.0,
            up_x: 0.0, up_y: 1.0, up_z: 0.0,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChannelConfig {
    pub channel_type: ChannelType,
    pub max_participants: u32,
    pub codec: AudioCodec,
    pub bitrate: u32,
    pub sample_rate: u32,
    pub enable_dtx: bool,
    pub enable_fec: bool,
    pub positional_config: Option<PositionalConfig>,
    pub audio_profile: AudioProfile,
    pub recording_enabled: bool,
    pub whisper_target: Option<UserId>,
    pub command_speakers: Option<Vec<UserId>>,
}

impl Default for ChannelConfig {
    fn default() -> Self {
        Self {
            channel_type: ChannelType::Team,
            max_participants: 256,
            codec: AudioCodec::Opus,
            bitrate: 48000,
            sample_rate: 48000,
            enable_dtx: true,
            enable_fec: true,
            positional_config: None,
            audio_profile: AudioProfile::Voice,
            recording_enabled: false,
            whisper_target: None,
            command_speakers: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PositionalConfig {
    pub near_distance: f32,
    pub far_distance: f32,
    pub rolloff: RolloffCurve,
    pub max_radius: f32,
}

impl Default for PositionalConfig {
    fn default() -> Self {
        Self {
            near_distance: 1.0,
            far_distance: 50.0,
            rolloff: RolloffCurve::Logarithmic,
            max_radius: 100.0,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RolloffCurve {
    Linear,
    Logarithmic,
    CustomSpline,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AudioProfile {
    Voice,
    Music,
    Broadcast,
    LowBandwidth,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ParticipantInfo {
    pub user_id: UserId,
    pub session_id: SessionId,
    pub display_name: String,
    pub channel_id: ChannelId,
    pub role: ChannelRole,
    pub is_speaking: bool,
    pub is_muted: bool,
    pub is_server_muted: bool,
    pub volume_level: f32,
    pub position: Option<Position3D>,
    pub orientation: Option<Orientation3D>,
    pub joined_at: DateTime<Utc>,
    pub media_node_id: MediaNodeId,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MediaNodeInfo {
    pub id: MediaNodeId,
    pub region: Region,
    pub address: String,
    pub media_port: u16,
    pub api_port: u16,
    pub active_channels: u32,
    pub active_participants: u32,
    pub cpu_usage: f32,
    pub memory_usage: f32,
    pub bandwidth_in_mbps: f32,
    pub bandwidth_out_mbps: f32,
    pub healthy: bool,
    pub last_heartbeat: DateTime<Utc>,
    pub capacity: u32,
}

impl MediaNodeInfo {
    pub fn load_factor(&self) -> f32 {
        if self.capacity == 0 { return 1.0; }
        self.active_participants as f32 / self.capacity as f32
    }

    pub fn is_available(&self) -> bool {
        self.healthy && self.load_factor() < 0.9
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QualityMetrics {
    pub rtt_ms: f32,
    pub jitter_ms: f32,
    pub packet_loss_percent: f32,
    pub bitrate_kbps: u32,
    pub mos_score: f32,
}

impl QualityMetrics {
    pub fn calculate_mos(&self) -> f32 {
        let effective_latency = self.rtt_ms + self.jitter_ms * 2.0 + 10.0;
        let r = if effective_latency < 160.0 {
            93.2 - (effective_latency / 40.0)
        } else {
            93.2 - ((effective_latency - 120.0) / 10.0)
        };
        let r = r - (self.packet_loss_percent * 2.5);
        let r = r.max(0.0).min(100.0);
        1.0 + 0.035 * r + r * (r - 60.0) * (100.0 - r) * 7.0e-6
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AudioProcessingConfig {
    pub agc_enabled: bool,
    pub agc_target_level_dbfs: f32,
    pub noise_suppression_enabled: bool,
    pub noise_suppression_level: NoiseSuppressionLevel,
    pub aec_enabled: bool,
    pub aec_filter_length_ms: u32,
    pub vad_enabled: bool,
    pub vad_onset_threshold: f32,
    pub vad_offset_threshold: f32,
    pub vad_hold_time_ms: u32,
    pub jitter_buffer_min_ms: u32,
    pub jitter_buffer_max_ms: u32,
}

impl Default for AudioProcessingConfig {
    fn default() -> Self {
        Self {
            agc_enabled: true,
            agc_target_level_dbfs: -18.0,
            noise_suppression_enabled: true,
            noise_suppression_level: NoiseSuppressionLevel::High,
            aec_enabled: true,
            aec_filter_length_ms: 128,
            vad_enabled: true,
            vad_onset_threshold: 0.7,
            vad_offset_threshold: 0.3,
            vad_hold_time_ms: 300,
            jitter_buffer_min_ms: 20,
            jitter_buffer_max_ms: 200,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NoiseSuppressionLevel {
    Low,
    Medium,
    High,
    VeryHigh,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TokenClaims {
    pub sub: String,
    pub user_id: UserId,
    pub app_id: AppId,
    pub display_name: String,
    pub channels: Vec<ChannelPermission>,
    pub exp: i64,
    pub iat: i64,
    pub jti: String,
    pub metadata: Option<HashMap<String, serde_json::Value>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChannelPermission {
    pub channel_id: ChannelId,
    pub join: bool,
    pub speak: bool,
    pub receive: bool,
    pub moderate: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuditLogEntry {
    pub id: Uuid,
    pub timestamp: DateTime<Utc>,
    pub app_id: Option<AppId>,
    pub actor_id: UserId,
    pub action: AuditAction,
    pub target_type: String,
    pub target_id: String,
    pub details: serde_json::Value,
    pub ip_address: Option<String>,
    pub previous_hash: String,
    pub hash: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuditAction {
    UserBanned,
    UserUnbanned,
    UserMuted,
    UserUnmuted,
    ChannelCreated,
    ChannelDeleted,
    ChannelConfigUpdated,
    ParticipantKicked,
    RecordingStarted,
    RecordingStopped,
    ApiKeyCreated,
    ApiKeyRevoked,
    RoleChanged,
    ConfigUpdated,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PaginationParams {
    pub page: u32,
    pub per_page: u32,
}

impl Default for PaginationParams {
    fn default() -> Self { Self { page: 1, per_page: 50 } }
}

impl PaginationParams {
    pub fn offset(&self) -> u32 { (self.page.saturating_sub(1)) * self.per_page }
    pub fn limit(&self) -> u32 { self.per_page.min(200) }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PaginatedResponse<T> {
    pub data: Vec<T>,
    pub page: u32,
    pub per_page: u32,
    pub total: u64,
    pub total_pages: u64,
}

impl<T> PaginatedResponse<T> {
    pub fn new(data: Vec<T>, page: u32, per_page: u32, total: u64) -> Self {
        let total_pages = (total as f64 / per_page as f64).ceil() as u64;
        Self { data, page, per_page, total, total_pages }
    }
}

/// Consent state for recording notifications
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecordingConsent {
    Pending,
    Accepted,
    Declined,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReverbDescriptor {
    pub room_size: f32,
    pub decay_time: f32,
    pub wet_dry_mix: f32,
}

impl Default for ReverbDescriptor {
    fn default() -> Self {
        Self { room_size: 0.5, decay_time: 1.0, wet_dry_mix: 0.3 }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OcclusionInfo {
    pub source_user_id: UserId,
    pub factor: f32,
}

/// Admin authentication context injected by admin middleware.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AdminContext {
    pub admin_id: uuid::Uuid,
    pub email: String,
    pub role: String,
}