use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use uuid::Uuid;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct UserId(pub Uuid);

impl UserId {
    pub fn new() -> Self {
        Self(Uuid::now_v7())
    }
    pub fn from_uuid(u: Uuid) -> Self {
        Self(u)
    }
}

impl std::fmt::Display for UserId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl Default for UserId {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ChannelId(pub Uuid);

impl Default for ChannelId {
    fn default() -> Self {
        Self::new()
    }
}

impl ChannelId {
    pub fn new() -> Self {
        Self(Uuid::now_v7())
    }
    pub fn from_uuid(u: Uuid) -> Self {
        Self(u)
    }

    /// Deterministic id of an ad-hoc channel: the same `(app, name)` pair always maps to the
    /// same channel, so game servers can address it (mute-all, kick-all, participants) without
    /// a lookup and concurrent first joiners agree on one row.
    pub fn ad_hoc(app_id: AppId, name: &str) -> Self {
        use sha2::{Digest, Sha256};
        let mut h = Sha256::new();
        h.update(b"aurix.ad_hoc_channel.v1\0");
        h.update(app_id.0.as_bytes());
        h.update(b"\0");
        h.update(name.as_bytes());
        let digest = h.finalize();
        let mut bytes = [0u8; 16];
        bytes.copy_from_slice(&digest[..16]);
        Self(uuid::Builder::from_custom_bytes(bytes).into_uuid())
    }
}

impl std::fmt::Display for ChannelId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SessionId(pub Uuid);

impl Default for SessionId {
    fn default() -> Self {
        Self::new()
    }
}

impl SessionId {
    pub fn new() -> Self {
        Self(Uuid::now_v7())
    }
    pub fn from_uuid(u: Uuid) -> Self {
        Self(u)
    }
}

impl std::fmt::Display for SessionId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct MediaNodeId(pub Uuid);

impl Default for MediaNodeId {
    fn default() -> Self {
        Self::new()
    }
}

impl MediaNodeId {
    pub fn new() -> Self {
        Self(Uuid::now_v7())
    }
    pub fn from_uuid(u: Uuid) -> Self {
        Self(u)
    }
}

impl std::fmt::Display for MediaNodeId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct AppId(pub Uuid);

impl Default for AppId {
    fn default() -> Self {
        Self::new()
    }
}

impl AppId {
    pub fn new() -> Self {
        Self(Uuid::now_v7())
    }
    pub fn from_uuid(u: Uuid) -> Self {
        Self(u)
    }
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
    /// Microphone test: every participant hears only their own audio, routed through the
    /// server (so it exercises the real uplink + downlink path). Never relayed to other
    /// nodes.
    Echo,
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

/// Audio codec of a media stream. Channels always carry Opus; a native AURX session may
/// negotiate PCMU for its own uplink/downlink with `SetAudioCodec`, in which case the server
/// transcodes between the two.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum AudioCodec {
    #[default]
    Opus,
    /// ITU-T G.711 μ-law, 8 kHz mono, 64 kbit/s (see `aurix_common::g711`).
    Pcmu,
}

/// How a session's media reaches the node (`MediaBound.transport`, session stats).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum MediaTransportKind {
    /// Native AURX over UDP.
    #[default]
    Udp,
    /// Native AURX tunneled through the control WebSocket (UDP-blocked fallback).
    Tunnel,
    /// Browser WebRTC (str0m).
    WebRtc,
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
            Region::UsEast,
            Region::UsWest,
            Region::EuWest,
            Region::EuCentral,
            Region::AsiaPacific,
            Region::SouthAmerica,
            Region::Australia,
            Region::MiddleEast,
            Region::Africa,
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
    pub fn new(x: f32, y: f32, z: f32) -> Self {
        Self { x, y, z }
    }
    pub fn zero() -> Self {
        Self {
            x: 0.0,
            y: 0.0,
            z: 0.0,
        }
    }

    pub fn is_finite(&self) -> bool {
        self.x.is_finite() && self.y.is_finite() && self.z.is_finite()
    }

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

impl Orientation3D {
    pub fn is_finite(&self) -> bool {
        [
            self.forward_x,
            self.forward_y,
            self.forward_z,
            self.up_x,
            self.up_y,
            self.up_z,
        ]
        .iter()
        .all(|v| v.is_finite())
    }

    /// Orthonormal listener basis `(forward, up, right)`; `None` when `forward` is zero or
    /// `up` is parallel to it (the game sent a degenerate orientation).
    fn basis(&self, coords: CoordinateSystem) -> Option<([f32; 3], [f32; 3], [f32; 3])> {
        let f = normalize([self.forward_x, self.forward_y, self.forward_z])?;
        let up = [self.up_x, self.up_y, self.up_z];
        let d = dot(up, f);
        let u = normalize([up[0] - d * f[0], up[1] - d * f[1], up[2] - d * f[2]])?;
        let r = match coords {
            CoordinateSystem::LeftHanded => cross(u, f),
            CoordinateSystem::RightHanded => cross(f, u),
        };
        Some((f, u, r))
    }
}

/// Where a sound source sits relative to a listener, in the listener's own frame.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Direction {
    /// Radians in `-π..=π`: `0` straight ahead, positive to the listener's right, `±π` behind.
    pub azimuth: f32,
    /// Radians in `-π/2..=π/2`: positive above the listener.
    pub elevation: f32,
}

impl Direction {
    pub const AHEAD: Direction = Direction {
        azimuth: 0.0,
        elevation: 0.0,
    };

    /// Direction from `listener` (facing `orientation`) to `source`. `None` when they share a
    /// position (no direction exists) or the orientation is degenerate.
    pub fn from_listener(
        listener: &Position3D,
        orientation: &Orientation3D,
        source: &Position3D,
        coords: CoordinateSystem,
    ) -> Option<Direction> {
        let (f, u, r) = orientation.basis(coords)?;
        let d = normalize([
            source.x - listener.x,
            source.y - listener.y,
            source.z - listener.z,
        ])?;
        let azimuth = dot(d, r).atan2(dot(d, f));
        let elevation = dot(d, u).clamp(-1.0, 1.0).asin();
        Some(Direction { azimuth, elevation })
    }

    /// Constant-power stereo gains `(left, right)` for a source at this direction, normalised
    /// so a centred source is unity in both channels (a hard-panned one is +3 dB in one ear and
    /// silent in the other). Stereo has no front/back cue, so a source behind the listener pans
    /// like one in front of it.
    pub fn stereo_gains(&self) -> (f32, f32) {
        let pan = self.azimuth.sin().clamp(-1.0, 1.0);
        let theta = (pan + 1.0) * std::f32::consts::FRAC_PI_4;
        (
            theta.cos() * std::f32::consts::SQRT_2,
            theta.sin() * std::f32::consts::SQRT_2,
        )
    }
}

fn dot(a: [f32; 3], b: [f32; 3]) -> f32 {
    a[0] * b[0] + a[1] * b[1] + a[2] * b[2]
}

fn cross(a: [f32; 3], b: [f32; 3]) -> [f32; 3] {
    [
        a[1] * b[2] - a[2] * b[1],
        a[2] * b[0] - a[0] * b[2],
        a[0] * b[1] - a[1] * b[0],
    ]
}

fn normalize(v: [f32; 3]) -> Option<[f32; 3]> {
    let len = dot(v, v).sqrt();
    if !len.is_finite() || len <= 1e-6 {
        return None;
    }
    Some([v[0] / len, v[1] / len, v[2] / len])
}

impl Default for Orientation3D {
    fn default() -> Self {
        Self {
            forward_x: 0.0,
            forward_y: 0.0,
            forward_z: 1.0,
            up_x: 0.0,
            up_y: 1.0,
            up_z: 0.0,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ChannelConfig {
    pub channel_type: ChannelType,
    pub max_participants: u32,
    pub codec: AudioCodec,
    /// Target uplink bitrate participants encode at (bit/s); also the ceiling the server's
    /// bitrate adaptation returns to.
    pub bitrate: u32,
    /// Floor for server-driven bitrate adaptation (bit/s).
    pub min_bitrate: u32,
    pub sample_rate: u32,
    pub enable_dtx: bool,
    pub enable_fec: bool,
    /// Widest audio bandwidth participants may encode.
    pub max_bandwidth: OpusBandwidth,
    /// Encoder complexity hint `0..=10` (`None`: the client's default).
    pub complexity: Option<u8>,
    pub positional_config: Option<PositionalConfig>,
    pub audio_profile: AudioProfile,
    /// Cocktail-party mixing: each receiver hears at most `max_voices` speakers at their
    /// computed gain, every other concurrent speaker at `ambient_gain` (`0` drops them).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ambient: Option<AmbientConfig>,
    pub recording_enabled: bool,
    /// Transcribe participants' speech (when `[stt]` is configured on the node) and deliver
    /// `Transcript` events to the channel's participants.
    pub transcription: bool,
    /// Run participants' speech through the node's `[safety]` pipeline (transcribe →
    /// classify → incidents). Independent of `transcription`: transcripts are not delivered to
    /// participants unless that is set too. Never applies to end-to-end encrypted media.
    pub safety_voice: bool,
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
            min_bitrate: 12000,
            sample_rate: 48000,
            enable_dtx: true,
            enable_fec: true,
            max_bandwidth: OpusBandwidth::Fullband,
            complexity: None,
            positional_config: None,
            audio_profile: AudioProfile::Voice,
            ambient: None,
            recording_enabled: false,
            transcription: false,
            safety_voice: false,
            whisper_target: None,
            command_speakers: None,
        }
    }
}

impl ChannelConfig {
    /// Opus bitrate range (RFC 6716).
    pub const MIN_OPUS_BITRATE: u32 = 6_000;
    pub const MAX_OPUS_BITRATE: u32 = 510_000;

    /// Rejects values a client could not honour. `bitrate_cap` is the node's
    /// `media.max_bitrate`.
    pub fn validate(&self, bitrate_cap: u32) -> std::result::Result<(), String> {
        if self.max_participants == 0 {
            return Err("max_participants must be > 0".into());
        }
        if self.codec != AudioCodec::Opus {
            return Err(
                "codec must be opus; PCMU is negotiated per session with SetAudioCodec".into(),
            );
        }
        let cap = bitrate_cap.clamp(Self::MIN_OPUS_BITRATE, Self::MAX_OPUS_BITRATE);
        if !(Self::MIN_OPUS_BITRATE..=cap).contains(&self.bitrate) {
            return Err(format!(
                "bitrate must be within {}..={} bit/s",
                Self::MIN_OPUS_BITRATE,
                cap
            ));
        }
        if !(Self::MIN_OPUS_BITRATE..=self.bitrate).contains(&self.min_bitrate) {
            return Err(format!(
                "min_bitrate must be within {}..=bitrate ({})",
                Self::MIN_OPUS_BITRATE,
                self.bitrate
            ));
        }
        if !matches!(self.sample_rate, 8_000 | 12_000 | 16_000 | 24_000 | 48_000) {
            return Err("sample_rate must be one of 8000, 12000, 16000, 24000, 48000".into());
        }
        if self.complexity.is_some_and(|c| c > 10) {
            return Err("complexity must be within 0..=10".into());
        }
        if let Some(p) = &self.positional_config {
            p.validate()?;
        }
        if let Some(a) = &self.ambient {
            a.validate()?;
        }
        Ok(())
    }

    /// The encoder policy this channel imposes on its participants.
    pub fn audio_policy(&self) -> AudioPolicy {
        AudioPolicy {
            bitrate_bps: self.bitrate,
            min_bitrate_bps: self.min_bitrate.min(self.bitrate),
            fec: self.enable_fec,
            dtx: self.enable_dtx,
            max_bandwidth: self.max_bandwidth,
            complexity: self.complexity,
            signal: match self.audio_profile {
                AudioProfile::Voice | AudioProfile::LowBandwidth => OpusSignal::Voice,
                AudioProfile::Music => OpusSignal::Music,
                AudioProfile::Broadcast => OpusSignal::Auto,
            },
        }
    }
}

/// Audio bandwidth an Opus encoder is allowed to use (`OPUS_SET_MAX_BANDWIDTH`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum OpusBandwidth {
    /// 4 kHz — telephone quality, ~6–10 kbit/s.
    Narrowband,
    /// 6 kHz.
    Mediumband,
    /// 8 kHz — the classic "wideband" voice codec range.
    Wideband,
    /// 12 kHz.
    Superwideband,
    /// 20 kHz — everything the 48 kHz stream carries.
    #[default]
    Fullband,
}

impl OpusBandwidth {
    /// `maxplaybackrate` value for SDP (RFC 7587): the sample rate that covers this bandwidth.
    pub fn max_playback_rate_hz(self) -> u32 {
        match self {
            Self::Narrowband => 8_000,
            Self::Mediumband => 12_000,
            Self::Wideband => 16_000,
            Self::Superwideband => 24_000,
            Self::Fullband => 48_000,
        }
    }
}

/// Content hint for the encoder (`OPUS_SET_SIGNAL`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum OpusSignal {
    #[default]
    Auto,
    Voice,
    Music,
}

/// Encoder settings a channel requires of everyone sending into it. Delivered in
/// `ChannelJoinAck.audio` and, when an operator edits the channel, in `ChannelAudioPolicy`.
/// A client in several channels applies [`AudioPolicy::merge`] over all of them.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AudioPolicy {
    pub bitrate_bps: u32,
    pub min_bitrate_bps: u32,
    pub fec: bool,
    pub dtx: bool,
    pub max_bandwidth: OpusBandwidth,
    pub complexity: Option<u8>,
    pub signal: OpusSignal,
}

impl Default for AudioPolicy {
    fn default() -> Self {
        ChannelConfig::default().audio_policy()
    }
}

impl AudioPolicy {
    /// Combined policy for a sender whose one encoder feeds several channels: the widest
    /// bitrate and bandwidth so no channel is starved, FEC if any channel wants it, DTX only if
    /// every channel allows it, the highest complexity hint, and `Music` if any channel is music.
    pub fn merge(self, other: AudioPolicy) -> AudioPolicy {
        AudioPolicy {
            bitrate_bps: self.bitrate_bps.max(other.bitrate_bps),
            min_bitrate_bps: self.min_bitrate_bps.max(other.min_bitrate_bps),
            fec: self.fec || other.fec,
            dtx: self.dtx && other.dtx,
            max_bandwidth: self.max_bandwidth.max(other.max_bandwidth),
            complexity: match (self.complexity, other.complexity) {
                (Some(a), Some(b)) => Some(a.max(b)),
                (a, b) => a.or(b),
            },
            signal: match (self.signal, other.signal) {
                (OpusSignal::Music, _) | (_, OpusSignal::Music) => OpusSignal::Music,
                (OpusSignal::Voice, _) | (_, OpusSignal::Voice) => OpusSignal::Voice,
                _ => OpusSignal::Auto,
            },
        }
    }

    /// Merge of all policies, or the default policy when the iterator is empty.
    pub fn merge_all(policies: impl IntoIterator<Item = AudioPolicy>) -> AudioPolicy {
        policies
            .into_iter()
            .reduce(AudioPolicy::merge)
            .unwrap_or_default()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct PositionalConfig {
    pub near_distance: f32,
    pub far_distance: f32,
    pub rolloff: RolloffCurve,
    pub max_radius: f32,
    /// Pan each speaker across the listener's stereo field by where they stand relative to
    /// the listener's reported orientation (native clients get the direction per frame, the
    /// WebRTC downlink is mixed in stereo). Off: mono, distance attenuation only.
    pub directional: bool,
    /// Handedness of the game's world coordinates, needed to tell left from right.
    pub coordinate_system: CoordinateSystem,
    /// Radius-based presence: two participants see each other (roster, join/leave, speaking,
    /// energy, typing, mute state, positions) only while closer than this. `None`: the whole
    /// channel is visible. A pair drops out of sight again at `roster_radius × 1.1`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub roster_radius: Option<f32>,
    /// Channel text messages reach only participants within this distance of the sender.
    /// `None`: every member.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text_radius: Option<f32>,
}

impl Default for PositionalConfig {
    fn default() -> Self {
        Self {
            near_distance: 1.0,
            far_distance: 50.0,
            rolloff: RolloffCurve::Logarithmic,
            max_radius: 100.0,
            directional: true,
            coordinate_system: CoordinateSystem::LeftHanded,
            roster_radius: None,
            text_radius: None,
        }
    }
}

impl PositionalConfig {
    /// Hysteresis applied when a visible pair separates: they stay in each other's roster
    /// until `roster_radius × ROSTER_EXIT_FACTOR`.
    pub const ROSTER_EXIT_FACTOR: f32 = 1.1;

    pub fn validate(&self) -> std::result::Result<(), String> {
        fn positive(name: &str, v: f32) -> std::result::Result<(), String> {
            if v.is_finite() && v > 0.0 {
                Ok(())
            } else {
                Err(format!(
                    "positional_config.{name} must be a positive number"
                ))
            }
        }
        positive("near_distance", self.near_distance)?;
        positive("far_distance", self.far_distance)?;
        positive("max_radius", self.max_radius)?;
        if self.far_distance < self.near_distance {
            return Err("positional_config.far_distance must be >= near_distance".into());
        }
        if let Some(r) = self.roster_radius {
            positive("roster_radius", r)?;
        }
        if let Some(r) = self.text_radius {
            positive("text_radius", r)?;
        }
        Ok(())
    }

    /// Distance at which a visible pair stops seeing each other.
    pub fn roster_exit_radius(&self) -> Option<f32> {
        self.roster_radius.map(|r| r * Self::ROSTER_EXIT_FACTOR)
    }
}

/// Cocktail-party mode ([`ChannelConfig::ambient`]). Speakers are ranked per receiver by the
/// gain they would be heard at (distance attenuation × local volume × focus); the top
/// `max_voices` are delivered as computed, the rest multiplied by `ambient_gain`. A speaker
/// that already holds a slot keeps it until a challenger is clearly louder, so slots do not
/// flap between equally loud voices.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AmbientConfig {
    pub max_voices: u8,
    pub ambient_gain: f32,
}

impl Default for AmbientConfig {
    fn default() -> Self {
        Self {
            max_voices: 4,
            ambient_gain: 0.15,
        }
    }
}

impl AmbientConfig {
    pub fn validate(&self) -> std::result::Result<(), String> {
        if self.max_voices == 0 {
            return Err("ambient.max_voices must be >= 1".into());
        }
        if !self.ambient_gain.is_finite() || !(0.0..=1.0).contains(&self.ambient_gain) {
            return Err("ambient.ambient_gain must be within 0.0..=1.0".into());
        }
        Ok(())
    }
}

/// Handedness of the world coordinate system positions/orientations are reported in.
/// Left-handed: Unity (`X` right, `Y` up, `Z` forward), Unreal (`X` forward, `Y` right, `Z` up).
/// Right-handed: OpenGL/Three.js/Godot (`X` right, `Y` up, `-Z` forward).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum CoordinateSystem {
    #[default]
    LeftHanded,
    RightHanded,
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
    /// UDP port of the node-to-node relay (None when cascade is disabled on that node).
    pub cascade_port: Option<u16>,
    /// Public WebSocket endpoint clients connect to (`wss://host/ws`). Nodes without one are
    /// never advertised by region discovery.
    #[serde(default)]
    pub ws_url: Option<String>,
    /// Public REST base URL of this node (`https://host`); `<api_url>/health` is the RTT probe.
    #[serde(default)]
    pub api_url: Option<String>,
    /// Approximate geographic position of the node, for distance-based selection.
    #[serde(default)]
    pub location: Option<GeoLocation>,
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
        if self.capacity == 0 {
            return 1.0;
        }
        self.active_participants as f32 / self.capacity as f32
    }

    pub fn is_available(&self) -> bool {
        self.healthy && self.load_factor() < 0.9
    }
}

/// WGS-84 coordinates in degrees.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct GeoLocation {
    pub latitude: f64,
    pub longitude: f64,
}

impl GeoLocation {
    pub fn is_valid(&self) -> bool {
        self.latitude.is_finite()
            && self.longitude.is_finite()
            && (-90.0..=90.0).contains(&self.latitude)
            && (-180.0..=180.0).contains(&self.longitude)
    }

    /// Great-circle distance in kilometres (haversine, mean Earth radius).
    pub fn distance_km(&self, other: &GeoLocation) -> f64 {
        const EARTH_RADIUS_KM: f64 = 6371.0088;
        let (lat1, lon1) = (self.latitude.to_radians(), self.longitude.to_radians());
        let (lat2, lon2) = (other.latitude.to_radians(), other.longitude.to_radians());
        let dlat = lat2 - lat1;
        let dlon = lon2 - lon1;
        let a = (dlat / 2.0).sin().powi(2) + lat1.cos() * lat2.cos() * (dlon / 2.0).sin().powi(2);
        2.0 * EARTH_RADIUS_KM * a.sqrt().asin()
    }
}

/// One entry of region discovery: the least-loaded healthy node of a region that advertises a
/// public WebSocket URL. Clients connect to `ws_url` and probe `probe_url` for RTT.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RegionEndpoint {
    pub region: Region,
    pub node_id: MediaNodeId,
    pub ws_url: String,
    /// `GET <probe_url>` is public, cheap and answered by exactly this node.
    pub probe_url: Option<String>,
    pub location: Option<GeoLocation>,
    /// Distance from the client's declared location, when both are known.
    pub distance_km: Option<f64>,
    /// Healthy nodes advertising a WebSocket URL in this region.
    pub nodes: u32,
    /// `active_participants / capacity` of the selected node.
    pub load_factor: f32,
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
        quality::mos_from_r(self.r_factor())
    }

    pub fn r_factor(&self) -> f32 {
        quality::r_factor(self.rtt_ms, self.jitter_ms, self.packet_loss_percent)
    }

    /// Network-quality indicator, `1` (unusable) to `5` (excellent).
    pub fn bars(&self) -> u8 {
        quality::bars_from_r(self.r_factor())
    }
}

/// Simplified E-model (ITU-T G.107) used by every Aurix component for a consistent
/// quality indicator: clients compute it from their own measurements, the server from the
/// client report plus what it observes on the uplink.
pub mod quality {
    /// Transmission rating `R` in `0..=100` from one-way network conditions. Jitter counts
    /// double because the jitter buffer turns it into delay; each percent of loss costs 2.5.
    pub fn r_factor(rtt_ms: f32, jitter_ms: f32, loss_percent: f32) -> f32 {
        let rtt = if rtt_ms.is_finite() {
            rtt_ms.max(0.0)
        } else {
            0.0
        };
        let jitter = if jitter_ms.is_finite() {
            jitter_ms.max(0.0)
        } else {
            0.0
        };
        let loss = if loss_percent.is_finite() {
            loss_percent.clamp(0.0, 100.0)
        } else {
            100.0
        };
        let effective_latency = rtt + jitter * 2.0 + 10.0;
        let r = if effective_latency < 160.0 {
            93.2 - (effective_latency / 40.0)
        } else {
            93.2 - ((effective_latency - 120.0) / 10.0)
        };
        (r - loss * 2.5).clamp(0.0, 100.0)
    }

    /// Mean opinion score `1.0..=4.5` for a rating `R`.
    pub fn mos_from_r(r: f32) -> f32 {
        let r = r.clamp(0.0, 100.0);
        1.0 + 0.035 * r + r * (r - 60.0) * (100.0 - r) * 7.0e-6
    }

    /// `R ≥ 80` → 5 bars, `≥ 70` → 4, `≥ 60` → 3, `≥ 50` → 2, else 1 (the G.107 user
    /// satisfaction bands).
    pub fn bars_from_r(r: f32) -> u8 {
        match r {
            r if r >= 80.0 => 5,
            r if r >= 70.0 => 4,
            r if r >= 60.0 => 3,
            r if r >= 50.0 => 2,
            _ => 1,
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn bands_follow_network_conditions() {
            assert_eq!(bars_from_r(r_factor(20.0, 2.0, 0.0)), 5);
            assert_eq!(bars_from_r(r_factor(150.0, 20.0, 3.0)), 4);
            assert_eq!(bars_from_r(r_factor(250.0, 30.0, 3.0)), 3);
            assert_eq!(bars_from_r(r_factor(300.0, 40.0, 6.0)), 2);
            assert_eq!(bars_from_r(r_factor(500.0, 80.0, 20.0)), 1);
            assert_eq!(bars_from_r(r_factor(f32::NAN, f32::INFINITY, f32::NAN)), 1);
            assert!(mos_from_r(93.0) > 4.3 && mos_from_r(0.0) == 1.0);
        }
    }
}

/// Link quality as seen by the server for one session: the client's view of its downlink
/// (from `QualityReport`) merged with what the SFU measures on the uplink. Sent to the
/// client as `NetworkQuality` and shown to operators in the participants/session stats API.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct NetworkQuality {
    /// `1..=5`, the worse of the two directions.
    pub bars: u8,
    pub r_factor: f32,
    pub mos: f32,
    /// Round trip as measured by the client (0 until it reported).
    pub rtt_ms: f32,
    pub downlink_jitter_ms: f32,
    pub downlink_loss_percent: f32,
    /// Inter-arrival jitter of the client's audio at the server (RFC 3550).
    pub uplink_jitter_ms: f32,
    /// Sequence gaps in the client's packets over the last report interval.
    pub uplink_loss_percent: f32,
    pub uplink_bitrate_kbps: u32,
    /// Totals since the session started.
    pub uplink_packets_received: u64,
    pub uplink_packets_lost: u64,
}

impl NetworkQuality {
    pub fn compose(
        client: &QualityMetrics,
        uplink_jitter_ms: f32,
        uplink_loss_percent: f32,
        uplink_bitrate_kbps: u32,
        uplink_packets_received: u64,
        uplink_packets_lost: u64,
    ) -> Self {
        let down = client.r_factor();
        let up = quality::r_factor(client.rtt_ms, uplink_jitter_ms, uplink_loss_percent);
        let r = down.min(up);
        Self {
            bars: quality::bars_from_r(r),
            r_factor: r,
            mos: quality::mos_from_r(r),
            rtt_ms: client.rtt_ms,
            downlink_jitter_ms: client.jitter_ms,
            downlink_loss_percent: client.packet_loss_percent,
            uplink_jitter_ms,
            uplink_loss_percent,
            uplink_bitrate_kbps,
            uplink_packets_received,
            uplink_packets_lost,
        }
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
    #[serde(default = "default_true")]
    pub join: bool,
    #[serde(default = "default_true")]
    pub speak: bool,
    #[serde(default = "default_true")]
    pub receive: bool,
    #[serde(default)]
    pub moderate: bool,
    /// When set, joining a channel that does not exist yet creates it with this template
    /// instead of failing with `CHANNEL_NOT_FOUND`. `channel_id` must equal
    /// `ChannelId::ad_hoc(app_id, template.name)`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ad_hoc: Option<AdHocChannel>,
}

/// Template for a channel created on first join. Kept deliberately small: the game server
/// picks a type and a size, the channel inherits the app defaults for everything else and is
/// destroyed automatically once the last participant leaves.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AdHocChannel {
    /// Stable per-app name, e.g. `match:1234` or `party:abcd`.
    pub name: String,
    #[serde(default = "default_ad_hoc_type")]
    pub channel_type: ChannelType,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_participants: Option<u32>,
}

fn default_ad_hoc_type() -> ChannelType {
    ChannelType::Team
}

impl AdHocChannel {
    pub fn channel_id(&self, app_id: AppId) -> ChannelId {
        ChannelId::ad_hoc(app_id, &self.name)
    }

    pub fn channel_config(&self) -> ChannelConfig {
        let mut config = ChannelConfig {
            channel_type: self.channel_type,
            ..ChannelConfig::default()
        };
        if let Some(max) = self.max_participants {
            config.max_participants = max;
        }
        if self.channel_type == ChannelType::Positional {
            config.positional_config = Some(PositionalConfig::default());
        }
        config
    }
}

/// Operation a one-time action token (`POST /v1/tokens/action`) authorises.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActionKind {
    /// Open a WebSocket session (no channel rights of its own).
    Login,
    /// Join one channel with the permissions carried in the token.
    Join,
    /// Remove `target_user_id` from `channel_id`.
    Kick,
    /// Server-mute `target_user_id` in `channel_id`.
    Mute,
    /// Lift a server-mute of `target_user_id` in `channel_id`.
    Unmute,
}

impl ActionKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Login => "login",
            Self::Join => "join",
            Self::Kick => "kick",
            Self::Mute => "mute",
            Self::Unmute => "unmute",
        }
    }

    /// Actions that require a channel.
    pub fn needs_channel(&self) -> bool {
        !matches!(self, Self::Login)
    }

    /// Actions that are performed on another player.
    pub fn needs_target(&self) -> bool {
        matches!(self, Self::Kick | Self::Mute | Self::Unmute)
    }
}

impl std::fmt::Display for ActionKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

fn default_true() -> bool {
    true
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
    UserKicked,
    ModerationAction,
    RecordingAccessed,
    RecordingDeleted,
    AdminLogin,
    AdminCreated,
    AppCreated,
    AppDeleted,
    WebhookCreated,
    WebhookUpdated,
    WebhookDeleted,
    WebhookSecretRotated,
    ChannelMuteAll,
    ChannelKickAll,
    UserDeleted,
    LiveStreamStarted,
    LiveStreamStopped,
    UserDataExported,
    RetentionSweep,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PaginationParams {
    pub page: u32,
    pub per_page: u32,
}

impl Default for PaginationParams {
    fn default() -> Self {
        Self {
            page: 1,
            per_page: 50,
        }
    }
}

impl PaginationParams {
    pub fn offset(&self) -> u32 {
        (self.page.saturating_sub(1)) * self.per_page
    }
    pub fn limit(&self) -> u32 {
        self.per_page.min(200)
    }
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
        Self {
            data,
            page,
            per_page,
            total,
            total_pages,
        }
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
        Self {
            room_size: 0.5,
            decay_time: 1.0,
            wet_dry_mix: 0.3,
        }
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::f32::consts::{FRAC_PI_2, PI};

    #[test]
    fn ad_hoc_channel_ids_are_stable_per_app_and_name() {
        let app = AppId::new();
        let other = AppId::new();
        assert_eq!(
            ChannelId::ad_hoc(app, "match-1"),
            ChannelId::ad_hoc(app, "match-1")
        );
        assert_ne!(
            ChannelId::ad_hoc(app, "match-1"),
            ChannelId::ad_hoc(app, "match-2")
        );
        assert_ne!(
            ChannelId::ad_hoc(app, "match-1"),
            ChannelId::ad_hoc(other, "match-1")
        );
        let template = AdHocChannel {
            name: "match-1".into(),
            channel_type: ChannelType::Team,
            max_participants: None,
        };
        assert_eq!(template.channel_id(app), ChannelId::ad_hoc(app, "match-1"));
        let parsed: AdHocChannel = serde_json::from_str(r#"{"name":"match-1"}"#).unwrap();
        assert_eq!(parsed, template);
    }

    #[test]
    fn ad_hoc_template_builds_a_channel_config() {
        let team = AdHocChannel {
            name: "t".into(),
            channel_type: ChannelType::Team,
            max_participants: Some(8),
        }
        .channel_config();
        assert_eq!(team.channel_type, ChannelType::Team);
        assert_eq!(team.max_participants, 8);
        assert!(team.positional_config.is_none());
        let positional = AdHocChannel {
            name: "p".into(),
            channel_type: ChannelType::Positional,
            max_participants: None,
        }
        .channel_config();
        assert_eq!(
            positional.max_participants,
            ChannelConfig::default().max_participants
        );
        assert!(positional.positional_config.is_some());
    }

    fn dir(listener: (f32, f32, f32), source: (f32, f32, f32), o: Orientation3D) -> Direction {
        Direction::from_listener(
            &Position3D::new(listener.0, listener.1, listener.2),
            &o,
            &Position3D::new(source.0, source.1, source.2),
            CoordinateSystem::LeftHanded,
        )
        .expect("direction")
    }

    #[test]
    fn direction_follows_unity_conventions() {
        let o = Orientation3D::default(); // facing +Z, up +Y → right is +X
        let ahead = dir((0.0, 0.0, 0.0), (0.0, 0.0, 5.0), o.clone());
        assert!(ahead.azimuth.abs() < 1e-5 && ahead.elevation.abs() < 1e-5);
        let right = dir((0.0, 0.0, 0.0), (3.0, 0.0, 0.0), o.clone());
        assert!((right.azimuth - FRAC_PI_2).abs() < 1e-5);
        let left = dir((0.0, 0.0, 0.0), (-3.0, 0.0, 0.0), o.clone());
        assert!((left.azimuth + FRAC_PI_2).abs() < 1e-5);
        let behind = dir((0.0, 0.0, 0.0), (0.0, 0.0, -1.0), o.clone());
        assert!((behind.azimuth.abs() - PI).abs() < 1e-5);
        let above = dir((0.0, 0.0, 0.0), (0.0, 4.0, 0.0), o);
        assert!((above.elevation - FRAC_PI_2).abs() < 1e-5);
    }

    #[test]
    fn direction_is_relative_to_listener_orientation() {
        // Listener turned to face +X: a source at +X is now ahead, one at +Z is on the left.
        let facing_x = Orientation3D {
            forward_x: 1.0,
            forward_y: 0.0,
            forward_z: 0.0,
            up_x: 0.0,
            up_y: 1.0,
            up_z: 0.0,
        };
        let ahead = dir((10.0, 0.0, 10.0), (20.0, 0.0, 10.0), facing_x.clone());
        assert!(ahead.azimuth.abs() < 1e-5);
        let left = dir((10.0, 0.0, 10.0), (10.0, 0.0, 20.0), facing_x);
        assert!((left.azimuth + FRAC_PI_2).abs() < 1e-5);

        // Right-handed worlds mirror left/right.
        let rh = Direction::from_listener(
            &Position3D::new(0.0, 0.0, 0.0),
            &Orientation3D::default(),
            &Position3D::new(3.0, 0.0, 0.0),
            CoordinateSystem::RightHanded,
        )
        .unwrap();
        assert!((rh.azimuth + FRAC_PI_2).abs() < 1e-5);
    }

    #[test]
    fn degenerate_inputs_have_no_direction() {
        let o = Orientation3D::default();
        let p = Position3D::new(1.0, 2.0, 3.0);
        assert!(Direction::from_listener(&p, &o, &p, CoordinateSystem::LeftHanded).is_none());
        let zero_forward = Orientation3D {
            forward_x: 0.0,
            forward_y: 0.0,
            forward_z: 0.0,
            ..Orientation3D::default()
        };
        let q = Position3D::new(0.0, 0.0, 0.0);
        assert!(
            Direction::from_listener(&q, &zero_forward, &p, CoordinateSystem::LeftHanded).is_none()
        );
        let up_is_forward = Orientation3D {
            up_x: 0.0,
            up_y: 0.0,
            up_z: 1.0,
            ..Orientation3D::default()
        };
        assert!(
            Direction::from_listener(&q, &up_is_forward, &p, CoordinateSystem::LeftHanded)
                .is_none()
        );
    }

    #[test]
    fn stereo_pan_is_constant_power() {
        let (l, r) = Direction::AHEAD.stereo_gains();
        assert!((l - 1.0).abs() < 1e-5 && (r - 1.0).abs() < 1e-5);
        let (l, r) = Direction {
            azimuth: FRAC_PI_2,
            elevation: 0.0,
        }
        .stereo_gains();
        assert!(l.abs() < 1e-5 && (r - std::f32::consts::SQRT_2).abs() < 1e-5);
        let (l, r) = Direction {
            azimuth: -FRAC_PI_2,
            elevation: 0.0,
        }
        .stereo_gains();
        assert!((l - std::f32::consts::SQRT_2).abs() < 1e-5 && r.abs() < 1e-5);
        // Power is constant along the arc.
        let (l, r) = Direction {
            azimuth: 0.4,
            elevation: 0.0,
        }
        .stereo_gains();
        assert!((l * l + r * r - 2.0).abs() < 1e-5);
        // Behind-right pans right just like front-right.
        let (l, r) = Direction {
            azimuth: 3.0 * PI / 4.0,
            elevation: 0.0,
        }
        .stereo_gains();
        assert!(r > l);
    }

    #[test]
    fn channel_config_validation_bounds_opus_settings() {
        let mut cfg = ChannelConfig::default();
        assert!(cfg.validate(510_000).is_ok());
        cfg.bitrate = 5_000;
        assert!(cfg.validate(510_000).is_err());
        cfg.bitrate = 64_000;
        assert!(cfg.validate(32_000).is_err(), "node cap applies");
        assert!(cfg.validate(64_000).is_ok());
        cfg.min_bitrate = 96_000;
        assert!(cfg.validate(510_000).is_err(), "floor above target");
        cfg.min_bitrate = 8_000;
        cfg.complexity = Some(11);
        assert!(cfg.validate(510_000).is_err());
        cfg.complexity = Some(10);
        cfg.sample_rate = 44_100;
        assert!(cfg.validate(510_000).is_err());
        cfg.sample_rate = 16_000;
        assert!(cfg.validate(510_000).is_ok());
        cfg.max_participants = 0;
        assert!(cfg.validate(510_000).is_err());
    }

    #[test]
    fn audio_policy_merge_is_the_most_permissive_encoder() {
        let voice = ChannelConfig {
            bitrate: 24_000,
            min_bitrate: 8_000,
            enable_dtx: true,
            enable_fec: false,
            max_bandwidth: OpusBandwidth::Wideband,
            complexity: Some(5),
            ..ChannelConfig::default()
        }
        .audio_policy();
        let music = ChannelConfig {
            bitrate: 96_000,
            min_bitrate: 32_000,
            enable_dtx: false,
            enable_fec: true,
            max_bandwidth: OpusBandwidth::Fullband,
            complexity: None,
            audio_profile: AudioProfile::Music,
            ..ChannelConfig::default()
        }
        .audio_policy();
        assert_eq!(voice.signal, OpusSignal::Voice);
        assert_eq!(music.signal, OpusSignal::Music);
        let merged = voice.merge(music);
        assert_eq!(merged.bitrate_bps, 96_000);
        assert_eq!(merged.min_bitrate_bps, 32_000);
        assert!(merged.fec && !merged.dtx);
        assert_eq!(merged.max_bandwidth, OpusBandwidth::Fullband);
        assert_eq!(merged.complexity, Some(5));
        assert_eq!(merged.signal, OpusSignal::Music);
        assert_eq!(music.merge(voice), merged, "commutative");
        assert_eq!(AudioPolicy::merge_all([]), AudioPolicy::default());
        assert_eq!(AudioPolicy::merge_all([voice]), voice);
        assert_eq!(
            AudioPolicy::default().max_bandwidth.max_playback_rate_hz(),
            48_000
        );
    }
}
