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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
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

/// How a native AURX session receives channel audio (`SetDownlinkMode`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum DownlinkMode {
    /// One stream per speaker, each with its own SSRC, gain and direction; the client mixes.
    #[default]
    Streams,
    /// One server-mixed stereo Opus stream per channel (`PacketFlags::Mixed`), with every
    /// per-receiver rule (mutes, volumes, focus, positional attenuation, ambient slots)
    /// already applied. End-to-end encrypted speakers still arrive as separate streams.
    Mixed,
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
    /// Native AURX as QUIC datagrams on the media port (0-RTT resume, connection migration).
    Quic,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
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
    /// Participants may send two-channel (stereo) Opus — music, DJ and broadcast sources.
    /// Off (the default) tells clients to encode mono; the SFU forwards whatever arrives
    /// either way, so a stereo receiver decodes stereo and a mono one downmixes.
    pub stereo: bool,
    /// Cocktail-party mixing: each receiver hears at most `max_voices` speakers at their
    /// computed gain, every other concurrent speaker at `ambient_gain` (`0` drops them).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ambient: Option<AmbientConfig>,
    /// Large-channel / audience settings (see [`AudienceConfig`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audience: Option<AudienceConfig>,
    /// Priority speakers (see [`DuckingConfig`]): while a member whose grant has
    /// `priority: true` (or a moderator, when `moderators` is set) is talking, every other
    /// voice in the channel is attenuated for every receiver. `None` = nobody is ducked.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ducking: Option<DuckingConfig>,
    pub recording_enabled: bool,
    /// Transcribe participants' speech (when `[stt]` is configured on the node) and deliver
    /// `Transcript` events to the channel's participants.
    pub transcription: bool,
    /// Run participants' speech through the node's `[safety]` pipeline (transcribe →
    /// classify → incidents). Independent of `transcription`: transcripts are not delivered to
    /// participants unless that is set too. Never applies to end-to-end encrypted media.
    pub safety_voice: bool,
    /// End-to-end encryption: participants encrypt their frames with sender keys they exchange
    /// among themselves (`crate::e2ee`); the node relays the key exchange and the frames but
    /// cannot decode them. Excludes everything that needs the node to hear the audio:
    /// recording, transcription, translation, safety, TTS into the channel, the server mix
    /// (native listeners get per-speaker streams, browsers only their per-participant tracks),
    /// PCMU transcoding and ambient mixing. Members whose client lacks E2EE support hear
    /// nothing and are not heard.
    pub e2ee: bool,
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
            stereo: false,
            ambient: None,
            audience: None,
            ducking: None,
            recording_enabled: false,
            transcription: false,
            safety_voice: false,
            e2ee: false,
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
        if let Some(a) = &self.audience {
            a.validate(self.max_participants)?;
        }
        if let Some(d) = &self.ducking {
            d.validate()?;
        }
        if self.e2ee {
            if self.recording_enabled {
                return Err("e2ee channels cannot be recorded".into());
            }
            if self.transcription || self.safety_voice {
                return Err(
                    "e2ee channels cannot be transcribed (transcription/safety_voice)".into(),
                );
            }
            if self.ambient.is_some() {
                return Err("e2ee channels cannot use ambient mixing".into());
            }
            if self.audience.is_some_and(|a| a.mix_for_listeners) {
                return Err("e2ee channels cannot mix for listeners".into());
            }
            if self.channel_type == ChannelType::Echo {
                return Err("echo channels cannot be end-to-end encrypted".into());
            }
        }
        Ok(())
    }

    /// Whether members without the `speak` permission are kept out of presence.
    pub fn hides_listeners(&self) -> bool {
        self.audience.is_some_and(|a| a.hide_listeners)
    }

    /// Whether native listeners get one server-mixed stream regardless of their own
    /// `DownlinkMode`.
    pub fn mixes_for_listeners(&self) -> bool {
        self.audience.is_some_and(|a| a.mix_for_listeners)
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
            stereo: self.stereo,
            e2ee: self.e2ee,
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
    /// Senders may encode two channels; `false` asks for mono.
    pub stereo: bool,
    /// Frames into this channel must be end-to-end encrypted (`crate::e2ee`).
    pub e2ee: bool,
}

impl Default for AudioPolicy {
    fn default() -> Self {
        ChannelConfig::default().audio_policy()
    }
}

impl AudioPolicy {
    /// Combined policy for a sender whose one encoder feeds several channels: the widest
    /// bitrate and bandwidth so no channel is starved, FEC if any channel wants it, DTX only if
    /// every channel allows it, the highest complexity hint, `Music` if any channel is music,
    /// stereo if any channel accepts it, and encrypted if any channel requires it (a client
    /// with one uplink for several channels must then encrypt for all of them).
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
            stereo: self.stereo || other.stereo,
            e2ee: self.e2ee || other.e2ee,
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

/// Audience (listen-only / large-channel) settings of a channel. A member whose permission
/// grant has `speak: false` (`ChannelRole::Listener`) is receive-only in every channel type;
/// this block controls how such listeners are presented and served so that a channel can
/// hold thousands of them at the cost of a handful of speakers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct AudienceConfig {
    /// Listeners are absent from the roster and from `ParticipantJoined` / `ParticipantLeft`
    /// / mute / energy notifications of other members (they still see the speakers and
    /// each other's text). `ChannelJoinAck.participant_count` carries the real headcount.
    pub hide_listeners: bool,
    /// Native listeners receive one server-mixed stream (`DownlinkMode::Mixed`) instead of a
    /// stream per speaker, whatever their own downlink mode. Browsers are always mixed.
    pub mix_for_listeners: bool,
    /// Members that may speak (`speak: true`) the channel admits at once, `0` = no separate
    /// limit (only `max_participants`). Counted over the members this node knows of (its own
    /// and those learned through the cascade).
    pub max_speakers: u32,
    /// Concurrent voices a receiver hears at once, `0` = unlimited. Speakers are ranked per
    /// receiver by what *that* receiver would hear (distance, volume, focus, ambient slot ×
    /// the sender's reported level); holders keep their slot while they talk, the rest are
    /// withheld until a slot frees. Bounds the per-receiver work everywhere: streams a native
    /// client decodes, tracks a browser receives, voices a server mix decodes.
    pub max_streams: u8,
}

impl Default for AudienceConfig {
    fn default() -> Self {
        Self {
            hide_listeners: true,
            mix_for_listeners: true,
            max_speakers: 0,
            max_streams: 0,
        }
    }
}

impl AudienceConfig {
    pub fn validate(&self, max_participants: u32) -> std::result::Result<(), String> {
        if self.max_speakers > max_participants {
            return Err("audience.max_speakers must not exceed max_participants".into());
        }
        Ok(())
    }
}

/// Priority speaker ducking ([`ChannelConfig::ducking`]). Priority members are those whose
/// channel grant carries `priority: true` (a raid leader, a shoutcaster) or whom a moderator
/// promoted at runtime; with `moderators` set, moderators and administrators count too.
/// While any priority member's frames are audible the gain of every non-priority voice is
/// ramped down to `gain` over `attack_ms`, held for `hold_ms` past their last audible frame
/// and ramped back over `release_ms`. Applied by the node to everything it mixes or
/// forwards with a gain (native streams, server mixes) and reproduced by browsers on their
/// per-participant tracks from the priority members' speaking state.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct DuckingConfig {
    /// Gain of non-priority voices while a priority speaker talks (`0.0` = silenced,
    /// `1.0` = no ducking).
    pub gain: f32,
    pub attack_ms: u32,
    pub release_ms: u32,
    /// How long ducking persists after the priority speaker's last audible frame, so
    /// pauses between words do not pump the mix.
    pub hold_ms: u32,
    /// Moderators and administrators are priority speakers as well.
    pub moderators: bool,
}

impl Default for DuckingConfig {
    fn default() -> Self {
        Self {
            gain: 0.25,
            attack_ms: 60,
            release_ms: 400,
            hold_ms: 250,
            moderators: false,
        }
    }
}

impl DuckingConfig {
    pub const MAX_MS: u32 = 10_000;

    pub fn validate(&self) -> std::result::Result<(), String> {
        if !self.gain.is_finite() || !(0.0..=1.0).contains(&self.gain) {
            return Err("ducking.gain must be within 0.0..=1.0".into());
        }
        for (name, ms) in [
            ("attack_ms", self.attack_ms),
            ("release_ms", self.release_ms),
            ("hold_ms", self.hold_ms),
        ] {
            if ms > Self::MAX_MS {
                return Err(format!("ducking.{name} must be <= {}", Self::MAX_MS));
            }
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
    /// Primary address other nodes reach this node at (public IPv4 when configured, else the
    /// IPv6 address or the bind host).
    pub address: String,
    /// Public IPv6 address, when the node is reachable over IPv6 as well.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub address_ipv6: Option<String>,
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
    /// Aurix version the node runs (`CARGO_PKG_VERSION` of the server binary).
    #[serde(default)]
    pub version: String,
    /// When the node first registered with the fleet.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub registered_at: Option<DateTime<Utc>>,
    pub capacity: u32,
    /// Pure cascade relay hub (`media.cascade_relay_only`): never hosts clients, never
    /// selected for sessions or failover, preferred as a regional hub for relay trees.
    #[serde(default)]
    pub relay_only: bool,
    /// Operator drain in progress: existing sessions stay (and may resume), fresh sessions,
    /// failover and region discovery skip the node. Persisted until `undrain`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub drain: Option<NodeDrain>,
}

impl MediaNodeInfo {
    pub fn load_factor(&self) -> f32 {
        if self.capacity == 0 {
            return 1.0;
        }
        self.active_participants as f32 / self.capacity as f32
    }

    pub fn is_available(&self) -> bool {
        self.healthy && !self.relay_only && self.drain.is_none() && self.load_factor() < 0.9
    }

    pub fn is_draining(&self) -> bool {
        self.drain.is_some()
    }
}

/// Who put a node into maintenance, when and why (`POST /v1/nodes/{id}/drain`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NodeDrain {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    pub since: DateTime<Utc>,
    /// Administrator id that started the drain.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub by: Option<Uuid>,
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
    /// Worst downlink loss any local receiver of this session's audio reported over its
    /// last interval (0 with no receivers). Senders protect their uplink against the worse
    /// of this and `uplink_loss_percent`, since only the sender can add FEC/DRED for a
    /// receiver on a lossy link. Receivers hosted on other nodes are not included.
    #[serde(default)]
    pub receivers_loss_percent: f32,
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
        receivers_loss_percent: f32,
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
            receivers_loss_percent,
            uplink_bitrate_kbps,
            uplink_packets_received,
            uplink_packets_lost,
        }
    }

    /// Loss a sender should protect its uplink against: the worse of what the server sees
    /// on the uplink and what its worst receiver sees on the downlink.
    pub fn protect_loss_percent(&self) -> f32 {
        let up = if self.uplink_loss_percent.is_finite() {
            self.uplink_loss_percent
        } else {
            0.0
        };
        let down = if self.receivers_loss_percent.is_finite() {
            self.receivers_loss_percent
        } else {
            0.0
        };
        up.max(down).clamp(0.0, 100.0)
    }
}

/// Lifetime quality record of one session, built from every `NetworkQuality` evaluation the
/// node made for it (`media.quality_interval_ms`). Persisted as `sessions.quality_stats`
/// (refreshed while the session lives, final on disconnect) and shown in the session stats
/// and user APIs. Averages are time-weighted per evaluation period; `jitter` and `loss` take
/// the worse direction of each sample, like the rating itself.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct QualitySummary {
    /// Evaluations folded in.
    pub samples: u64,
    /// Rated time (sum of the evaluation periods), seconds.
    pub seconds: f64,
    pub mos_avg: f32,
    pub mos_min: f32,
    pub mos_last: f32,
    pub r_factor_avg: f32,
    pub rtt_avg_ms: f32,
    pub rtt_max_ms: f32,
    pub jitter_avg_ms: f32,
    pub loss_avg_percent: f32,
    pub loss_max_percent: f32,
    /// Evaluations per bar count, index 0 = 1 bar.
    pub bars: [u64; 5],
    /// Rated time at 1–2 bars (R < 60, "many users dissatisfied").
    pub poor_seconds: f64,
    /// `quality.alert {metric: "mos"}` events raised for the session.
    pub mos_alerts: u32,
    /// The most recent evaluation.
    pub last: Option<NetworkQuality>,
}

/// Quality metered into the usage buckets since the last flush (see
/// [`crate::usage::UsageMetric`]): integer sums so the counters stay additive.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct QualityDelta {
    pub samples: u64,
    pub mos_milli: u64,
    pub rtt_ms: u64,
    pub jitter_ms: u64,
    pub loss_permille: u64,
    pub poor_samples: u64,
}

impl QualityDelta {
    pub fn is_empty(&self) -> bool {
        self.samples == 0
    }
}

/// Accumulates [`NetworkQuality`] evaluations into a [`QualitySummary`] and the not yet
/// metered [`QualityDelta`].
#[derive(Debug, Clone, Default, PartialEq)]
pub struct QualityAccumulator {
    samples: u64,
    seconds: f64,
    mos_sum: f64,
    mos_min: f32,
    r_sum: f64,
    rtt_sum: f64,
    rtt_max: f32,
    jitter_sum: f64,
    loss_sum: f64,
    loss_max: f32,
    bars: [u64; 5],
    poor_seconds: f64,
    mos_alerts: u32,
    last: Option<NetworkQuality>,
    pending: QualityDelta,
}

impl QualityAccumulator {
    /// Continues a summary persisted by a previous host of the session (cross-node resume):
    /// the averages are re-weighted by their sample count, so later samples extend them.
    pub fn from_summary(s: &QualitySummary) -> Self {
        let n = s.samples as f64;
        Self {
            samples: s.samples,
            seconds: s.seconds.max(0.0),
            mos_sum: f64::from(s.mos_avg) * n,
            mos_min: s.mos_min,
            r_sum: f64::from(s.r_factor_avg) * n,
            rtt_sum: f64::from(s.rtt_avg_ms) * n,
            rtt_max: s.rtt_max_ms,
            jitter_sum: f64::from(s.jitter_avg_ms) * n,
            loss_sum: f64::from(s.loss_avg_percent) * n,
            loss_max: s.loss_max_percent,
            bars: s.bars,
            poor_seconds: s.poor_seconds.max(0.0),
            mos_alerts: s.mos_alerts,
            last: s.last,
            pending: QualityDelta::default(),
        }
    }

    /// Folds one evaluation covering `period_secs` of the session in.
    pub fn record(&mut self, q: &NetworkQuality, period_secs: f64) {
        let period = if period_secs.is_finite() {
            period_secs.max(0.0)
        } else {
            0.0
        };
        let jitter = q.downlink_jitter_ms.max(q.uplink_jitter_ms);
        let loss = q.downlink_loss_percent.max(q.uplink_loss_percent);
        if self.samples == 0 {
            self.mos_min = q.mos;
        } else {
            self.mos_min = self.mos_min.min(q.mos);
        }
        self.samples += 1;
        self.seconds += period;
        self.mos_sum += f64::from(q.mos);
        self.r_sum += f64::from(q.r_factor);
        self.rtt_sum += f64::from(q.rtt_ms);
        self.rtt_max = self.rtt_max.max(q.rtt_ms);
        self.jitter_sum += f64::from(jitter);
        self.loss_sum += f64::from(loss);
        self.loss_max = self.loss_max.max(loss);
        let bar = usize::from(q.bars.clamp(1, 5)) - 1;
        self.bars[bar] += 1;
        let poor = q.bars <= 2;
        if poor {
            self.poor_seconds += period;
        }
        self.last = Some(*q);
        self.pending.samples += 1;
        self.pending.mos_milli += (q.mos.clamp(0.0, 5.0) * 1000.0).round() as u64;
        self.pending.rtt_ms += q.rtt_ms.clamp(0.0, 60_000.0).round() as u64;
        self.pending.jitter_ms += jitter.clamp(0.0, 60_000.0).round() as u64;
        self.pending.loss_permille += (loss.clamp(0.0, 100.0) * 10.0).round() as u64;
        if poor {
            self.pending.poor_samples += 1;
        }
    }

    pub fn note_mos_alert(&mut self) {
        self.mos_alerts = self.mos_alerts.saturating_add(1);
    }

    /// Quality metered since the previous call.
    pub fn take_unmetered(&mut self) -> QualityDelta {
        std::mem::take(&mut self.pending)
    }

    pub fn samples(&self) -> u64 {
        self.samples
    }

    pub fn last(&self) -> Option<NetworkQuality> {
        self.last
    }

    pub fn summary(&self) -> QualitySummary {
        let n = self.samples as f64;
        let avg = |sum: f64| {
            if self.samples == 0 {
                0.0
            } else {
                (sum / n) as f32
            }
        };
        QualitySummary {
            samples: self.samples,
            seconds: self.seconds,
            mos_avg: avg(self.mos_sum),
            mos_min: self.mos_min,
            mos_last: self.last.map(|q| q.mos).unwrap_or(0.0),
            r_factor_avg: avg(self.r_sum),
            rtt_avg_ms: avg(self.rtt_sum),
            rtt_max_ms: self.rtt_max,
            jitter_avg_ms: avg(self.jitter_sum),
            loss_avg_percent: avg(self.loss_sum),
            loss_max_percent: self.loss_max,
            bars: self.bars,
            poor_seconds: self.poor_seconds,
            mos_alerts: self.mos_alerts,
            last: self.last,
        }
    }
}

/// Debounced MOS threshold detector for one session: `Degraded` after `periods` consecutive
/// evaluations below `threshold`, `Recovered` after as many consecutive evaluations at or
/// above `threshold + MOS_RECOVERY_MARGIN`. A threshold of `0` disables it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MosAlertState {
    below: u32,
    above: u32,
    alerting: bool,
}

/// Hysteresis above the alert threshold before a session counts as recovered.
pub const MOS_RECOVERY_MARGIN: f32 = 0.2;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MosTransition {
    Degraded,
    Recovered,
}

impl MosAlertState {
    pub fn is_alerting(&self) -> bool {
        self.alerting
    }

    pub fn observe(&mut self, mos: f32, threshold: f32, periods: u32) -> Option<MosTransition> {
        if threshold.is_nan() || threshold <= 0.0 || !mos.is_finite() {
            return None;
        }
        let periods = periods.max(1);
        if mos < threshold {
            self.below = self.below.saturating_add(1);
            self.above = 0;
            if !self.alerting && self.below >= periods {
                self.alerting = true;
                return Some(MosTransition::Degraded);
            }
        } else {
            self.below = 0;
            if mos >= threshold + MOS_RECOVERY_MARGIN {
                self.above = self.above.saturating_add(1);
                if self.alerting && self.above >= periods {
                    self.alerting = false;
                    self.above = 0;
                    return Some(MosTransition::Recovered);
                }
            } else {
                self.above = 0;
            }
        }
        None
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
    /// Priority speaker: while this member talks, the channel's [`DuckingConfig`] (if any)
    /// attenuates everyone else. Meaningless without `speak`.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub priority: bool,
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
    PriorityChanged,
    ChannelCreated,
    ChannelDeleted,
    ChannelConfigUpdated,
    ParticipantKicked,
    RecordingStarted,
    RecordingStopped,
    ApiKeyCreated,
    ApiKeyUpdated,
    ApiKeyRevoked,
    RoleChanged,
    ConfigUpdated,
    UserKicked,
    ModerationAction,
    RecordingAccessed,
    RecordingDeleted,
    AdminLogin,
    AdminCreated,
    AdminUpdated,
    AdminDeactivated,
    AdminPasswordChanged,
    AppCreated,
    AppUpdated,
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
    NodeDrained,
    NodeUndrained,
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

/// Dashboard role of an administrator. Roles are ordered: a higher role holds every permission
/// of the lower ones.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AdminRole {
    /// Read-only view of applications, nodes and health.
    Viewer,
    /// Viewer plus the audit log and moderation data across applications.
    Moderator,
    /// Moderator plus application management (create/update, API-key rotation).
    Admin,
    /// Everything, including administrator accounts, application deletion and retention sweeps.
    Superadmin,
}

impl AdminRole {
    pub const ALL: [AdminRole; 4] = [Self::Viewer, Self::Moderator, Self::Admin, Self::Superadmin];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Viewer => "viewer",
            Self::Moderator => "moderator",
            Self::Admin => "admin",
            Self::Superadmin => "superadmin",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "viewer" => Some(Self::Viewer),
            "moderator" => Some(Self::Moderator),
            "admin" => Some(Self::Admin),
            "superadmin" => Some(Self::Superadmin),
            _ => None,
        }
    }

    pub fn allows(self, permission: AdminPermission) -> bool {
        self >= permission.minimum_role()
    }

    pub fn permissions(self) -> Vec<AdminPermission> {
        AdminPermission::ALL
            .into_iter()
            .filter(|p| self.allows(*p))
            .collect()
    }
}

impl std::fmt::Display for AdminRole {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Fine-grained dashboard capabilities, each granted from a minimum [`AdminRole`] upwards.
/// Serialised as the `scope:action` strings from [`AdminPermission::as_str`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AdminPermission {
    AppsRead,
    AppsWrite,
    AppsDelete,
    KeysRotate,
    NodesRead,
    NodesDrain,
    ConfigRead,
    AuditRead,
    ModerationRead,
    AnalyticsRead,
    RetentionRun,
    AdminsManage,
}

impl AdminPermission {
    pub const ALL: [AdminPermission; 12] = [
        Self::AppsRead,
        Self::AppsWrite,
        Self::AppsDelete,
        Self::KeysRotate,
        Self::NodesRead,
        Self::NodesDrain,
        Self::ConfigRead,
        Self::AuditRead,
        Self::ModerationRead,
        Self::AnalyticsRead,
        Self::RetentionRun,
        Self::AdminsManage,
    ];

    pub fn minimum_role(self) -> AdminRole {
        match self {
            Self::AppsRead | Self::NodesRead | Self::AnalyticsRead => AdminRole::Viewer,
            Self::AuditRead | Self::ModerationRead => AdminRole::Moderator,
            Self::AppsWrite | Self::KeysRotate | Self::NodesDrain | Self::ConfigRead => {
                AdminRole::Admin
            }
            Self::AppsDelete | Self::RetentionRun | Self::AdminsManage => AdminRole::Superadmin,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::AppsRead => "apps:read",
            Self::AppsWrite => "apps:write",
            Self::AppsDelete => "apps:delete",
            Self::KeysRotate => "keys:rotate",
            Self::NodesRead => "nodes:read",
            Self::NodesDrain => "nodes:drain",
            Self::ConfigRead => "config:read",
            Self::AuditRead => "audit:read",
            Self::ModerationRead => "moderation:read",
            Self::AnalyticsRead => "analytics:read",
            Self::RetentionRun => "retention:run",
            Self::AdminsManage => "admins:manage",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|p| p.as_str() == s)
    }
}

impl Serialize for AdminPermission {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for AdminPermission {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        Self::parse(&s)
            .ok_or_else(|| serde::de::Error::custom(format!("unknown admin permission {s:?}")))
    }
}

/// How an administrator authenticates.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AdminAuthSource {
    Password,
    Oidc,
}

impl AdminAuthSource {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Password => "password",
            Self::Oidc => "oidc",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "password" => Some(Self::Password),
            "oidc" => Some(Self::Oidc),
            _ => None,
        }
    }
}

/// Admin authentication context injected by admin middleware. `role` is the account's current
/// role (read from the database on every request, not from the token).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AdminContext {
    pub admin_id: uuid::Uuid,
    pub email: String,
    pub role: AdminRole,
    pub auth_source: AdminAuthSource,
}

impl AdminContext {
    pub fn require(&self, permission: AdminPermission) -> crate::error::Result<()> {
        if self.role.allows(permission) {
            Ok(())
        } else {
            Err(crate::error::AurixError::AuthorizationDenied(format!(
                "Admin role '{}' lacks {}",
                self.role,
                permission.as_str()
            )))
        }
    }
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

    #[test]
    fn admin_permissions_use_scope_action_strings_on_the_wire() {
        for p in AdminPermission::ALL {
            let json = serde_json::to_string(&p).unwrap();
            assert_eq!(json, format!("\"{}\"", p.as_str()));
            assert_eq!(serde_json::from_str::<AdminPermission>(&json).unwrap(), p);
            assert_eq!(AdminPermission::parse(p.as_str()), Some(p));
            assert!(p.minimum_role().allows(p));
        }
        assert!(serde_json::from_str::<AdminPermission>("\"audit_read\"").is_err());
        assert_eq!(
            serde_json::to_value(AdminRole::Viewer.permissions()).unwrap(),
            serde_json::json!(["apps:read", "nodes:read", "analytics:read"])
        );
        assert!(AdminRole::Superadmin.permissions().len() == AdminPermission::ALL.len());
        assert!(!AdminRole::Admin.allows(AdminPermission::AdminsManage));
    }

    fn rated(rtt: f32, loss: f32) -> NetworkQuality {
        let client = QualityMetrics {
            rtt_ms: rtt,
            jitter_ms: 5.0,
            packet_loss_percent: loss,
            bitrate_kbps: 32,
            mos_score: 0.0,
        };
        NetworkQuality::compose(&client, 2.0, 0.0, 0.0, 30, 100, 0)
    }

    #[test]
    fn network_quality_protects_the_worse_direction_and_tolerates_old_nodes() {
        let mut q = rated(40.0, 0.0);
        q.uplink_loss_percent = 4.0;
        q.receivers_loss_percent = 11.0;
        assert_eq!(q.protect_loss_percent(), 11.0);
        q.receivers_loss_percent = f32::NAN;
        assert_eq!(q.protect_loss_percent(), 4.0);
        q.receivers_loss_percent = 11.0;
        let mut json: serde_json::Value = serde_json::to_value(q).unwrap();
        json.as_object_mut()
            .unwrap()
            .remove("receivers_loss_percent");
        let old: NetworkQuality = serde_json::from_value(json).unwrap();
        assert_eq!(old.receivers_loss_percent, 0.0);
        assert_eq!(old.protect_loss_percent(), 4.0);
    }

    #[test]
    fn quality_accumulator_averages_and_meters() {
        let mut acc = QualityAccumulator::default();
        assert_eq!(acc.summary().samples, 0);
        assert_eq!(acc.summary().mos_avg, 0.0);
        let good = rated(40.0, 0.0);
        let bad = rated(40.0, 25.0);
        acc.record(&good, 2.0);
        acc.record(&good, 2.0);
        acc.record(&bad, 2.0);
        let s = acc.summary();
        assert_eq!(s.samples, 3);
        assert_eq!(s.seconds, 6.0);
        assert_eq!(s.bars, [1, 0, 0, 0, 2]);
        assert_eq!(s.poor_seconds, 2.0);
        assert_eq!(s.mos_min, bad.mos);
        assert_eq!(s.mos_last, bad.mos);
        assert!((s.mos_avg - (2.0 * good.mos + bad.mos) / 3.0).abs() < 1e-4);
        assert_eq!(s.loss_max_percent, 25.0);
        assert!((s.loss_avg_percent - 25.0 / 3.0).abs() < 1e-4);
        assert_eq!(s.rtt_avg_ms, 40.0);
        assert_eq!(s.last, Some(bad));

        let d = acc.take_unmetered();
        assert_eq!(d.samples, 3);
        assert_eq!(d.poor_samples, 1);
        assert_eq!(d.rtt_ms, 120);
        assert_eq!(d.loss_permille, 250);
        assert_eq!(
            d.mos_milli,
            (good.mos * 1000.0).round() as u64 * 2 + (bad.mos * 1000.0).round() as u64
        );
        assert!(acc.take_unmetered().is_empty());

        // A node that adopts the session continues the averages.
        let mut resumed = QualityAccumulator::from_summary(&s);
        resumed.record(&good, 2.0);
        let r = resumed.summary();
        assert_eq!(r.samples, 4);
        assert_eq!(r.bars, [1, 0, 0, 0, 3]);
        assert!((r.mos_avg - (3.0 * good.mos + bad.mos) / 4.0).abs() < 1e-4);
        assert_eq!(r.mos_min, bad.mos);
        assert!(resumed.take_unmetered().samples == 1);
    }

    #[test]
    fn mos_alert_debounces_and_recovers_with_hysteresis() {
        let mut st = MosAlertState::default();
        assert_eq!(st.observe(2.0, 0.0, 3), None, "disabled");
        assert_eq!(st.observe(2.0, 3.1, 3), None);
        assert_eq!(st.observe(2.0, 3.1, 3), None);
        assert_eq!(st.observe(4.0, 3.1, 3), None, "streak broken");
        assert_eq!(st.observe(2.0, 3.1, 3), None);
        assert_eq!(st.observe(2.0, 3.1, 3), None);
        assert_eq!(st.observe(2.0, 3.1, 3), Some(MosTransition::Degraded));
        assert!(st.is_alerting());
        assert_eq!(st.observe(2.0, 3.1, 3), None, "no repeat while alerting");
        // Inside the hysteresis band: neither recovers nor re-alerts.
        assert_eq!(st.observe(3.2, 3.1, 3), None);
        assert_eq!(st.observe(3.2, 3.1, 3), None);
        assert_eq!(st.observe(3.2, 3.1, 3), None);
        assert!(st.is_alerting());
        assert_eq!(st.observe(3.5, 3.1, 3), None);
        assert_eq!(st.observe(3.5, 3.1, 3), None);
        assert_eq!(st.observe(3.5, 3.1, 3), Some(MosTransition::Recovered));
        assert!(!st.is_alerting());
        assert_eq!(st.observe(1.5, 3.1, 1), Some(MosTransition::Degraded));
        assert_eq!(st.observe(f32::NAN, 3.1, 1), None);
    }
}
