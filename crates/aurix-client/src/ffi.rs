//! Stable C ABI over [`Client`]. Generated header: `include/aurix_client.h` (cbindgen).
//!
//! # Ownership
//! * `aurix_client_create` returns a handle owned by the caller; release it with
//!   `aurix_client_destroy` (which disconnects). Never use a handle after destroy.
//! * `aurix_client_poll_event` / `aurix_client_wait_event` return an event owned by the caller;
//!   release it with `aurix_event_free`. Every pointer obtained from `aurix_event_*` accessors
//!   (strings, arrays) belongs to the event and dies with it.
//! * Strings passed *in* are borrowed for the duration of the call; the library copies what it
//!   keeps. All strings are NUL-terminated UTF-8.
//! * `aurix_last_error` returns a thread-local string valid until the next failing call on the
//!   same thread. `aurix_version` is static.
//!
//! # Threads
//! Every `aurix_client_*` function may be called from any thread concurrently, including the
//! audio callbacks (`push_capture`, `mix_output`) — they only take short uncontended locks.
//! The wake callback runs on an internal thread; it must return quickly and must not call
//! `aurix_client_destroy`. Events are never delivered on a callback: pump them with
//! `aurix_client_poll_event` from whatever thread suits the engine (typically the game tick).
//!
//! # Errors
//! Functions returning `AurixResult` set `aurix_last_error` on anything but `AURIX_OK`.
//! `NULL` handles/pointers yield `AURIX_NULL_POINTER` instead of crashing.
//!
//! Safety contract for every `unsafe extern "C"` function here: pointers are either `NULL` or
//! valid for the documented type/length for the duration of the call, and handles/events are
//! not used after being destroyed/freed.
#![allow(clippy::missing_safety_doc)]

use aurix_common::protocol::{
    ChatMessage, TransmissionMode, TtsDestination, TtsState, UserPosition,
};
use aurix_common::types::{
    ActionKind, AudioCodec, AudioPolicy, ChannelId, ChannelRole, DownlinkMode, DuckingConfig,
    NetworkQuality, OpusBandwidth, OpusSignal, Orientation3D, Position3D, RecordingConsent, UserId,
};
use std::cell::RefCell;
use std::ffi::{c_char, c_void, CStr, CString};
use std::ptr;
use std::sync::Arc;
use std::time::Duration;
use uuid::Uuid;

use crate::audio::{DecoderSettings, EncoderSettings};
use crate::client::Client;
use crate::config::ClientConfig;
use crate::dsp::{DspConfig, DspStats, NoiseSuppression};
use crate::effects::{
    CallbackEffect, EffectChain, EffectPreset, VoiceEffectParams, MAX_DISTORTION_DRIVE,
    MAX_FILTER_HZ, MAX_FORMANT_SEMITONES, MAX_PITCH_SEMITONES, MAX_RING_MOD_HZ, MAX_TREMOLO_HZ,
    MIN_FILTER_HZ,
};
use crate::error::ClientError;
use crate::events::{ChannelScope, ChatScope, ConnectionState, Event};
use crate::media::{MediaPath, MediaPathPolicy};
use crate::resilience::{LossAdaptation, LossProfile};
use crate::visemes::{Viseme, VisemeAnalyzer, VisemeFrame, VISEME_COUNT};

// ------------------------------------------------------------------------------- results

/// Return code of fallible calls; details via `aurix_last_error`.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AurixResult {
    AurixOk = 0,
    AurixNullPointer = 1,
    AurixInvalidArgument = 2,
    AurixNotConnected = 3,
    AurixClosed = 4,
    AurixTransport = 5,
    AurixUnauthorized = 6,
    AurixTimeout = 7,
    AurixServerRejected = 8,
    AurixCodec = 9,
    AurixProtocol = 10,
}

thread_local! {
    static LAST_ERROR: RefCell<CString> = RefCell::new(CString::default());
}

fn set_error(msg: &str) {
    let c = CString::new(msg.replace('\0', " ")).unwrap_or_default();
    LAST_ERROR.with(|e| *e.borrow_mut() = c);
}

fn fail(e: ClientError) -> AurixResult {
    set_error(&e.to_string());
    match e {
        ClientError::InvalidArgument(_) => AurixResult::AurixInvalidArgument,
        ClientError::NotConnected => AurixResult::AurixNotConnected,
        ClientError::Closed => AurixResult::AurixClosed,
        ClientError::Transport(_) => AurixResult::AurixTransport,
        ClientError::Unauthorized(_) => AurixResult::AurixUnauthorized,
        ClientError::Timeout(_) => AurixResult::AurixTimeout,
        ClientError::Server { .. } => AurixResult::AurixServerRejected,
        ClientError::Codec(_) => AurixResult::AurixCodec,
        ClientError::Protocol(_) => AurixResult::AurixProtocol,
    }
}

fn null_ptr(what: &str) -> AurixResult {
    set_error(&format!("{what} is NULL"));
    AurixResult::AurixNullPointer
}

fn ok<T>(r: crate::error::Result<T>) -> AurixResult {
    match r {
        Ok(_) => AurixResult::AurixOk,
        Err(e) => fail(e),
    }
}

/// Message of the last error raised on the calling thread (empty string if none).
/// Valid until the next failing call on this thread.
#[no_mangle]
pub extern "C" fn aurix_last_error() -> *const c_char {
    LAST_ERROR.with(|e| e.borrow().as_ptr())
}

/// Library version (`CARGO_PKG_VERSION`), static.
#[no_mangle]
pub extern "C" fn aurix_version() -> *const c_char {
    static VERSION: &[u8] = concat!(env!("CARGO_PKG_VERSION"), "\0").as_bytes();
    VERSION.as_ptr() as *const c_char
}

// ---------------------------------------------------------------------------------- ids

/// 128-bit id (session, channel, user, recording) in RFC 4122 byte order.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AurixUuid {
    pub bytes: [u8; 16],
}

impl From<Uuid> for AurixUuid {
    fn from(u: Uuid) -> Self {
        Self {
            bytes: *u.as_bytes(),
        }
    }
}

impl From<AurixUuid> for Uuid {
    fn from(u: AurixUuid) -> Self {
        Uuid::from_bytes(u.bytes)
    }
}

/// Parse a canonical/hyphenated/simple UUID string.
#[no_mangle]
pub unsafe extern "C" fn aurix_uuid_parse(text: *const c_char, out: *mut AurixUuid) -> AurixResult {
    if text.is_null() || out.is_null() {
        return null_ptr("text/out");
    }
    match CStr::from_ptr(text)
        .to_str()
        .ok()
        .and_then(|s| Uuid::parse_str(s).ok())
    {
        Some(u) => {
            *out = u.into();
            AurixResult::AurixOk
        }
        None => {
            set_error("invalid UUID");
            AurixResult::AurixInvalidArgument
        }
    }
}

/// Write the hyphenated form into `out` (at least `AURIX_UUID_STRING_LEN` bytes incl. NUL).
#[no_mangle]
pub unsafe extern "C" fn aurix_uuid_format(
    id: *const AurixUuid,
    out: *mut c_char,
    out_len: usize,
) -> AurixResult {
    if id.is_null() || out.is_null() {
        return null_ptr("id/out");
    }
    if out_len < AURIX_UUID_STRING_LEN {
        set_error("buffer too small");
        return AurixResult::AurixInvalidArgument;
    }
    let s = Uuid::from(*id).hyphenated().to_string();
    ptr::copy_nonoverlapping(s.as_ptr(), out as *mut u8, s.len());
    *out.add(s.len()) = 0;
    AurixResult::AurixOk
}

/// Bytes needed for `aurix_uuid_format` (36 characters + NUL).
pub const AURIX_UUID_STRING_LEN: usize = 37;
/// Capacity of fixed-size UTF-8 name buffers in this ABI (including the NUL).
pub const AURIX_NAME_LEN: usize = 128;

/// Bit set on the SSRC of a synthesized (text-to-speech) voice; the base SSRC is the owner's.
pub const AURIX_SYNTH_SSRC_FLAG: u32 = 0x8000_0000;
/// Everything on the wire is Opus at 48 kHz; `mix_output` produces this rate.
pub const AURIX_SAMPLE_RATE: u32 = 48_000;
/// Samples per channel in one 20 ms frame at `AURIX_SAMPLE_RATE`.
pub const AURIX_FRAME_SAMPLES: u32 = 960;
/// Longest echo tail `AurixDspConfig::echo_tail_ms` accepts (ms).
pub const AURIX_DSP_MAX_ECHO_TAIL_MS: u32 = crate::dsp::MAX_ECHO_TAIL_MS;
/// Largest `AurixDspConfig::stream_delay_ms` (ms).
pub const AURIX_DSP_MAX_STREAM_DELAY_MS: u32 = crate::dsp::MAX_STREAM_DELAY_MS;

#[no_mangle]
pub extern "C" fn aurix_is_synthesized_ssrc(ssrc: u32) -> bool {
    crate::is_synthesized_ssrc(ssrc)
}

#[no_mangle]
pub extern "C" fn aurix_source_ssrc(ssrc: u32) -> u32 {
    crate::source_ssrc(ssrc)
}

// --------------------------------------------------------------------------------- enums

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AurixConnectionState {
    AurixStateDisconnected = 0,
    AurixStateConnecting = 1,
    AurixStateConnected = 2,
    AurixStateMediaBound = 3,
    AurixStateReconnecting = 4,
    AurixStateFailed = 5,
}

impl From<ConnectionState> for AurixConnectionState {
    fn from(s: ConnectionState) -> Self {
        match s {
            ConnectionState::Disconnected => Self::AurixStateDisconnected,
            ConnectionState::Connecting => Self::AurixStateConnecting,
            ConnectionState::Connected => Self::AurixStateConnected,
            ConnectionState::MediaBound => Self::AurixStateMediaBound,
            ConnectionState::Reconnecting => Self::AurixStateReconnecting,
            ConnectionState::Failed => Self::AurixStateFailed,
        }
    }
}

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AurixTransmissionMode {
    /// Microphone reaches no channel.
    AurixTransmitNone = 0,
    /// Microphone reaches one channel (`channel_id` argument).
    AurixTransmitSingle = 1,
    /// Microphone reaches every joined channel.
    AurixTransmitAll = 2,
}

/// Session audio codec (see `aurix_client_set_audio_codec`).
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AurixAudioCodec {
    /// Default: 48 kHz Opus.
    AurixCodecOpus = 0,
    /// G.711 μ-law fallback: 8 kHz, 64 kbit/s, no Opus CPU cost, telephone quality.
    AurixCodecPcmu = 1,
}

impl From<AudioCodec> for AurixAudioCodec {
    fn from(c: AudioCodec) -> Self {
        match c {
            AudioCodec::Opus => Self::AurixCodecOpus,
            AudioCodec::Pcmu => Self::AurixCodecPcmu,
        }
    }
}

impl From<AurixAudioCodec> for AudioCodec {
    fn from(c: AurixAudioCodec) -> Self {
        match c {
            AurixAudioCodec::AurixCodecOpus => Self::Opus,
            AurixAudioCodec::AurixCodecPcmu => Self::Pcmu,
        }
    }
}

/// How the node delivers other speakers to this session (see
/// `aurix_client_set_downlink_mode`).
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AurixDownlinkMode {
    /// Default: one stream per audible speaker, mixed by this client.
    AurixDownlinkStreams = 0,
    /// One server-mixed stereo stream per channel (mutes / volumes / focus / positional gains
    /// applied by the node); E2EE speakers still arrive as separate streams.
    AurixDownlinkMixed = 1,
}

impl From<DownlinkMode> for AurixDownlinkMode {
    fn from(m: DownlinkMode) -> Self {
        match m {
            DownlinkMode::Streams => Self::AurixDownlinkStreams,
            DownlinkMode::Mixed => Self::AurixDownlinkMixed,
        }
    }
}

impl From<AurixDownlinkMode> for DownlinkMode {
    fn from(m: AurixDownlinkMode) -> Self {
        match m {
            AurixDownlinkMode::AurixDownlinkStreams => Self::Streams,
            AurixDownlinkMode::AurixDownlinkMixed => Self::Mixed,
        }
    }
}

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AurixRole {
    AurixRoleListener = 0,
    AurixRoleSpeaker = 1,
    AurixRoleModerator = 2,
    AurixRoleAdministrator = 3,
}

impl From<ChannelRole> for AurixRole {
    fn from(r: ChannelRole) -> Self {
        match r {
            ChannelRole::Listener => Self::AurixRoleListener,
            ChannelRole::Speaker => Self::AurixRoleSpeaker,
            ChannelRole::Moderator => Self::AurixRoleModerator,
            ChannelRole::Administrator => Self::AurixRoleAdministrator,
        }
    }
}

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AurixModerationAction {
    AurixModerationKick = 0,
    AurixModerationMute = 1,
    AurixModerationUnmute = 2,
}

impl From<AurixModerationAction> for ActionKind {
    fn from(a: AurixModerationAction) -> Self {
        match a {
            AurixModerationAction::AurixModerationKick => ActionKind::Kick,
            AurixModerationAction::AurixModerationMute => ActionKind::Mute,
            AurixModerationAction::AurixModerationUnmute => ActionKind::Unmute,
        }
    }
}

fn action_to_c(a: ActionKind) -> AurixModerationAction {
    match a {
        ActionKind::Mute => AurixModerationAction::AurixModerationMute,
        ActionKind::Unmute => AurixModerationAction::AurixModerationUnmute,
        ActionKind::Kick | ActionKind::Login | ActionKind::Join => {
            AurixModerationAction::AurixModerationKick
        }
    }
}

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AurixTtsDestination {
    /// Heard by the channel (as this participant's voice) and locally.
    AurixTtsBoth = 0,
    AurixTtsChannel = 1,
    AurixTtsLocal = 2,
}

impl From<AurixTtsDestination> for TtsDestination {
    fn from(d: AurixTtsDestination) -> Self {
        match d {
            AurixTtsDestination::AurixTtsBoth => TtsDestination::Both,
            AurixTtsDestination::AurixTtsChannel => TtsDestination::Channel,
            AurixTtsDestination::AurixTtsLocal => TtsDestination::Local,
        }
    }
}

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AurixTtsState {
    AurixTtsQueued = 0,
    AurixTtsPlaying = 1,
    AurixTtsFinished = 2,
    AurixTtsCancelled = 3,
    AurixTtsFailed = 4,
}

impl From<TtsState> for AurixTtsState {
    fn from(s: TtsState) -> Self {
        match s {
            TtsState::Queued => Self::AurixTtsQueued,
            TtsState::Playing => Self::AurixTtsPlaying,
            TtsState::Finished => Self::AurixTtsFinished,
            TtsState::Cancelled => Self::AurixTtsCancelled,
            TtsState::Failed => Self::AurixTtsFailed,
        }
    }
}

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AurixRecordingConsent {
    AurixConsentAccepted = 0,
    AurixConsentDeclined = 1,
}

// -------------------------------------------------------------------------------- config

/// Opus coding bandwidth (`OPUS_SET_MAX_BANDWIDTH`), widest band the encoder may use.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AurixOpusBandwidth {
    /// 4 kHz audio band.
    AurixBandwidthNarrowband = 0,
    /// 6 kHz.
    AurixBandwidthMediumband = 1,
    /// 8 kHz.
    AurixBandwidthWideband = 2,
    /// 12 kHz.
    AurixBandwidthSuperwideband = 3,
    /// 20 kHz.
    AurixBandwidthFullband = 4,
}

impl From<OpusBandwidth> for AurixOpusBandwidth {
    fn from(b: OpusBandwidth) -> Self {
        match b {
            OpusBandwidth::Narrowband => Self::AurixBandwidthNarrowband,
            OpusBandwidth::Mediumband => Self::AurixBandwidthMediumband,
            OpusBandwidth::Wideband => Self::AurixBandwidthWideband,
            OpusBandwidth::Superwideband => Self::AurixBandwidthSuperwideband,
            OpusBandwidth::Fullband => Self::AurixBandwidthFullband,
        }
    }
}

impl From<AurixOpusBandwidth> for OpusBandwidth {
    fn from(b: AurixOpusBandwidth) -> Self {
        match b {
            AurixOpusBandwidth::AurixBandwidthNarrowband => Self::Narrowband,
            AurixOpusBandwidth::AurixBandwidthMediumband => Self::Mediumband,
            AurixOpusBandwidth::AurixBandwidthWideband => Self::Wideband,
            AurixOpusBandwidth::AurixBandwidthSuperwideband => Self::Superwideband,
            AurixOpusBandwidth::AurixBandwidthFullband => Self::Fullband,
        }
    }
}

/// Opus content hint (`OPUS_SET_SIGNAL`).
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AurixOpusSignal {
    AurixSignalAuto = 0,
    AurixSignalVoice = 1,
    AurixSignalMusic = 2,
}

impl From<OpusSignal> for AurixOpusSignal {
    fn from(s: OpusSignal) -> Self {
        match s {
            OpusSignal::Auto => Self::AurixSignalAuto,
            OpusSignal::Voice => Self::AurixSignalVoice,
            OpusSignal::Music => Self::AurixSignalMusic,
        }
    }
}

impl From<AurixOpusSignal> for OpusSignal {
    fn from(s: AurixOpusSignal) -> Self {
        match s {
            AurixOpusSignal::AurixSignalAuto => Self::Auto,
            AurixOpusSignal::AurixSignalVoice => Self::Voice,
            AurixOpusSignal::AurixSignalMusic => Self::Music,
        }
    }
}

/// Uplink Opus encoder settings. Out-of-range values are clamped, never rejected.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AurixEncoderSettings {
    /// 6000..=300000 for mono, ..=510000 for stereo (libopus' ceilings).
    pub bitrate_bps: u32,
    /// 0..=10 (10 = best quality, most CPU).
    pub complexity: u8,
    pub max_bandwidth: AurixOpusBandwidth,
    pub signal: AurixOpusSignal,
    /// Variable bitrate; `false` = hard CBR.
    pub vbr: bool,
    /// Constrained VBR (frames stay within the bitrate's byte budget).
    pub constrained_vbr: bool,
    /// In-band forward error correction.
    pub fec: bool,
    /// Loss the FEC is tuned for, 0..=100 %.
    pub expected_loss_percent: u8,
    /// Discontinuous transmission during silence.
    pub dtx: bool,
    /// 1 (mono voice, default) or 2 (stereo music / broadcast). Only honoured in channels
    /// whose policy allows `stereo`; PCMU is always mono. Other values are read as 1.
    pub channels: u8,
    /// Deep REDundancy (libopus 1.5+): history each packet carries a neural low-rate copy
    /// of, 0 (off) ..= 1040 ms in 10 ms steps. Lets receivers rebuild bursts of lost
    /// frames; the automatic loss profile turns it on under heavy loss. Reads back 0 on a
    /// libopus without DRED.
    pub dred_duration_ms: u16,
}

impl From<EncoderSettings> for AurixEncoderSettings {
    fn from(s: EncoderSettings) -> Self {
        Self {
            bitrate_bps: s.bitrate_bps,
            complexity: s.complexity,
            max_bandwidth: s.max_bandwidth.into(),
            signal: s.signal.into(),
            vbr: s.vbr,
            constrained_vbr: s.constrained_vbr,
            fec: s.fec,
            expected_loss_percent: s.expected_loss_percent,
            dtx: s.dtx,
            channels: s.channels,
            dred_duration_ms: s.dred_duration_ms,
        }
    }
}

impl From<AurixEncoderSettings> for EncoderSettings {
    fn from(s: AurixEncoderSettings) -> Self {
        EncoderSettings {
            bitrate_bps: s.bitrate_bps,
            complexity: s.complexity,
            max_bandwidth: s.max_bandwidth.into(),
            signal: s.signal.into(),
            vbr: s.vbr,
            constrained_vbr: s.constrained_vbr,
            fec: s.fec,
            expected_loss_percent: s.expected_loss_percent,
            dtx: s.dtx,
            channels: s.channels,
            dred_duration_ms: s.dred_duration_ms,
        }
        .clamped()
    }
}

/// Downlink Opus decoder tuning (libopus 1.5+ neural paths), shared by every remote stream.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AurixDecoderSettings {
    /// 0..=10. >= 5 conceals lost frames with the neural PLC (default), >= 6 adds OSCE LACE
    /// speech enhancement of SILK frames, >= 7 NoLACE (best, several times LACE's CPU).
    pub complexity: u8,
    /// OSCE bandwidth extension of wideband speech to fullband (needs complexity >= 4).
    pub osce_bwe: bool,
}

impl From<DecoderSettings> for AurixDecoderSettings {
    fn from(s: DecoderSettings) -> Self {
        Self {
            complexity: s.complexity,
            osce_bwe: s.osce_bwe,
        }
    }
}

impl From<AurixDecoderSettings> for DecoderSettings {
    fn from(s: AurixDecoderSettings) -> Self {
        DecoderSettings {
            complexity: s.complexity,
            osce_bwe: s.osce_bwe,
        }
        .clamped()
    }
}

/// Redundancy tier of the uplink encoder (see `aurix_client_set_loss_adaptation`).
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AurixLossProfile {
    /// Clean link: the baseline settings as they are.
    #[default]
    AurixLossProfileLow = 0,
    /// In-band FEC on, tuned for >= 10 % (or the measured loss).
    AurixLossProfileModerate = 1,
    /// FEC tuned for >= 20 %, DRED covering 400 ms, bitrate lifted to 28 kbit/s where the
    /// channel target / server command allows.
    AurixLossProfileHigh = 2,
}

impl From<LossProfile> for AurixLossProfile {
    fn from(p: LossProfile) -> Self {
        match p {
            LossProfile::Low => Self::AurixLossProfileLow,
            LossProfile::Moderate => Self::AurixLossProfileModerate,
            LossProfile::High => Self::AurixLossProfileHigh,
        }
    }
}

impl From<AurixLossProfile> for LossProfile {
    fn from(p: AurixLossProfile) -> Self {
        match p {
            AurixLossProfile::AurixLossProfileLow => Self::Low,
            AurixLossProfile::AurixLossProfileModerate => Self::Moderate,
            AurixLossProfile::AurixLossProfileHigh => Self::High,
        }
    }
}

/// How the uplink loss profile is chosen.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AurixLossAdaptation {
    /// From the server's uplink-loss reports: low → moderate at 3 %, → high at 10 %; back
    /// below 1 % / 5 % after a 6 s dwell (default).
    AurixLossAdaptationAuto = 0,
    AurixLossAdaptationFixedLow = 1,
    AurixLossAdaptationFixedModerate = 2,
    AurixLossAdaptationFixedHigh = 3,
}

impl From<LossAdaptation> for AurixLossAdaptation {
    fn from(a: LossAdaptation) -> Self {
        match a {
            LossAdaptation::Auto => Self::AurixLossAdaptationAuto,
            LossAdaptation::Fixed(LossProfile::Low) => Self::AurixLossAdaptationFixedLow,
            LossAdaptation::Fixed(LossProfile::Moderate) => Self::AurixLossAdaptationFixedModerate,
            LossAdaptation::Fixed(LossProfile::High) => Self::AurixLossAdaptationFixedHigh,
        }
    }
}

impl From<AurixLossAdaptation> for LossAdaptation {
    fn from(a: AurixLossAdaptation) -> Self {
        match a {
            AurixLossAdaptation::AurixLossAdaptationAuto => Self::Auto,
            AurixLossAdaptation::AurixLossAdaptationFixedLow => Self::Fixed(LossProfile::Low),
            AurixLossAdaptation::AurixLossAdaptationFixedModerate => {
                Self::Fixed(LossProfile::Moderate)
            }
            AurixLossAdaptation::AurixLossAdaptationFixedHigh => Self::Fixed(LossProfile::High),
        }
    }
}

/// A channel's audio policy as set by the operator (`ChannelConfig`), merged across the
/// joined channels. `complexity < 0` = no hint.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AurixAudioPolicy {
    /// Target uplink bitrate.
    pub bitrate_bps: u32,
    /// Floor the server's adaptive bitrate never goes below.
    pub min_bitrate_bps: u32,
    pub fec: bool,
    pub dtx: bool,
    pub max_bandwidth: AurixOpusBandwidth,
    pub complexity: i8,
    pub signal: AurixOpusSignal,
    /// Senders may encode two channels (stereo music / broadcast); `false` asks for mono.
    pub stereo: bool,
}

impl From<AudioPolicy> for AurixAudioPolicy {
    fn from(p: AudioPolicy) -> Self {
        Self {
            bitrate_bps: p.bitrate_bps,
            min_bitrate_bps: p.min_bitrate_bps,
            fec: p.fec,
            dtx: p.dtx,
            max_bandwidth: p.max_bandwidth.into(),
            complexity: p.complexity.map_or(-1, |c| c.min(10) as i8),
            signal: p.signal.into(),
            stereo: p.stereo,
        }
    }
}

/// Connection parameters. Fill with `aurix_client_config_default`, then set `ws_url`/`token`.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct AurixClientConfig {
    /// `ws://host:port/ws` or `wss://…` (required, borrowed).
    pub ws_url: *const c_char,
    /// Session JWT or one-time `login` action token (required, borrowed; copied).
    pub token: *const c_char,
    pub auto_reconnect: bool,
    /// 0 = never give up.
    pub reconnect_max_attempts: u32,
    pub reconnect_initial_delay_ms: u32,
    pub reconnect_max_delay_ms: u32,
    pub request_timeout_ms: u32,
    pub ping_interval_ms: u32,
    pub heartbeat_interval_ms: u32,
    /// Uplink Opus encoder before any channel policy applies.
    pub encoder: AurixEncoderSettings,
    /// Adopt joined channels' audio policy (bitrate/FEC/DTX/bandwidth/signal and the complexity
    /// hint unless pinned with `aurix_client_set_complexity`). Server bitrate commands apply
    /// either way.
    pub follow_channel_policy: bool,
    /// Jitter buffer depth before playout starts (20 ms frames).
    pub jitter_target_frames: u32,
    pub jitter_max_frames: u32,
    /// Only send frames the voice activity detector marks as speech.
    pub vad_gate: bool,
    /// Tokio worker threads for the control plane (1 is plenty).
    pub worker_threads: u32,
    /// Capture DSP (high-pass / echo cancellation / noise suppression / AGC) before the VAD
    /// and encoder; `aurix_dsp_config_default` = everything on, `aurix_dsp_config_bypass` for
    /// hosts with their own processing. Changeable later with `aurix_client_set_dsp`.
    pub dsp: AurixDspConfig,
    /// Which link carries media: QUIC (when the node offers it) then UDP, with the TLS tunnel
    /// (port 443) and then the WebSocket tunnel as fallbacks (default), or exactly one of them.
    pub media_path: AurixMediaPathPolicy,
    /// `Auto`: unanswered QUIC/UDP heartbeats in a row before media moves to a tunnel (0 =
    /// never fall back mid-session; the default 3 ≈ 15 s with 5 s heartbeats).
    pub udp_fallback_lost_heartbeats: u32,
    /// `Auto`: how often a tunnelled session re-probes the native links (QUIC, then UDP) and
    /// moves back when one answers (0 = never; stays tunnelled until the next connect).
    pub udp_reprobe_interval_ms: u32,
    /// Take part in end-to-end encrypted channels (identity key, sender-key exchange). Off,
    /// joining an `e2ee` channel fails with `E2EE_REQUIRED`.
    pub e2ee: bool,
    /// `true`: `e2ee_identity` holds a persisted 32-byte X25519 secret (stable fingerprint
    /// across runs); `false`: a fresh identity per client.
    pub has_e2ee_identity: bool,
    pub e2ee_identity: [u8; 32],
    /// Downlink decoder tuning (neural PLC / OSCE); `aurix_client_set_decoder_settings`
    /// changes it later.
    pub decoder: AurixDecoderSettings,
    /// Uplink redundancy: adapt FEC / DRED to the server's loss reports (default) or pin a
    /// tier; `aurix_client_set_loss_adaptation` changes it later.
    pub loss_adaptation: AurixLossAdaptation,
    /// `Auto`: try QUIC before raw UDP when the node offers it (default true). Off, `Auto`
    /// is UDP → tunnel as before; `AurixMediaPathQuicOnly` ignores this switch.
    pub quic: bool,
    /// `Auto`: when neither QUIC nor UDP binds, try the node's dedicated TLS tunnel port
    /// (normally 443) before the WebSocket tunnel (default true); `AurixMediaPathTlsOnly`
    /// ignores this switch.
    pub tls_tunnel: bool,
}

#[no_mangle]
pub unsafe extern "C" fn aurix_client_config_default(out: *mut AurixClientConfig) {
    if out.is_null() {
        return;
    }
    let d = ClientConfig::new("", "");
    *out = AurixClientConfig {
        ws_url: ptr::null(),
        token: ptr::null(),
        auto_reconnect: d.auto_reconnect,
        reconnect_max_attempts: d.reconnect.max_attempts,
        reconnect_initial_delay_ms: d.reconnect.initial_delay.as_millis() as u32,
        reconnect_max_delay_ms: d.reconnect.max_delay.as_millis() as u32,
        request_timeout_ms: d.request_timeout.as_millis() as u32,
        ping_interval_ms: d.ping_interval.as_millis() as u32,
        heartbeat_interval_ms: d.heartbeat_interval.as_millis() as u32,
        encoder: d.encoder.into(),
        follow_channel_policy: d.follow_channel_policy,
        jitter_target_frames: d.jitter_target_frames as u32,
        jitter_max_frames: d.jitter_max_frames as u32,
        vad_gate: d.vad_gate,
        worker_threads: d.worker_threads as u32,
        dsp: d.dsp.into(),
        media_path: d.media_path.into(),
        udp_fallback_lost_heartbeats: d.udp_fallback_lost_heartbeats,
        udp_reprobe_interval_ms: d.udp_reprobe_interval.as_millis() as u32,
        e2ee: d.e2ee,
        has_e2ee_identity: false,
        e2ee_identity: [0; 32],
        decoder: d.decoder.into(),
        loss_adaptation: d.loss_adaptation.into(),
        quic: d.quic,
        tls_tunnel: d.tls_tunnel,
    };
}

/// Media link selection policy (`AurixClientConfig::media_path`).
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AurixMediaPathPolicy {
    /// QUIC (when offered and `quic`), then UDP; the TLS tunnel (when offered and
    /// `tls_tunnel`) and then the WebSocket tunnel when both are blocked; back to a native
    /// link when it answers again.
    AurixMediaPathAuto = 0,
    AurixMediaPathUdpOnly = 1,
    AurixMediaPathTunnelOnly = 2,
    /// QUIC only; a node without QUIC (or a blocked media port) fails the connection.
    AurixMediaPathQuicOnly = 3,
    /// TLS tunnel only; a node without the tunnel port (or a blocked port) fails the
    /// connection.
    AurixMediaPathTlsOnly = 4,
}

impl From<MediaPathPolicy> for AurixMediaPathPolicy {
    fn from(p: MediaPathPolicy) -> Self {
        match p {
            MediaPathPolicy::Auto => Self::AurixMediaPathAuto,
            MediaPathPolicy::UdpOnly => Self::AurixMediaPathUdpOnly,
            MediaPathPolicy::TunnelOnly => Self::AurixMediaPathTunnelOnly,
            MediaPathPolicy::QuicOnly => Self::AurixMediaPathQuicOnly,
            MediaPathPolicy::TlsOnly => Self::AurixMediaPathTlsOnly,
        }
    }
}

impl From<AurixMediaPathPolicy> for MediaPathPolicy {
    fn from(p: AurixMediaPathPolicy) -> Self {
        match p {
            AurixMediaPathPolicy::AurixMediaPathAuto => Self::Auto,
            AurixMediaPathPolicy::AurixMediaPathUdpOnly => Self::UdpOnly,
            AurixMediaPathPolicy::AurixMediaPathTunnelOnly => Self::TunnelOnly,
            AurixMediaPathPolicy::AurixMediaPathQuicOnly => Self::QuicOnly,
            AurixMediaPathPolicy::AurixMediaPathTlsOnly => Self::TlsOnly,
        }
    }
}

/// Link the media currently travels over.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AurixMediaPath {
    /// No media link yet (before `AurixEventMediaBound`).
    #[default]
    AurixMediaNone = 0,
    /// Native AURX over UDP.
    AurixMediaUdp = 1,
    /// AURX packets as binary frames on the control WebSocket (TCP; higher latency under
    /// loss).
    AurixMediaTunnel = 2,
    /// Native AURX as QUIC datagrams (0-RTT reconnect, connection migration on
    /// `aurix_client_network_changed`).
    AurixMediaQuic = 3,
    /// WebRTC; never reported by the native core. Reserved so the browser bridges (Unity
    /// WebGL, Godot Web) share one numbering with the native SDKs.
    AurixMediaWebrtc = 4,
    /// AURX packets as frames on a TLS connection to the node's dedicated tunnel port
    /// (normally 443; TCP, higher latency under loss).
    AurixMediaTls = 5,
}

impl From<Option<MediaPath>> for AurixMediaPath {
    fn from(p: Option<MediaPath>) -> Self {
        match p {
            None => Self::AurixMediaNone,
            Some(MediaPath::Udp) => Self::AurixMediaUdp,
            Some(MediaPath::Tunnel) => Self::AurixMediaTunnel,
            Some(MediaPath::Quic) => Self::AurixMediaQuic,
            Some(MediaPath::Tls) => Self::AurixMediaTls,
        }
    }
}

/// Noise suppression strength (dry/wet blend of the RNNoise output).
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AurixNoiseSuppression {
    AurixNoiseSuppressionOff = 0,
    AurixNoiseSuppressionLow = 1,
    AurixNoiseSuppressionModerate = 2,
    AurixNoiseSuppressionHigh = 3,
}

impl From<NoiseSuppression> for AurixNoiseSuppression {
    fn from(n: NoiseSuppression) -> Self {
        match n {
            NoiseSuppression::Off => Self::AurixNoiseSuppressionOff,
            NoiseSuppression::Low => Self::AurixNoiseSuppressionLow,
            NoiseSuppression::Moderate => Self::AurixNoiseSuppressionModerate,
            NoiseSuppression::High => Self::AurixNoiseSuppressionHigh,
        }
    }
}

impl From<AurixNoiseSuppression> for NoiseSuppression {
    fn from(n: AurixNoiseSuppression) -> Self {
        match n {
            AurixNoiseSuppression::AurixNoiseSuppressionOff => Self::Off,
            AurixNoiseSuppression::AurixNoiseSuppressionLow => Self::Low,
            AurixNoiseSuppression::AurixNoiseSuppressionModerate => Self::Moderate,
            AurixNoiseSuppression::AurixNoiseSuppressionHigh => Self::High,
        }
    }
}

/// Capture DSP configuration. Out-of-range values are clamped, never rejected. The chain adds
/// 10 ms of latency when the echo canceller is on; everything off is a pass-through.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct AurixDspConfig {
    /// 80 Hz second-order high-pass.
    pub high_pass: bool,
    /// Acoustic echo cancellation against the audio fed by `aurix_client_mix_output_*` /
    /// `aurix_client_push_render_*`.
    pub echo_cancellation: bool,
    /// Echo tail modelled by the adaptive filter, 40..=`AURIX_DSP_MAX_ECHO_TAIL_MS`.
    pub echo_tail_ms: u32,
    /// Initial playback→capture delay hint, 0..=`AURIX_DSP_MAX_STREAM_DELAY_MS`; refined at
    /// runtime by the delay estimator.
    pub stream_delay_ms: u32,
    pub noise_suppression: AurixNoiseSuppression,
    /// Speech-gated automatic gain control with a soft limiter.
    pub agc: bool,
    /// AGC target speech level, dBFS RMS, -30..=-6.
    pub agc_target_dbfs: f32,
    /// Maximum AGC boost, 0..=40 dB.
    pub agc_max_gain_db: f32,
}

impl From<DspConfig> for AurixDspConfig {
    fn from(c: DspConfig) -> Self {
        Self {
            high_pass: c.high_pass,
            echo_cancellation: c.echo_cancellation,
            echo_tail_ms: c.echo_tail_ms,
            stream_delay_ms: c.stream_delay_ms,
            noise_suppression: c.noise_suppression.into(),
            agc: c.agc,
            agc_target_dbfs: c.agc_target_dbfs,
            agc_max_gain_db: c.agc_max_gain_db,
        }
    }
}

impl From<AurixDspConfig> for DspConfig {
    fn from(c: AurixDspConfig) -> Self {
        Self {
            high_pass: c.high_pass,
            echo_cancellation: c.echo_cancellation,
            echo_tail_ms: c.echo_tail_ms,
            stream_delay_ms: c.stream_delay_ms,
            noise_suppression: c.noise_suppression.into(),
            agc: c.agc,
            agc_target_dbfs: c.agc_target_dbfs,
            agc_max_gain_db: c.agc_max_gain_db,
        }
        .clamped()
    }
}

/// Everything on (high-pass, AEC with a 200 ms tail, high noise suppression, AGC to -18 dBFS).
#[no_mangle]
pub unsafe extern "C" fn aurix_dsp_config_default(out: *mut AurixDspConfig) {
    if !out.is_null() {
        *out = DspConfig::default().into();
    }
}

/// Everything off: the capture stream reaches the VAD/encoder untouched.
#[no_mangle]
pub unsafe extern "C" fn aurix_dsp_config_bypass(out: *mut AurixDspConfig) {
    if !out.is_null() {
        *out = DspConfig::BYPASS.into();
    }
}

/// Runtime DSP diagnostics.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct AurixDspStats {
    /// Echo return loss enhancement of the linear filter (dB) while the far end is active.
    pub erle_db: f32,
    /// Playback→capture delay the echo canceller is aligned to (ms).
    pub echo_delay_ms: u32,
    /// The adaptive filter has seen enough far-end audio to have converged.
    pub echo_converged: bool,
    /// Rendered audio was fed in the last second.
    pub far_end_active: bool,
    /// Speech probability of the last 10 ms block (`0..=1`).
    pub speech_probability: f32,
    /// Current AGC gain (dB; 0 when AGC is off).
    pub agc_gain_db: f32,
    /// Blocks processed without reference audio while the AEC was on and being fed.
    pub far_end_underruns: u64,
}

impl From<DspStats> for AurixDspStats {
    fn from(s: DspStats) -> Self {
        Self {
            erle_db: s.erle_db,
            echo_delay_ms: s.echo_delay_ms,
            echo_converged: s.echo_converged,
            far_end_active: s.far_end_active,
            speech_probability: s.speech_probability,
            agc_gain_db: s.agc_gain_db,
            far_end_underruns: s.far_end_underruns,
        }
    }
}

unsafe fn cstr_arg(p: *const c_char, what: &str) -> Result<String, AurixResult> {
    if p.is_null() {
        return Err(null_ptr(what));
    }
    CStr::from_ptr(p).to_str().map(str::to_string).map_err(|_| {
        set_error(&format!("{what} is not valid UTF-8"));
        AurixResult::AurixInvalidArgument
    })
}

unsafe fn opt_cstr_arg(p: *const c_char, what: &str) -> Result<Option<String>, AurixResult> {
    if p.is_null() {
        Ok(None)
    } else {
        cstr_arg(p, what).map(Some)
    }
}

unsafe fn uuid_arg(p: *const AurixUuid, what: &str) -> Result<Uuid, AurixResult> {
    if p.is_null() {
        return Err(null_ptr(what));
    }
    Ok(Uuid::from(*p))
}

unsafe fn opt_uuid_arg(p: *const AurixUuid) -> Option<Uuid> {
    if p.is_null() {
        None
    } else {
        Some(Uuid::from(*p))
    }
}

fn build_config(c: &AurixClientConfig) -> Result<ClientConfig, AurixResult> {
    let ws_url = unsafe { cstr_arg(c.ws_url, "ws_url")? };
    let token = unsafe { cstr_arg(c.token, "token")? };
    let mut cfg = ClientConfig::new(ws_url, token);
    cfg.auto_reconnect = c.auto_reconnect;
    cfg.reconnect.max_attempts = if c.reconnect_max_attempts == 0 {
        u32::MAX
    } else {
        c.reconnect_max_attempts
    };
    cfg.reconnect.initial_delay =
        Duration::from_millis(c.reconnect_initial_delay_ms.max(50) as u64);
    cfg.reconnect.max_delay = Duration::from_millis(
        c.reconnect_max_delay_ms
            .max(c.reconnect_initial_delay_ms.max(50)) as u64,
    );
    cfg.request_timeout = Duration::from_millis(c.request_timeout_ms.max(500) as u64);
    cfg.ping_interval = Duration::from_millis(c.ping_interval_ms.max(1000) as u64);
    cfg.heartbeat_interval = Duration::from_millis(c.heartbeat_interval_ms.max(500) as u64);
    cfg.encoder = c.encoder.into();
    cfg.follow_channel_policy = c.follow_channel_policy;
    cfg.jitter_target_frames = c.jitter_target_frames.clamp(1, 50) as usize;
    cfg.jitter_max_frames = c
        .jitter_max_frames
        .clamp(cfg.jitter_target_frames as u32, 100) as usize;
    cfg.vad_gate = c.vad_gate;
    cfg.worker_threads = c.worker_threads.clamp(1, 8) as usize;
    cfg.dsp = c.dsp.into();
    cfg.media_path = c.media_path.into();
    cfg.udp_fallback_lost_heartbeats = c.udp_fallback_lost_heartbeats;
    cfg.udp_reprobe_interval = Duration::from_millis(c.udp_reprobe_interval_ms as u64);
    cfg.e2ee = c.e2ee;
    cfg.e2ee_identity = c.has_e2ee_identity.then_some(c.e2ee_identity);
    cfg.decoder = c.decoder.into();
    cfg.loss_adaptation = c.loss_adaptation.into();
    cfg.quic = c.quic;
    cfg.tls_tunnel = c.tls_tunnel;
    Ok(cfg)
}

// -------------------------------------------------------------------------------- handle

/// Opaque voice client. One per player/session.
pub struct AurixClient {
    client: Client,
    effects: parking_lot::Mutex<VoiceEffectsState>,
}

/// Host-provided voice effect: `frame` is one 20 ms 48 kHz interleaved frame
/// (`samples_per_channel * channels` floats in `-1..=1`) to modify in place. Called on the
/// thread that feeds `aurix_client_push_capture_*` — no blocking, no allocation.
pub type AurixVoiceEffectFn = Option<
    unsafe extern "C" fn(
        user_data: *mut c_void,
        frame: *mut f32,
        samples_per_channel: u32,
        channels: u8,
    ),
>;

/// Built-in voice effects (`aurix_client_set_voice_effects`); zero = that stage is off, so a
/// zeroed struct is a bypass. Stages run in field order: filters, formant, pitch, ring
/// modulator, distortion, tremolo, static, reverb, then the host callback. Start from a
/// preset with `aurix_voice_effects_preset`.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct AurixVoiceEffects {
    /// High-pass corner in Hz (`AURIX_MIN_FILTER_HZ..=AURIX_MAX_FILTER_HZ`, 0 = off).
    pub highpass_hz: f32,
    /// Low-pass corner in Hz (0 = off).
    pub lowpass_hz: f32,
    /// Formant (vocal-tract) shift in semitones, pitch unchanged, clamped to
    /// ±`AURIX_MAX_FORMANT_SEMITONES`. Adds ~43 ms of latency while non-zero.
    pub formant_semitones: f32,
    /// Pitch shift in semitones, clamped to ±`AURIX_MAX_PITCH_SEMITONES`.
    pub pitch_semitones: f32,
    /// Ring-modulator ("robot") carrier in Hz, clamped to `0..=AURIX_MAX_RING_MOD_HZ`.
    pub ring_mod_hz: f32,
    /// Saturation drive (`1..=AURIX_MAX_DISTORTION_DRIVE`, 0 = off).
    pub distortion_drive: f32,
    /// Tremolo rate in Hz (`0..=AURIX_MAX_TREMOLO_HZ`, 0 = off).
    pub tremolo_hz: f32,
    /// Tremolo depth `0..=1`.
    pub tremolo_depth: f32,
    /// Static / hiss level `0..=1`, gated by the voice (silence stays silent).
    pub static_level: f32,
    /// Reverb wet mix `0..=1` (0 = off).
    pub reverb_mix: f32,
    /// Reverb room size `0..=1`.
    pub reverb_size: f32,
    /// Reverb high-frequency damping `0..=1`.
    pub reverb_damping: f32,
}

impl From<AurixVoiceEffects> for VoiceEffectParams {
    fn from(e: AurixVoiceEffects) -> Self {
        Self {
            highpass_hz: e.highpass_hz,
            lowpass_hz: e.lowpass_hz,
            formant_semitones: e.formant_semitones,
            pitch_semitones: e.pitch_semitones,
            ring_mod_hz: e.ring_mod_hz,
            distortion_drive: e.distortion_drive,
            tremolo_hz: e.tremolo_hz,
            tremolo_depth: e.tremolo_depth,
            static_level: e.static_level,
            reverb_mix: e.reverb_mix,
            reverb_size: e.reverb_size,
            reverb_damping: e.reverb_damping,
        }
    }
}

impl From<VoiceEffectParams> for AurixVoiceEffects {
    fn from(p: VoiceEffectParams) -> Self {
        Self {
            highpass_hz: p.highpass_hz,
            lowpass_hz: p.lowpass_hz,
            formant_semitones: p.formant_semitones,
            pitch_semitones: p.pitch_semitones,
            ring_mod_hz: p.ring_mod_hz,
            distortion_drive: p.distortion_drive,
            tremolo_hz: p.tremolo_hz,
            tremolo_depth: p.tremolo_depth,
            static_level: p.static_level,
            reverb_mix: p.reverb_mix,
            reverb_size: p.reverb_size,
            reverb_damping: p.reverb_damping,
        }
    }
}

/// Ready-made voices for `aurix_voice_effects_preset`.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AurixVoicePreset {
    /// Ring-modulated, band-limited, slightly saturated.
    AurixVoicePresetRobot = 0,
    /// Pitched and formant-shifted down, growly, in a large space.
    AurixVoicePresetMonster = 1,
    /// Telephone band, crunchy, static under the voice.
    AurixVoicePresetRadio = 2,
    /// Pitched and formant-shifted up.
    AurixVoicePresetHelium = 3,
    /// Hollow, wavering, drenched in reverb.
    AurixVoicePresetGhost = 4,
}

impl From<AurixVoicePreset> for EffectPreset {
    fn from(p: AurixVoicePreset) -> Self {
        match p {
            AurixVoicePreset::AurixVoicePresetRobot => EffectPreset::Robot,
            AurixVoicePreset::AurixVoicePresetMonster => EffectPreset::Monster,
            AurixVoicePreset::AurixVoicePresetRadio => EffectPreset::Radio,
            AurixVoicePreset::AurixVoicePresetHelium => EffectPreset::Helium,
            AurixVoicePreset::AurixVoicePresetGhost => EffectPreset::Ghost,
        }
    }
}

/// Largest built-in pitch shift either way, in semitones.
pub const AURIX_MAX_PITCH_SEMITONES: f32 = 24.0;
/// Highest built-in ring-modulator carrier, in Hz.
pub const AURIX_MAX_RING_MOD_HZ: f32 = 2000.0;
/// Largest built-in formant shift either way, in semitones.
pub const AURIX_MAX_FORMANT_SEMITONES: f32 = 12.0;
/// Hardest distortion drive.
pub const AURIX_MAX_DISTORTION_DRIVE: f32 = 20.0;
/// Fastest tremolo, in Hz.
pub const AURIX_MAX_TREMOLO_HZ: f32 = 20.0;
/// Lowest filter corner, in Hz.
pub const AURIX_MIN_FILTER_HZ: f32 = 20.0;
/// Highest filter corner, in Hz.
pub const AURIX_MAX_FILTER_HZ: f32 = 20000.0;

const _: () = assert!(AURIX_MAX_PITCH_SEMITONES == MAX_PITCH_SEMITONES);
const _: () = assert!(AURIX_MAX_RING_MOD_HZ == MAX_RING_MOD_HZ);
const _: () = assert!(AURIX_MAX_FORMANT_SEMITONES == MAX_FORMANT_SEMITONES);
const _: () = assert!(AURIX_MAX_DISTORTION_DRIVE == MAX_DISTORTION_DRIVE);
const _: () = assert!(AURIX_MAX_TREMOLO_HZ == MAX_TREMOLO_HZ);
const _: () = assert!(AURIX_MIN_FILTER_HZ == MIN_FILTER_HZ);
const _: () = assert!(AURIX_MAX_FILTER_HZ == MAX_FILTER_HZ);

struct HostEffect {
    f: unsafe extern "C" fn(*mut c_void, *mut f32, u32, u8),
    user_data: *mut c_void,
}

// The host promises `user_data` may be used from the capture thread (as with every other
// callback in this ABI).
unsafe impl Send for HostEffect {}

impl HostEffect {
    fn process(&self, frame: &mut [f32], channels: u8) {
        let per_channel = frame.len() / channels.max(1) as usize;
        // SAFETY: `frame` is a live, exclusively borrowed slice for the duration of the
        // call; the host contract is to touch only that many samples.
        unsafe {
            (self.f)(
                self.user_data,
                frame.as_mut_ptr(),
                per_channel as u32,
                channels,
            )
        }
    }
}

#[derive(Default)]
struct VoiceEffectsState {
    builtin: VoiceEffectParams,
    host: Option<HostEffect>,
}

impl VoiceEffectsState {
    /// Built-in stages first, the host callback last.
    fn chain(&self) -> EffectChain {
        let mut chain = self.builtin.chain();
        if let Some(host) = &self.host {
            let holder = HostEffect {
                f: host.f,
                user_data: host.user_data,
            };
            chain.push(Box::new(CallbackEffect::new(
                move |frame: &mut [f32], channels| holder.process(frame, channels),
            )));
        }
        chain
    }
}

unsafe fn client<'a>(h: *const AurixClient) -> Result<&'a Client, AurixResult> {
    if h.is_null() {
        Err(null_ptr("client"))
    } else {
        Ok(&(*h).client)
    }
}

/// Create a client. Returns `NULL` (see `aurix_last_error`) if the config is invalid or the
/// runtime/codec cannot be initialised. Does not connect.
#[no_mangle]
pub unsafe extern "C" fn aurix_client_create(config: *const AurixClientConfig) -> *mut AurixClient {
    if config.is_null() {
        null_ptr("config");
        return ptr::null_mut();
    }
    let cfg = match build_config(&*config) {
        Ok(c) => c,
        Err(_) => return ptr::null_mut(),
    };
    match Client::new(cfg) {
        Ok(client) => Box::into_raw(Box::new(AurixClient {
            client,
            effects: parking_lot::Mutex::new(VoiceEffectsState::default()),
        })),
        Err(e) => {
            fail(e);
            ptr::null_mut()
        }
    }
}

/// Disconnect (if needed) and free the handle. Do not call from the wake callback.
#[no_mangle]
pub unsafe extern "C" fn aurix_client_destroy(client: *mut AurixClient) {
    if !client.is_null() {
        drop(Box::from_raw(client));
    }
}

/// Start connecting; progress is reported through events. Idempotent while connected.
#[no_mangle]
pub unsafe extern "C" fn aurix_client_connect(client: *mut AurixClient) -> AurixResult {
    match self::client(client) {
        Ok(c) => ok(c.connect()),
        Err(r) => r,
    }
}

/// Close the session for good (blocks up to ~3 s for the server to acknowledge).
#[no_mangle]
pub unsafe extern "C" fn aurix_client_disconnect(client: *mut AurixClient) {
    if let Ok(c) = self::client(client) {
        c.disconnect();
    }
}

/// Replace the session token used for the next connect/reconnect and for re-joins.
#[no_mangle]
pub unsafe extern "C" fn aurix_client_set_token(
    client: *mut AurixClient,
    token: *const c_char,
) -> AurixResult {
    let c = match self::client(client) {
        Ok(c) => c,
        Err(r) => return r,
    };
    match cstr_arg(token, "token") {
        Ok(t) => ok(c.set_token(&t)),
        Err(r) => r,
    }
}

#[no_mangle]
pub unsafe extern "C" fn aurix_client_state(client: *const AurixClient) -> AurixConnectionState {
    match self::client(client) {
        Ok(c) => c.state().into(),
        Err(_) => AurixConnectionState::AurixStateDisconnected,
    }
}

/// Facts about the current session.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct AurixSessionInfo {
    pub session_id: AurixUuid,
    /// This client's own SSRC (also the base of its TTS voice SSRC).
    pub ssrc: u32,
    /// How long the server keeps the session resumable after a drop.
    pub resume_grace_ms: u32,
    /// The latest (re)connect resumed the previous session.
    pub resumed: bool,
    /// Zero UUID when the token carries no `sub`.
    pub user_id: AurixUuid,
    /// The node accepts media over the control WebSocket (fallback when UDP is blocked).
    pub media_tunnel: bool,
    /// The node can deliver one server-mixed stream per channel
    /// (`aurix_client_set_downlink_mode`).
    pub downlink_mix: bool,
    /// The latest (re)connect resumed the session on a *different* node (same session id
    /// and SSRC, new media key/endpoint). See `aurix_client_endpoint`.
    pub migrated: bool,
    /// The node translates transcripts for listeners (`aurix_client_set_translation`).
    pub translation: bool,
    /// Translations can also be spoken into this session's downlink.
    pub translation_speech: bool,
    /// The node accepts media as QUIC datagrams on its media port.
    pub media_quic: bool,
    /// The node accepts media as frames on its dedicated TLS tunnel port (normally 443).
    pub media_tls: bool,
}

/// `false` when no session is open.
#[no_mangle]
pub unsafe extern "C" fn aurix_client_session(
    client: *const AurixClient,
    out: *mut AurixSessionInfo,
) -> bool {
    let Ok(c) = self::client(client) else {
        return false;
    };
    if out.is_null() {
        return false;
    }
    match c.session() {
        Some(s) => {
            *out = AurixSessionInfo {
                session_id: s.session_id.0.into(),
                ssrc: s.ssrc,
                resume_grace_ms: s.resume_grace.as_millis().min(u32::MAX as u128) as u32,
                resumed: s.resumed,
                user_id: c.user_id().map(|u| u.0).unwrap_or(Uuid::nil()).into(),
                media_tunnel: s.media_tunnel,
                downlink_mix: s.downlink_mix,
                migrated: s.migrated,
                translation: s.translation.is_some(),
                translation_speech: s.translation.as_ref().is_some_and(|t| t.speech),
                media_quic: s.media_quic,
                media_tls: s.media_tls,
            };
            true
        }
        None => false,
    }
}

/// WebSocket URL of the node serving (or last serving) the session — `AurixConfig.ws_url`
/// until a failover moved it. Returns the number of bytes needed (excluding NUL); `buf` may
/// be `NULL` to size it. 0 for an invalid handle.
#[no_mangle]
pub unsafe extern "C" fn aurix_client_endpoint(
    client: *const AurixClient,
    buf: *mut c_char,
    capacity: usize,
) -> usize {
    match self::client(client) {
        Ok(c) => copy_out(&c.endpoint(), buf, capacity),
        Err(_) => 0,
    }
}

/// Number of alternate nodes advertised by the server for this session (tried in order,
/// after the current node, when the connection drops).
#[no_mangle]
pub unsafe extern "C" fn aurix_client_failover_endpoint_count(client: *const AurixClient) -> usize {
    match self::client(client) {
        Ok(c) => c.failover_endpoints().len(),
        Err(_) => 0,
    }
}

/// The `index`-th failover URL (see `aurix_client_failover_endpoint_count`); same buffer
/// contract as `aurix_client_endpoint`. 0 when `index` is out of range.
#[no_mangle]
pub unsafe extern "C" fn aurix_client_failover_endpoint(
    client: *const AurixClient,
    index: usize,
    buf: *mut c_char,
    capacity: usize,
) -> usize {
    match self::client(client) {
        Ok(c) => match c.failover_endpoints().get(index) {
            Some(url) => copy_out(url, buf, capacity),
            None => 0,
        },
        Err(_) => 0,
    }
}

/// Copies `s` (NUL-terminated, truncated to fit) into `buf`; returns the full length.
unsafe fn copy_out(s: &str, buf: *mut c_char, capacity: usize) -> usize {
    if !buf.is_null() && capacity > 0 {
        let n = s.len().min(capacity - 1);
        ptr::copy_nonoverlapping(s.as_ptr(), buf as *mut u8, n);
        *buf.add(n) = 0;
    }
    s.len()
}

/// Link the media currently uses; `AurixMediaNone` before the first bind.
#[no_mangle]
pub unsafe extern "C" fn aurix_client_media_path(client: *const AurixClient) -> AurixMediaPath {
    match self::client(client) {
        Ok(c) => c.media_path().into(),
        Err(_) => AurixMediaPath::AurixMediaNone,
    }
}

/// Tell the client the device's network changed (Wi-Fi ↔ cellular, VPN, new interface). On
/// QUIC the connection migrates to a fresh local socket in place (same session, sequence
/// counter and E2EE state; `AurixEventMediaPathChanged` follows); on UDP the session
/// re-announces itself to the node. `false` when not connected.
#[no_mangle]
pub unsafe extern "C" fn aurix_client_network_changed(client: *const AurixClient) -> bool {
    match self::client(client) {
        Ok(c) => c.network_changed().is_ok(),
        Err(_) => false,
    }
}

// -------------------------------------------------------------------------------- events

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AurixEventType {
    /// `state`.
    AurixEventStateChanged = 0,
    /// `session`.
    AurixEventSessionReady = 1,
    AurixEventMediaBound = 2,
    /// `request_id`, `channel_id`, `participants`, `flag` = transcription enabled, `flag2` =
    /// content-safety monitoring (disclose it to the player).
    AurixEventChannelJoined = 3,
    /// `channel_id`.
    AurixEventChannelLeft = 4,
    /// `channel_id`, `participants[0]`.
    AurixEventParticipantJoined = 5,
    /// `channel_id`, `user_id`.
    AurixEventParticipantLeft = 6,
    /// `channel_id`, `user_id`, `flag` = muted, `flag2` = server muted.
    AurixEventParticipantMuteChanged = 7,
    /// `channel_id`, `user_id`, `flag` = speaking.
    AurixEventParticipantSpeaking = 8,
    /// `channel_id`, `participants[i].user_id/energy`.
    AurixEventChannelEnergy = 9,
    /// `flag` = speaking.
    AurixEventLocalSpeaking = 10,
    /// `transmission`, `channel_id` for single.
    AurixEventTransmissionChanged = 11,
    /// `channel_id` (zero UUID = no focus).
    AurixEventChannelFocusChanged = 12,
    /// `user_id`, `flag` = blocked.
    AurixEventUserBlockChanged = 13,
    /// `channel_id`, `recording_id`, `flag` = active, `flag2` = live stream (not a stored
    /// file), `user_id` = initiator (zero UUID = operator).
    AurixEventRecording = 14,
    /// `number` = bitrate bps, `message` = reason.
    AurixEventBitrateChanged = 15,
    /// `channel_id`, `message` = reason.
    AurixEventKicked = 16,
    /// `request_id`, `channel_id`, `user_id`, `moderation_action`.
    AurixEventModerationApplied = 17,
    /// `chat`.
    AurixEventChatMessage = 18,
    /// `channel_id`, `user_id`, `flag` = typing.
    AurixEventParticipantTyping = 19,
    /// `transcript`.
    AurixEventTranscript = 20,
    /// `tts`.
    AurixEventTtsStatus = 21,
    /// `channel_id`, `positions` via `json`.
    AurixEventPositions = 22,
    /// `channel_id`, `code`, `message`.
    AurixEventRejoinFailed = 23,
    /// `request_id`, `code`, `message`.
    AurixEventRequestFailed = 24,
    /// `code`, `message`.
    AurixEventServerError = 25,
    /// `number` = attempt, `number2` = delay ms, `message` = cause.
    AurixEventRecovering = 26,
    /// `flag` = resumed (else channels were re-joined with fresh state), `flag2` = migrated
    /// (resumed on another node; `aurix_client_endpoint` names it).
    AurixEventRecovered = 27,
    /// `message` = reason.
    AurixEventFailedToRecover = 28,
    /// `message` = reason.
    AurixEventDisconnected = 29,
    /// `network_quality`.
    AurixEventNetworkQuality = 30,
    /// `audio_policy`: merged policy of the joined channels changed.
    AurixEventAudioPolicyChanged = 31,
    /// `audio_codec`: the server switched this session's codec (`aurix_event_audio_codec`).
    AurixEventAudioCodecChanged = 32,
    /// `media_path` (`aurix_event_media_path`), `message` = why: emitted after every
    /// `MediaBound` and on each mid-session UDP ↔ tunnel switch.
    AurixEventMediaPathChanged = 33,
    /// `downlink_mode` (`aurix_event_downlink_mode`): the server acknowledged a downlink
    /// mode; a fresh session reports `Streams` and the requested mode is re-applied.
    AurixEventDownlinkModeChanged = 34,
    /// `message` = the WebSocket URL the client now talks to: a failover endpoint answered
    /// while the previous node did not. Precedes that connection's `SessionReady`.
    AurixEventEndpointChanged = 35,
    /// `request_id`, `channel_id` / `user_id` = conversation, `aurix_event_chat_history` +
    /// `aurix_event_chat_history_message`: one page of stored history (newest first).
    AurixEventChatHistory = 36,
    /// `aurix_event_read_marker`: a read marker moved (this user's on any device, or another
    /// participant's when the server has read receipts on).
    AurixEventChatReadMarker = 37,
    /// `channel_id` / `user_id` = conversation, `number` = unread count, `number2` = marker
    /// count, `aurix_event_read_marker_at`: answer to `aurix_client_chat_read_markers`.
    AurixEventChatReadMarkers = 38,
    /// `number` = directed messages replayed (each arrived as `ChatMessage` with `offline`)
    /// since connecting, `flag` = older unread ones were left out (page with history).
    AurixEventChatInboxSynced = 39,
    /// `aurix_event_translation`: the server applied `aurix_client_set_translation`
    /// (tags normalised). A fresh session starts untranslated; the request is re-applied.
    AurixEventTranslationChanged = 40,
    /// `user_id`, `message` = fingerprint of the peer's E2EE identity key, `code` = the
    /// fingerprint we trusted before (empty for a first sighting; non-empty means the user
    /// now presents a different key — verify out of band).
    AurixEventE2eePeerKey = 41,
    /// `user_id`, `flag` = we hold the peer's sender key (their encrypted frames decode).
    AurixEventE2eePeerDecryptable = 42,
    /// `number` = generation of our new sender key (a member joined or left).
    AurixEventE2eeKeyRotated = 43,
    /// `channel_id`, `user_id`, `flag` = priority speaker: their speech now ducks (or no
    /// longer ducks) the channel.
    AurixEventParticipantPriorityChanged = 44,
    /// `channel_id`, `flag` = another member's priority speech started (`true`) / stopped
    /// ducking the channel; `aurix_event_ducking` = the depth and timing to apply to the
    /// game's own music/SFX bus (the voice mix is ducked server-side already).
    AurixEventDuckingChanged = 45,
    /// `aurix_event_loss_profile` = the uplink redundancy tier now in force (`number` = its
    /// value, `number2` = the server-measured uplink loss in whole percent that triggered
    /// it); FEC tuning / DRED already applied to the encoder.
    AurixEventLossProfileChanged = 46,
    /// `chat` (`aurix_event_chat`): a message of one of this client's conversations was
    /// edited (`edited_at_ms`) or deleted (`deleted_at_ms`, empty text) — replace the copy
    /// with the same `message_id`; `request_id` is set on the echo of this client's own
    /// `aurix_client_edit_chat` / `aurix_client_delete_chat`.
    AurixEventChatMessageUpdated = 47,
    /// `aurix_event_reaction`: a user added (`flag`) or removed a reaction on a message;
    /// `object_id` = message, `user_id` = who, `channel_id` = conversation (zero for direct).
    AurixEventChatReactionChanged = 48,
    /// `request_id`, `channel_id` / `user_id` = searched conversation (both zero = every
    /// direct conversation), `message` = the query, `aurix_event_chat_history` (count,
    /// `next_before`) + `aurix_event_chat_history_message`: answer to
    /// `aurix_client_search_chat`, newest match first.
    AurixEventChatSearchResult = 49,
    /// `channel_id`, `user_id`, `aurix_event_role` = the member's effective role now,
    /// `flag` = it gained a speaker slot (`false`: lost one — `AurixRoleListener` until the
    /// server promotes it again). Speaker-slot admission (`audience.max_speakers`); the
    /// member's grant is unchanged. For our own user `aurix_client_channel_info` follows.
    AurixEventParticipantRoleChanged = 50,
}

/// Channel member snapshot. Also used for energy levels (only `user_id` and `energy` set).
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct AurixParticipant {
    pub user_id: AurixUuid,
    pub ssrc: u32,
    pub role: AurixRole,
    pub muted: bool,
    pub server_muted: bool,
    pub speaking: bool,
    /// `0..=1`.
    pub energy: f32,
    /// Priority speaker (`ChannelConfig.ducking`).
    pub priority: bool,
    /// UTF-8, NUL-terminated, truncated to fit.
    pub display_name: [c_char; AURIX_NAME_LEN],
}

fn name_buf(s: &str) -> [c_char; AURIX_NAME_LEN] {
    let mut out = [0 as c_char; AURIX_NAME_LEN];
    let mut end = s.len().min(AURIX_NAME_LEN - 1);
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    for (dst, src) in out.iter_mut().zip(s.as_bytes()[..end].iter()) {
        *dst = *src as c_char;
    }
    out
}

fn participant_to_c(p: &crate::events::Participant) -> AurixParticipant {
    AurixParticipant {
        user_id: p.user_id.0.into(),
        ssrc: p.ssrc,
        role: p.role.into(),
        muted: p.muted,
        server_muted: p.server_muted,
        speaking: p.speaking,
        energy: p.energy,
        priority: p.priority,
        display_name: name_buf(&p.display_name),
    }
}

/// Chat message (strings owned by the event).
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct AurixChatMessage {
    pub message_id: AurixUuid,
    /// Zero UUID for directed messages.
    pub channel_id: AurixUuid,
    pub sender_id: AurixUuid,
    /// Zero UUID for channel messages.
    pub recipient_id: AurixUuid,
    pub sender_name: *const c_char,
    pub text: *const c_char,
    /// JSON or `NULL`.
    pub metadata_json: *const c_char,
    /// Unix milliseconds.
    pub sent_at_ms: i64,
    /// Non-zero when this is the echo of a message this client sent.
    pub request_id: u64,
    /// Directed message that waited for the recipient (replayed on connect, or — on the
    /// sender's echo — queued because the recipient is offline).
    pub offline: bool,
    /// History cursor of this message (`before` / `after` of `aurix_client_chat_history`).
    pub cursor: *const c_char,
    /// Unix milliseconds of the last edit, 0 when never edited.
    pub edited_at_ms: i64,
    /// Unix milliseconds of the deletion, 0 for a live message. A deleted message (tombstone)
    /// keeps its id and position; `text` is empty, `metadata_json` and `reactions_json` are
    /// `NULL`.
    pub deleted_at_ms: i64,
    /// Who deleted it (author, moderator or the zero UUID for the operator); zero when live.
    pub deleted_by: AurixUuid,
    /// Reaction tallies as a JSON array `[{"reaction","count","user_ids"}]`, or `NULL` when
    /// there are none (live `ChatMessage` events never carry them; history / search do).
    pub reactions_json: *const c_char,
}

/// One page of chat history (`AurixEventChatHistory`). Strings owned by the event.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct AurixChatHistory {
    /// Messages in the page, newest first (`aurix_event_chat_history_message`).
    pub count: usize,
    /// Cursor for the next older page, or `NULL` when the beginning was reached.
    pub next_before: *const c_char,
    /// Cursor for the next newer page, or `NULL` when the page is the most recent.
    pub next_after: *const c_char,
}

/// A reaction change (`AurixEventChatReactionChanged`). Strings owned by the event.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct AurixChatReaction {
    pub message_id: AurixUuid,
    /// Zero UUID for a direct message.
    pub channel_id: AurixUuid,
    /// Author of the message; `message_recipient_id` is its recipient (zero for channels).
    pub message_sender_id: AurixUuid,
    pub message_recipient_id: AurixUuid,
    /// Who added / removed the reaction.
    pub user_id: AurixUuid,
    pub reaction: *const c_char,
    pub added: bool,
    /// Users carrying `reaction` after the change.
    pub count: u32,
    /// Unix milliseconds.
    pub timestamp_ms: i64,
}

/// A user's reading position in a channel or a direct conversation.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct AurixReadMarker {
    pub user_id: AurixUuid,
    /// Zero UUID for direct conversations.
    pub channel_id: AurixUuid,
    /// The other party of a direct conversation; zero UUID for channels.
    pub peer_user_id: AurixUuid,
    /// Last read message and its send time (Unix ms).
    pub message_id: AurixUuid,
    pub message_sent_at_ms: i64,
    pub read_at_ms: i64,
}

fn marker_to_c(m: &aurix_common::protocol::ChatReadMarker) -> AurixReadMarker {
    AurixReadMarker {
        user_id: m.user_id.0.into(),
        channel_id: m.channel_id.map(|c| c.0.into()).unwrap_or_else(zero),
        peer_user_id: m.peer_user_id.map(|u| u.0.into()).unwrap_or_else(zero),
        message_id: m.message_id.into(),
        message_sent_at_ms: m.message_sent_at.timestamp_millis(),
        read_at_ms: m.read_at.timestamp_millis(),
    }
}

/// Strings of one chat message in `OwnedStrings::extra`: name, text, metadata, cursor,
/// reactions.
const CHAT_STRINGS: usize = 5;

fn push_chat_strings(extra: &mut Vec<CString>, m: &ChatMessage) {
    extra.push(cstring(&m.display_name));
    extra.push(cstring(&m.text));
    extra.push(cstring(
        &m.metadata
            .as_ref()
            .map(|v| v.to_string())
            .unwrap_or_default(),
    ));
    extra.push(cstring(&m.cursor()));
    extra.push(cstring(&if m.reactions.is_empty() {
        String::new()
    } else {
        serde_json::to_string(&m.reactions).unwrap_or_default()
    }));
}

fn chat_to_c(m: &ChatMessage, strings: &[CString], request_id: u64) -> AurixChatMessage {
    AurixChatMessage {
        message_id: m.id.into(),
        channel_id: m.channel_id.map(|c| c.0.into()).unwrap_or_else(zero),
        sender_id: m.from_user_id.0.into(),
        recipient_id: m.to_user_id.map(|u| u.0.into()).unwrap_or_else(zero),
        sender_name: strings[0].as_ptr(),
        text: strings[1].as_ptr(),
        metadata_json: if m.metadata.is_some() {
            strings[2].as_ptr()
        } else {
            ptr::null()
        },
        sent_at_ms: m.sent_at.timestamp_millis(),
        request_id,
        offline: m.offline,
        cursor: strings[3].as_ptr(),
        edited_at_ms: m.edited_at.map(|t| t.timestamp_millis()).unwrap_or(0),
        deleted_at_ms: m.deleted_at.map(|t| t.timestamp_millis()).unwrap_or(0),
        deleted_by: m.deleted_by.map(|u| u.0.into()).unwrap_or_else(zero),
        reactions_json: if m.reactions.is_empty() {
            ptr::null()
        } else {
            strings[4].as_ptr()
        },
    }
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct AurixTranscript {
    pub channel_id: AurixUuid,
    pub user_id: AurixUuid,
    pub text: *const c_char,
    /// BCP-47 or `NULL`.
    pub language: *const c_char,
    /// Server clock, Unix milliseconds.
    pub started_at_ms: i64,
    pub duration_ms: u32,
    /// Word timings are available through `aurix_event_json`.
    pub word_count: u32,
    /// Speaker's words before translation, or `NULL` for an untranslated transcript (then
    /// `text` / `language` are the speaker's own).
    pub original_text: *const c_char,
    /// Language of `original_text` (BCP-47) or `NULL`.
    pub original_language: *const c_char,
}

/// Translation preferences as applied by the server (`AurixEventTranslationChanged`).
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct AurixTranslation {
    /// Target language of translated transcripts, `NULL` = original language only.
    pub language: *const c_char,
    /// Language this participant declared to speak, or `NULL`.
    pub spoken_language: *const c_char,
    /// Translations are also spoken into this session's downlink.
    pub speech: bool,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct AurixTtsStatus {
    /// Id returned by `aurix_client_speak`, 0 if the request was not made by this client.
    pub request_id: u64,
    pub server_request_id: AurixUuid,
    pub state: AurixTtsState,
    pub duration_ms: u32,
    /// Failure detail or `NULL`.
    pub message: *const c_char,
}

struct OwnedStrings {
    code: CString,
    message: CString,
    extra: Vec<CString>,
    json: std::cell::OnceCell<CString>,
}

/// Opaque event; read with the `aurix_event_*` accessors and release with `aurix_event_free`.
pub struct AurixEvent {
    event: Event,
    strings: OwnedStrings,
    participants: Vec<AurixParticipant>,
}

fn cstring(s: &str) -> CString {
    CString::new(s.replace('\0', " ")).unwrap_or_default()
}

impl AurixEvent {
    fn new(event: Event) -> Self {
        let mut code = String::new();
        let mut message = String::new();
        let mut extra: Vec<CString> = Vec::new();
        let mut participants = Vec::new();
        match &event {
            Event::ChannelJoined {
                participants: ps, ..
            } => {
                participants = ps.iter().map(participant_to_c).collect();
            }
            Event::ParticipantJoined { participant, .. } => {
                participants.push(participant_to_c(participant));
            }
            Event::ChannelEnergy { levels, .. } => {
                participants = levels
                    .iter()
                    .map(|l| AurixParticipant {
                        user_id: l.user_id.0.into(),
                        ssrc: 0,
                        role: AurixRole::AurixRoleSpeaker,
                        muted: false,
                        server_muted: false,
                        speaking: l.energy > 0.0,
                        energy: l.energy,
                        priority: false,
                        display_name: [0; AURIX_NAME_LEN],
                    })
                    .collect();
            }
            Event::BitrateChanged { reason, .. } => message = reason.clone(),
            Event::MediaPathChanged { reason, .. } => message = reason.clone(),
            Event::EndpointChanged { url } => message = url.clone(),
            Event::Kicked { reason, .. }
            | Event::FailedToRecover { reason }
            | Event::Disconnected { reason } => message = reason.clone(),
            Event::Recovering { cause, .. } => message = cause.clone(),
            Event::RejoinFailed {
                code: c,
                message: m,
                ..
            }
            | Event::RequestFailed {
                code: c,
                message: m,
                ..
            }
            | Event::ServerError {
                code: c,
                message: m,
            } => {
                code = c.clone();
                message = m.clone();
            }
            Event::ChatMessage { message: m, .. }
            | Event::ChatMessageUpdated { message: m, .. } => push_chat_strings(&mut extra, m),
            Event::ChatHistory {
                messages,
                next_before,
                next_after,
                ..
            } => {
                extra.push(cstring(next_before.as_deref().unwrap_or_default()));
                extra.push(cstring(next_after.as_deref().unwrap_or_default()));
                for m in messages {
                    push_chat_strings(&mut extra, m);
                }
            }
            Event::ChatSearchResult {
                query,
                messages,
                next_before,
                ..
            } => {
                message = query.clone();
                extra.push(cstring(next_before.as_deref().unwrap_or_default()));
                extra.push(cstring(""));
                for m in messages {
                    push_chat_strings(&mut extra, m);
                }
            }
            Event::ChatReactionChanged { reaction, .. } => extra.push(cstring(reaction)),
            Event::Transcript(t) => {
                extra.push(cstring(&t.text));
                extra.push(cstring(t.language.as_deref().unwrap_or_default()));
                if let Some(o) = &t.original {
                    extra.push(cstring(&o.text));
                    extra.push(cstring(o.language.as_deref().unwrap_or_default()));
                }
            }
            Event::TranslationChanged {
                language,
                spoken_language,
                ..
            } => {
                extra.push(cstring(language.as_deref().unwrap_or_default()));
                extra.push(cstring(spoken_language.as_deref().unwrap_or_default()));
            }
            Event::TtsStatus { message: m, .. } => {
                extra.push(cstring(m.as_deref().unwrap_or_default()));
            }
            Event::E2eePeerKey {
                fingerprint,
                previous_fingerprint,
                ..
            } => {
                message = fingerprint.clone();
                code = previous_fingerprint.clone().unwrap_or_default();
            }
            _ => {}
        }
        Self {
            event,
            strings: OwnedStrings {
                code: cstring(&code),
                message: cstring(&message),
                extra,
                json: std::cell::OnceCell::new(),
            },
            participants,
        }
    }

    fn kind(&self) -> AurixEventType {
        use AurixEventType as T;
        match &self.event {
            Event::StateChanged(_) => T::AurixEventStateChanged,
            Event::SessionReady(_) => T::AurixEventSessionReady,
            Event::MediaBound => T::AurixEventMediaBound,
            Event::MediaPathChanged { .. } => T::AurixEventMediaPathChanged,
            Event::ChannelJoined { .. } => T::AurixEventChannelJoined,
            Event::ChannelLeft { .. } => T::AurixEventChannelLeft,
            Event::ParticipantJoined { .. } => T::AurixEventParticipantJoined,
            Event::ParticipantLeft { .. } => T::AurixEventParticipantLeft,
            Event::ParticipantMuteChanged { .. } => T::AurixEventParticipantMuteChanged,
            Event::ParticipantPriorityChanged { .. } => T::AurixEventParticipantPriorityChanged,
            Event::ParticipantRoleChanged { .. } => T::AurixEventParticipantRoleChanged,
            Event::DuckingChanged { .. } => T::AurixEventDuckingChanged,
            Event::ParticipantSpeaking { .. } => T::AurixEventParticipantSpeaking,
            Event::ChannelEnergy { .. } => T::AurixEventChannelEnergy,
            Event::LocalSpeaking(_) => T::AurixEventLocalSpeaking,
            Event::TransmissionChanged(_) => T::AurixEventTransmissionChanged,
            Event::ChannelFocusChanged(_) => T::AurixEventChannelFocusChanged,
            Event::AudioCodecChanged(_) => T::AurixEventAudioCodecChanged,
            Event::DownlinkModeChanged(_) => T::AurixEventDownlinkModeChanged,
            Event::TranslationChanged { .. } => T::AurixEventTranslationChanged,
            Event::EndpointChanged { .. } => T::AurixEventEndpointChanged,
            Event::UserBlockChanged { .. } => T::AurixEventUserBlockChanged,
            Event::Recording { .. } => T::AurixEventRecording,
            Event::BitrateChanged { .. } => T::AurixEventBitrateChanged,
            Event::LossProfileChanged { .. } => T::AurixEventLossProfileChanged,
            Event::Kicked { .. } => T::AurixEventKicked,
            Event::ModerationApplied { .. } => T::AurixEventModerationApplied,
            Event::ChatMessage { .. } => T::AurixEventChatMessage,
            Event::ChatMessageUpdated { .. } => T::AurixEventChatMessageUpdated,
            Event::ChatReactionChanged { .. } => T::AurixEventChatReactionChanged,
            Event::ChatHistory { .. } => T::AurixEventChatHistory,
            Event::ChatSearchResult { .. } => T::AurixEventChatSearchResult,
            Event::ChatReadMarker(_) => T::AurixEventChatReadMarker,
            Event::ChatReadMarkers { .. } => T::AurixEventChatReadMarkers,
            Event::ChatInboxSynced { .. } => T::AurixEventChatInboxSynced,
            Event::ParticipantTyping { .. } => T::AurixEventParticipantTyping,
            Event::Transcript(_) => T::AurixEventTranscript,
            Event::TtsStatus { .. } => T::AurixEventTtsStatus,
            Event::Positions { .. } => T::AurixEventPositions,
            Event::RejoinFailed { .. } => T::AurixEventRejoinFailed,
            Event::RequestFailed { .. } => T::AurixEventRequestFailed,
            Event::ServerError { .. } => T::AurixEventServerError,
            Event::Recovering { .. } => T::AurixEventRecovering,
            Event::Recovered { .. } => T::AurixEventRecovered,
            Event::FailedToRecover { .. } => T::AurixEventFailedToRecover,
            Event::Disconnected { .. } => T::AurixEventDisconnected,
            Event::NetworkQuality(_) => T::AurixEventNetworkQuality,
            Event::AudioPolicyChanged(_) => T::AurixEventAudioPolicyChanged,
            Event::E2eePeerKey { .. } => T::AurixEventE2eePeerKey,
            Event::E2eePeerDecryptable { .. } => T::AurixEventE2eePeerDecryptable,
            Event::E2eeKeyRotated { .. } => T::AurixEventE2eeKeyRotated,
        }
    }
}

unsafe fn event<'a>(e: *const AurixEvent) -> Option<&'a AurixEvent> {
    if e.is_null() {
        None
    } else {
        Some(&*e)
    }
}

/// Next queued event or `NULL`. Caller owns the result (`aurix_event_free`).
#[no_mangle]
pub unsafe extern "C" fn aurix_client_poll_event(client: *mut AurixClient) -> *mut AurixEvent {
    match self::client(client) {
        Ok(c) => match c.poll_event() {
            Some(e) => Box::into_raw(Box::new(AurixEvent::new(e))),
            None => ptr::null_mut(),
        },
        Err(_) => ptr::null_mut(),
    }
}

/// Block up to `timeout_ms` for the next event; `NULL` on timeout.
#[no_mangle]
pub unsafe extern "C" fn aurix_client_wait_event(
    client: *mut AurixClient,
    timeout_ms: u32,
) -> *mut AurixEvent {
    match self::client(client) {
        Ok(c) => match c.wait_event(Duration::from_millis(timeout_ms as u64)) {
            Some(e) => Box::into_raw(Box::new(AurixEvent::new(e))),
            None => ptr::null_mut(),
        },
        Err(_) => ptr::null_mut(),
    }
}

#[no_mangle]
pub unsafe extern "C" fn aurix_client_pending_events(client: *const AurixClient) -> usize {
    self::client(client)
        .map(|c| c.pending_events())
        .unwrap_or(0)
}

/// Called on an internal thread whenever an event is queued (e.g. to wake the game thread).
/// Pass `NULL` to clear. Must not block or call `aurix_client_destroy`.
#[no_mangle]
pub unsafe extern "C" fn aurix_client_set_wake_callback(
    client: *mut AurixClient,
    callback: Option<unsafe extern "C" fn(user_data: *mut c_void)>,
    user_data: *mut c_void,
) {
    let Ok(c) = self::client(client) else {
        return;
    };
    match callback {
        Some(cb) => {
            let user = user_data as usize;
            c.set_wake_hook(Some(Arc::new(move || cb(user as *mut c_void))));
        }
        None => c.set_wake_hook(None),
    }
}

#[no_mangle]
pub unsafe extern "C" fn aurix_event_free(event: *mut AurixEvent) {
    if !event.is_null() {
        drop(Box::from_raw(event));
    }
}

#[no_mangle]
pub unsafe extern "C" fn aurix_event_type(event: *const AurixEvent) -> AurixEventType {
    self::event(event)
        .map(AurixEvent::kind)
        .unwrap_or(AurixEventType::AurixEventServerError)
}

/// Whole event as JSON (`{"type": …, "data": …}`); handy for logging and for `Positions`.
/// Owned by the event.
#[no_mangle]
pub unsafe extern "C" fn aurix_event_json(event: *const AurixEvent) -> *const c_char {
    match self::event(event) {
        Some(e) => e
            .strings
            .json
            .get_or_init(|| cstring(&serde_json::to_string(&e.event).unwrap_or_default()))
            .as_ptr(),
        None => ptr::null(),
    }
}

#[no_mangle]
pub unsafe extern "C" fn aurix_event_state(event: *const AurixEvent) -> AurixConnectionState {
    match self::event(event).map(|e| &e.event) {
        Some(Event::StateChanged(s)) => (*s).into(),
        _ => AurixConnectionState::AurixStateDisconnected,
    }
}

/// Request id the event answers (`aurix_client_join_channel`, `moderate`, `send_chat`,
/// `speak`); 0 when unrelated to a request.
#[no_mangle]
pub unsafe extern "C" fn aurix_event_request_id(event: *const AurixEvent) -> u64 {
    match self::event(event).map(|e| &e.event) {
        Some(Event::ChannelJoined { request_id, .. })
        | Some(Event::ModerationApplied { request_id, .. })
        | Some(Event::RequestFailed { request_id, .. }) => *request_id,
        Some(Event::ChatMessage { request_id, .. })
        | Some(Event::ChatMessageUpdated { request_id, .. })
        | Some(Event::ChatHistory { request_id, .. })
        | Some(Event::ChatSearchResult { request_id, .. })
        | Some(Event::TtsStatus { request_id, .. }) => request_id.unwrap_or(0),
        _ => 0,
    }
}

fn zero() -> AurixUuid {
    AurixUuid { bytes: [0; 16] }
}

/// Channel the event concerns (zero UUID if none).
#[no_mangle]
pub unsafe extern "C" fn aurix_event_channel_id(event: *const AurixEvent) -> AurixUuid {
    let Some(e) = self::event(event) else {
        return zero();
    };
    let id: Option<ChannelId> = match &e.event {
        Event::ChannelJoined { channel_id, .. }
        | Event::ChannelLeft { channel_id }
        | Event::ParticipantJoined { channel_id, .. }
        | Event::ParticipantLeft { channel_id, .. }
        | Event::ParticipantMuteChanged { channel_id, .. }
        | Event::ParticipantPriorityChanged { channel_id, .. }
        | Event::ParticipantRoleChanged { channel_id, .. }
        | Event::DuckingChanged { channel_id, .. }
        | Event::ParticipantSpeaking { channel_id, .. }
        | Event::ChannelEnergy { channel_id, .. }
        | Event::Recording { channel_id, .. }
        | Event::Kicked { channel_id, .. }
        | Event::ModerationApplied { channel_id, .. }
        | Event::ParticipantTyping { channel_id, .. }
        | Event::Positions { channel_id, .. }
        | Event::RejoinFailed { channel_id, .. } => Some(*channel_id),
        Event::ChannelFocusChanged(c) => *c,
        Event::TransmissionChanged(TransmissionMode::Single { channel_id }) => Some(*channel_id),
        Event::ChatMessage { message, .. } | Event::ChatMessageUpdated { message, .. } => {
            message.channel_id
        }
        Event::ChatReactionChanged { channel_id, .. } => *channel_id,
        Event::ChatHistory { scope, .. } | Event::ChatReadMarkers { scope, .. } => scope.split().0,
        Event::ChatSearchResult { scope, .. } => scope.and_then(|s| s.split().0),
        Event::ChatReadMarker(m) => m.channel_id,
        Event::Transcript(t) => Some(t.channel_id),
        _ => None,
    };
    id.map(|c| c.0.into()).unwrap_or_else(zero)
}

/// User the event concerns (zero UUID if none).
#[no_mangle]
pub unsafe extern "C" fn aurix_event_user_id(event: *const AurixEvent) -> AurixUuid {
    let Some(e) = self::event(event) else {
        return zero();
    };
    let id: Option<UserId> = match &e.event {
        Event::ParticipantJoined { participant, .. } => Some(participant.user_id),
        Event::ParticipantLeft { user_id, .. }
        | Event::ParticipantMuteChanged { user_id, .. }
        | Event::ParticipantPriorityChanged { user_id, .. }
        | Event::ParticipantRoleChanged { user_id, .. }
        | Event::ParticipantSpeaking { user_id, .. }
        | Event::UserBlockChanged { user_id, .. }
        | Event::ModerationApplied { user_id, .. }
        | Event::ParticipantTyping { user_id, .. }
        | Event::E2eePeerKey { user_id, .. }
        | Event::E2eePeerDecryptable { user_id, .. } => Some(*user_id),
        Event::Recording { initiated_by, .. } => Some(*initiated_by),
        Event::ChatMessage { message, .. } | Event::ChatMessageUpdated { message, .. } => {
            Some(message.from_user_id)
        }
        Event::ChatReactionChanged { user_id, .. } => Some(*user_id),
        Event::ChatHistory { scope, .. } | Event::ChatReadMarkers { scope, .. } => scope.split().1,
        Event::ChatSearchResult { scope, .. } => scope.and_then(|s| s.split().1),
        Event::ChatReadMarker(m) => Some(m.user_id),
        Event::Transcript(t) => Some(t.user_id),
        _ => None,
    };
    id.map(|u| u.0.into()).unwrap_or_else(zero)
}

/// Recording id for `Recording`, server request id for `TtsStatus`, message id for chat.
#[no_mangle]
pub unsafe extern "C" fn aurix_event_object_id(event: *const AurixEvent) -> AurixUuid {
    let Some(e) = self::event(event) else {
        return zero();
    };
    match &e.event {
        Event::Recording { recording_id, .. } => (*recording_id).into(),
        Event::TtsStatus {
            server_request_id, ..
        } => (*server_request_id).into(),
        Event::ChatMessage { message, .. } | Event::ChatMessageUpdated { message, .. } => {
            message.id.into()
        }
        Event::ChatReactionChanged { message_id, .. } => (*message_id).into(),
        Event::ChatReadMarker(m) => m.message_id.into(),
        _ => zero(),
    }
}

/// Primary boolean of the event (muted / speaking / typing / blocked / active / resumed /
/// transcription — see `AurixEventType` docs).
#[no_mangle]
pub unsafe extern "C" fn aurix_event_flag(event: *const AurixEvent) -> bool {
    match self::event(event).map(|e| &e.event) {
        Some(Event::ChannelJoined { transcription, .. }) => *transcription,
        Some(Event::ParticipantMuteChanged { muted, .. }) => *muted,
        Some(Event::ParticipantPriorityChanged { priority, .. }) => *priority,
        Some(Event::ParticipantRoleChanged { admitted, .. }) => *admitted,
        Some(Event::DuckingChanged { active, .. }) => *active,
        Some(Event::ParticipantSpeaking { speaking, .. }) => *speaking,
        Some(Event::LocalSpeaking(s)) => *s,
        Some(Event::UserBlockChanged { blocked, .. }) => *blocked,
        Some(Event::Recording { active, .. }) => *active,
        Some(Event::ParticipantTyping { typing, .. }) => *typing,
        Some(Event::Recovered { resumed, .. }) => *resumed,
        Some(Event::ChatInboxSynced { truncated, .. }) => *truncated,
        Some(Event::ChatReactionChanged { added, .. }) => *added,
        Some(Event::E2eePeerDecryptable { decryptable, .. }) => *decryptable,
        _ => false,
    }
}

/// Secondary boolean: `server_muted` for `ParticipantMuteChanged`, `live` for `Recording`,
/// `safety_voice` (content-safety monitoring, disclose it) for `ChannelJoined`, `migrated`
/// for `Recovered`.
#[no_mangle]
pub unsafe extern "C" fn aurix_event_flag2(event: *const AurixEvent) -> bool {
    match self::event(event).map(|e| &e.event) {
        Some(Event::ChannelJoined { safety_voice, .. }) => *safety_voice,
        Some(Event::ParticipantMuteChanged { server_muted, .. }) => *server_muted,
        Some(Event::Recording { live, .. }) => *live,
        Some(Event::Recovered { migrated, .. }) => *migrated,
        _ => false,
    }
}

/// Numeric payload: bitrate (bps) for `BitrateChanged`, attempt for `Recovering`, unread
/// count for `ChatReadMarkers`, replayed messages for `ChatInboxSynced`.
#[no_mangle]
pub unsafe extern "C" fn aurix_event_number(event: *const AurixEvent) -> u64 {
    match self::event(event).map(|e| &e.event) {
        Some(Event::BitrateChanged { bitrate_bps, .. }) => *bitrate_bps as u64,
        Some(Event::LossProfileChanged { profile, .. }) => AurixLossProfile::from(*profile) as u64,
        Some(Event::Recovering { attempt, .. }) => *attempt as u64,
        Some(Event::ChatReadMarkers { unread_count, .. }) => u64::from(*unread_count),
        Some(Event::ChatInboxSynced { delivered, .. }) => u64::from(*delivered),
        Some(Event::ChatReactionChanged { count, .. }) => u64::from(*count),
        Some(Event::E2eeKeyRotated { generation }) => u64::from(*generation),
        _ => 0,
    }
}

/// Secondary number: reconnect delay in ms for `Recovering`, marker count for
/// `ChatReadMarkers`.
#[no_mangle]
pub unsafe extern "C" fn aurix_event_number2(event: *const AurixEvent) -> u64 {
    match self::event(event).map(|e| &e.event) {
        Some(Event::Recovering { delay, .. }) => delay.as_millis() as u64,
        Some(Event::ChatReadMarkers { markers, .. }) => markers.len() as u64,
        Some(Event::LossProfileChanged {
            uplink_loss_percent,
            ..
        }) => uplink_loss_percent.clamp(0.0, 100.0).round() as u64,
        _ => 0,
    }
}

/// Redundancy tier of a `LossProfileChanged` event (`Low` for other events).
#[no_mangle]
pub unsafe extern "C" fn aurix_event_loss_profile(event: *const AurixEvent) -> AurixLossProfile {
    match self::event(event).map(|e| &e.event) {
        Some(Event::LossProfileChanged { profile, .. }) => (*profile).into(),
        _ => AurixLossProfile::AurixLossProfileLow,
    }
}

/// Machine-readable code for failures (`RequestFailed`, `ServerError`, `RejoinFailed`);
/// empty otherwise. Owned by the event.
#[no_mangle]
pub unsafe extern "C" fn aurix_event_code(event: *const AurixEvent) -> *const c_char {
    self::event(event)
        .map(|e| e.strings.code.as_ptr())
        .unwrap_or(ptr::null())
}

/// Human-readable text: error message, kick/disconnect reason, bitrate reason, reconnect
/// cause. Empty when not applicable. Owned by the event.
#[no_mangle]
pub unsafe extern "C" fn aurix_event_message(event: *const AurixEvent) -> *const c_char {
    self::event(event)
        .map(|e| e.strings.message.as_ptr())
        .unwrap_or(ptr::null())
}

#[no_mangle]
pub unsafe extern "C" fn aurix_event_transmission(
    event: *const AurixEvent,
) -> AurixTransmissionMode {
    match self::event(event).map(|e| &e.event) {
        Some(Event::TransmissionChanged(TransmissionMode::None)) => {
            AurixTransmissionMode::AurixTransmitNone
        }
        Some(Event::TransmissionChanged(TransmissionMode::Single { .. })) => {
            AurixTransmissionMode::AurixTransmitSingle
        }
        _ => AurixTransmissionMode::AurixTransmitAll,
    }
}

/// Codec of an `AudioCodecChanged` event; Opus otherwise.
#[no_mangle]
pub unsafe extern "C" fn aurix_event_audio_codec(event: *const AurixEvent) -> AurixAudioCodec {
    match self::event(event).map(|e| &e.event) {
        Some(Event::AudioCodecChanged(codec)) => (*codec).into(),
        _ => AurixAudioCodec::AurixCodecOpus,
    }
}

/// Mode of a `DownlinkModeChanged` event; `Streams` otherwise.
#[no_mangle]
pub unsafe extern "C" fn aurix_event_downlink_mode(event: *const AurixEvent) -> AurixDownlinkMode {
    match self::event(event).map(|e| &e.event) {
        Some(Event::DownlinkModeChanged(mode)) => (*mode).into(),
        _ => AurixDownlinkMode::AurixDownlinkStreams,
    }
}

/// Link of a `MediaPathChanged` event; `AurixMediaNone` for other events.
#[no_mangle]
pub unsafe extern "C" fn aurix_event_media_path(event: *const AurixEvent) -> AurixMediaPath {
    match self::event(event).map(|e| &e.event) {
        Some(Event::MediaPathChanged { path, .. }) => Some(*path).into(),
        _ => AurixMediaPath::AurixMediaNone,
    }
}

#[no_mangle]
pub unsafe extern "C" fn aurix_event_moderation_action(
    event: *const AurixEvent,
) -> AurixModerationAction {
    match self::event(event).map(|e| &e.event) {
        Some(Event::ModerationApplied { action, .. }) => action_to_c(*action),
        _ => AurixModerationAction::AurixModerationKick,
    }
}

/// Session facts for `SessionReady`.
#[no_mangle]
pub unsafe extern "C" fn aurix_event_session(
    event: *const AurixEvent,
    out: *mut AurixSessionInfo,
) -> bool {
    if out.is_null() {
        return false;
    }
    match self::event(event).map(|e| &e.event) {
        Some(Event::SessionReady(s)) => {
            *out = AurixSessionInfo {
                session_id: s.session_id.0.into(),
                ssrc: s.ssrc,
                resume_grace_ms: s.resume_grace.as_millis().min(u32::MAX as u128) as u32,
                resumed: s.resumed,
                user_id: zero(),
                media_tunnel: s.media_tunnel,
                downlink_mix: s.downlink_mix,
                migrated: s.migrated,
                translation: s.translation.is_some(),
                translation_speech: s.translation.as_ref().is_some_and(|t| t.speech),
                media_quic: s.media_quic,
                media_tls: s.media_tls,
            };
            true
        }
        _ => false,
    }
}

/// Number of entries readable with `aurix_event_participant` (roster on `ChannelJoined`,
/// one on `ParticipantJoined`, levels on `ChannelEnergy`).
#[no_mangle]
pub unsafe extern "C" fn aurix_event_participant_count(event: *const AurixEvent) -> usize {
    self::event(event)
        .map(|e| e.participants.len())
        .unwrap_or(0)
}

#[no_mangle]
pub unsafe extern "C" fn aurix_event_participant(
    event: *const AurixEvent,
    index: usize,
    out: *mut AurixParticipant,
) -> bool {
    if out.is_null() {
        return false;
    }
    match self::event(event).and_then(|e| e.participants.get(index)) {
        Some(p) => {
            *out = *p;
            true
        }
        None => false,
    }
}

#[no_mangle]
pub unsafe extern "C" fn aurix_event_chat(
    event: *const AurixEvent,
    out: *mut AurixChatMessage,
) -> bool {
    if out.is_null() {
        return false;
    }
    let Some(e) = self::event(event) else {
        return false;
    };
    let (request_id, message) = match &e.event {
        Event::ChatMessage {
            request_id,
            message,
        }
        | Event::ChatMessageUpdated {
            request_id,
            message,
        } => (request_id, message),
        _ => return false,
    };
    *out = chat_to_c(message, &e.strings.extra, request_id.unwrap_or(0));
    true
}

/// Page summary of a `ChatHistory` or `ChatSearchResult` event (`next_after` is always
/// `NULL` for a search).
#[no_mangle]
pub unsafe extern "C" fn aurix_event_chat_history(
    event: *const AurixEvent,
    out: *mut AurixChatHistory,
) -> bool {
    if out.is_null() {
        return false;
    }
    let Some(e) = self::event(event) else {
        return false;
    };
    let (messages, next_before, next_after) = match &e.event {
        Event::ChatHistory {
            messages,
            next_before,
            next_after,
            ..
        } => (messages, next_before, next_after),
        Event::ChatSearchResult {
            messages,
            next_before,
            ..
        } => (messages, next_before, &None),
        _ => return false,
    };
    let s = &e.strings.extra;
    *out = AurixChatHistory {
        count: messages.len(),
        next_before: if next_before.is_some() {
            s[0].as_ptr()
        } else {
            ptr::null()
        },
        next_after: if next_after.is_some() {
            s[1].as_ptr()
        } else {
            ptr::null()
        },
    };
    true
}

/// Message `index` (0 = newest) of a `ChatHistory` / `ChatSearchResult` page; `request_id`
/// is 0 for all of them.
#[no_mangle]
pub unsafe extern "C" fn aurix_event_chat_history_message(
    event: *const AurixEvent,
    index: usize,
    out: *mut AurixChatMessage,
) -> bool {
    if out.is_null() {
        return false;
    }
    let Some(e) = self::event(event) else {
        return false;
    };
    let messages = match &e.event {
        Event::ChatHistory { messages, .. } | Event::ChatSearchResult { messages, .. } => messages,
        _ => return false,
    };
    let Some(m) = messages.get(index) else {
        return false;
    };
    let start = 2 + index * CHAT_STRINGS;
    *out = chat_to_c(m, &e.strings.extra[start..start + CHAT_STRINGS], 0);
    true
}

/// The change of a `ChatReactionChanged` event.
#[no_mangle]
pub unsafe extern "C" fn aurix_event_reaction(
    event: *const AurixEvent,
    out: *mut AurixChatReaction,
) -> bool {
    if out.is_null() {
        return false;
    }
    let Some(e) = self::event(event) else {
        return false;
    };
    let Event::ChatReactionChanged {
        message_id,
        channel_id,
        message_from_user_id,
        message_to_user_id,
        user_id,
        added,
        count,
        timestamp,
        ..
    } = &e.event
    else {
        return false;
    };
    *out = AurixChatReaction {
        message_id: (*message_id).into(),
        channel_id: channel_id.map(|c| c.0.into()).unwrap_or_else(zero),
        message_sender_id: message_from_user_id.0.into(),
        message_recipient_id: message_to_user_id.map(|u| u.0.into()).unwrap_or_else(zero),
        user_id: user_id.0.into(),
        reaction: e.strings.extra[0].as_ptr(),
        added: *added,
        count: *count,
        timestamp_ms: timestamp.timestamp_millis(),
    };
    true
}

/// The marker of a `ChatReadMarker` event.
#[no_mangle]
pub unsafe extern "C" fn aurix_event_read_marker(
    event: *const AurixEvent,
    out: *mut AurixReadMarker,
) -> bool {
    if out.is_null() {
        return false;
    }
    match self::event(event).map(|e| &e.event) {
        Some(Event::ChatReadMarker(m)) => {
            *out = marker_to_c(m);
            true
        }
        _ => false,
    }
}

/// Marker `index` of a `ChatReadMarkers` answer (`aurix_event_number2` = count). This
/// user's own marker, when stored, is among them.
#[no_mangle]
pub unsafe extern "C" fn aurix_event_read_marker_at(
    event: *const AurixEvent,
    index: usize,
    out: *mut AurixReadMarker,
) -> bool {
    if out.is_null() {
        return false;
    }
    match self::event(event).map(|e| &e.event) {
        Some(Event::ChatReadMarkers { markers, .. }) => match markers.get(index) {
            Some(m) => {
                *out = marker_to_c(m);
                true
            }
            None => false,
        },
        _ => false,
    }
}

#[no_mangle]
pub unsafe extern "C" fn aurix_event_transcript(
    event: *const AurixEvent,
    out: *mut AurixTranscript,
) -> bool {
    if out.is_null() {
        return false;
    }
    let Some(e) = self::event(event) else {
        return false;
    };
    let Event::Transcript(t) = &e.event else {
        return false;
    };
    let s = &e.strings.extra;
    *out = AurixTranscript {
        channel_id: t.channel_id.0.into(),
        user_id: t.user_id.0.into(),
        text: s[0].as_ptr(),
        language: if t.language.is_some() {
            s[1].as_ptr()
        } else {
            ptr::null()
        },
        started_at_ms: t.started_at.timestamp_millis(),
        duration_ms: t.duration_ms.min(u32::MAX as u64) as u32,
        word_count: t.words.len() as u32,
        original_text: match &t.original {
            Some(_) => s[2].as_ptr(),
            None => ptr::null(),
        },
        original_language: match &t.original {
            Some(o) if o.language.is_some() => s[3].as_ptr(),
            _ => ptr::null(),
        },
    };
    true
}

/// Preferences of a `TranslationChanged` event.
#[no_mangle]
pub unsafe extern "C" fn aurix_event_translation(
    event: *const AurixEvent,
    out: *mut AurixTranslation,
) -> bool {
    if out.is_null() {
        return false;
    }
    let Some(e) = self::event(event) else {
        return false;
    };
    let Event::TranslationChanged {
        language,
        spoken_language,
        speech,
    } = &e.event
    else {
        return false;
    };
    let s = &e.strings.extra;
    *out = AurixTranslation {
        language: if language.is_some() {
            s[0].as_ptr()
        } else {
            ptr::null()
        },
        spoken_language: if spoken_language.is_some() {
            s[1].as_ptr()
        } else {
            ptr::null()
        },
        speech: *speech,
    };
    true
}

#[no_mangle]
pub unsafe extern "C" fn aurix_event_tts(
    event: *const AurixEvent,
    out: *mut AurixTtsStatus,
) -> bool {
    if out.is_null() {
        return false;
    }
    let Some(e) = self::event(event) else {
        return false;
    };
    let Event::TtsStatus {
        request_id,
        server_request_id,
        state,
        duration_ms,
        message,
    } = &e.event
    else {
        return false;
    };
    *out = AurixTtsStatus {
        request_id: request_id.unwrap_or(0),
        server_request_id: (*server_request_id).into(),
        state: (*state).into(),
        duration_ms: duration_ms.unwrap_or(0).min(u32::MAX as u64) as u32,
        message: if message.is_some() {
            e.strings.extra[0].as_ptr()
        } else {
            ptr::null()
        },
    };
    true
}

// ------------------------------------------------------------------------------ channels

/// Join `channel_id`. `join_token` may be `NULL` (session token rights apply). The request id
/// written to `request_id_out` is echoed by `ChannelJoined` / `RequestFailed`.
#[no_mangle]
pub unsafe extern "C" fn aurix_client_join_channel(
    client: *mut AurixClient,
    channel_id: *const AurixUuid,
    join_token: *const c_char,
    request_id_out: *mut u64,
) -> AurixResult {
    let c = match self::client(client) {
        Ok(c) => c,
        Err(r) => return r,
    };
    let ch = match uuid_arg(channel_id, "channel_id") {
        Ok(u) => ChannelId(u),
        Err(r) => return r,
    };
    let token = match opt_cstr_arg(join_token, "join_token") {
        Ok(t) => t,
        Err(r) => return r,
    };
    match c.join_channel(ch, token.as_deref()) {
        Ok(id) => {
            if !request_id_out.is_null() {
                *request_id_out = id;
            }
            AurixResult::AurixOk
        }
        Err(e) => fail(e),
    }
}

#[no_mangle]
pub unsafe extern "C" fn aurix_client_leave_channel(
    client: *mut AurixClient,
    channel_id: *const AurixUuid,
) -> AurixResult {
    let c = match self::client(client) {
        Ok(c) => c,
        Err(r) => return r,
    };
    match uuid_arg(channel_id, "channel_id") {
        Ok(u) => ok(c.leave_channel(ChannelId(u))),
        Err(r) => r,
    }
}

/// Copy up to `capacity` joined channel ids into `out`; returns the total number joined (may
/// exceed `capacity`).
#[no_mangle]
pub unsafe extern "C" fn aurix_client_joined_channels(
    client: *const AurixClient,
    out: *mut AurixUuid,
    capacity: usize,
) -> usize {
    let Ok(c) = self::client(client) else {
        return 0;
    };
    let ids = c.joined_channels();
    if !out.is_null() {
        for (i, id) in ids.iter().take(capacity).enumerate() {
            *out.add(i) = id.0.into();
        }
    }
    ids.len()
}

/// Whether speech in a joined channel is transcribed server-side (`Transcript` events).
#[no_mangle]
pub unsafe extern "C" fn aurix_client_channel_transcribes(
    client: *const AurixClient,
    channel_id: *const AurixUuid,
) -> bool {
    match (self::client(client), uuid_arg(channel_id, "channel_id")) {
        (Ok(c), Ok(id)) => c.channel_transcribes(ChannelId(id)),
        _ => false,
    }
}

/// Whether speech in a joined channel is analysed by the server's content-safety classifier
/// (`ChannelJoinAck.safety_voice`); games should disclose it to the player.
#[no_mangle]
pub unsafe extern "C" fn aurix_client_channel_monitored(
    client: *const AurixClient,
    channel_id: *const AurixUuid,
) -> bool {
    match (self::client(client), uuid_arg(channel_id, "channel_id")) {
        (Ok(c), Ok(id)) => c.channel_monitored(ChannelId(id)),
        _ => false,
    }
}

/// How far presence and text reach in a positional channel (`PositionalConfig.roster_radius` /
/// `text_radius`). A radius `<= 0` means "the whole channel".
#[repr(C)]
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct AurixChannelScope {
    /// The roster only lists members within this distance of us (once both positions are
    /// known); `ParticipantJoined`/`ParticipantLeft` also fire when someone moves in or out
    /// of range (leaving uses a 10 % wider radius so the edge does not flicker).
    pub roster_radius: f32,
    /// Channel chat, typing and transcripts reach only members within this distance.
    pub text_radius: f32,
}

impl From<ChannelScope> for AurixChannelScope {
    fn from(s: ChannelScope) -> Self {
        Self {
            roster_radius: s.roster_radius.unwrap_or(0.0),
            text_radius: s.text_radius.unwrap_or(0.0),
        }
    }
}

/// Presence / text range of a joined channel; `false` (and `out` untouched) until its join
/// is acknowledged.
#[no_mangle]
pub unsafe extern "C" fn aurix_client_channel_scope(
    client: *const AurixClient,
    channel_id: *const AurixUuid,
    out: *mut AurixChannelScope,
) -> bool {
    if out.is_null() {
        return false;
    }
    match (self::client(client), uuid_arg(channel_id, "channel_id")) {
        (Ok(c), Ok(id)) => match c.channel_scope(ChannelId(id)) {
            Some(s) => {
                *out = s.into();
                true
            }
            None => false,
        },
        _ => false,
    }
}

/// Membership facts of a joined channel.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct AurixChannelInfo {
    /// This session's role; `AurixRoleListener` cannot transmit (the server drops its frames).
    pub role: AurixRole,
    /// Members across all nodes, including listeners hidden from the roster.
    pub participant_count: u32,
    /// Receive-only listeners are absent from the roster and never announced.
    pub hidden_listeners: bool,
    /// Speech is transcribed and captions delivered.
    pub transcription: bool,
    /// Speech is analysed by the content-safety classifier (disclose it).
    pub safety_voice: bool,
    /// This session is a priority speaker here.
    pub priority: bool,
    /// Priority-speaker ducking the server applies (`enabled == false`: off).
    pub ducking: AurixDucking,
    /// Our grant allows speaking but every speaker slot is taken: `role` is
    /// `AurixRoleListener` until an `AurixEventParticipantRoleChanged` promotes us.
    pub waiting_to_speak: bool,
}

/// Priority-speaker ducking (`ChannelConfig.ducking`): while a priority speaker talks, every
/// other stream is attenuated to `gain` with these timings. The same numbers are what a game
/// should apply to its own music/SFX on `AurixEventDuckingChanged`.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct AurixDucking {
    pub enabled: bool,
    /// Linear gain non-priority audio is attenuated to (`0..=1`).
    pub gain: f32,
    pub attack_ms: u32,
    pub release_ms: u32,
    pub hold_ms: u32,
    /// Moderators / administrators duck too, not only explicit priority grants.
    pub moderators: bool,
}

impl From<Option<DuckingConfig>> for AurixDucking {
    fn from(d: Option<DuckingConfig>) -> Self {
        match d {
            Some(d) => Self {
                enabled: true,
                gain: d.gain,
                attack_ms: d.attack_ms,
                release_ms: d.release_ms,
                hold_ms: d.hold_ms,
                moderators: d.moderators,
            },
            None => Self::default(),
        }
    }
}

impl Default for AurixChannelInfo {
    fn default() -> Self {
        Self {
            role: AurixRole::AurixRoleListener,
            participant_count: 0,
            hidden_listeners: false,
            transcription: false,
            safety_voice: false,
            priority: false,
            waiting_to_speak: false,
            ducking: AurixDucking::default(),
        }
    }
}

/// Role / member count / roster policy of a joined channel; `false` (and `out` untouched)
/// until its join is acknowledged.
#[no_mangle]
pub unsafe extern "C" fn aurix_client_channel_info(
    client: *const AurixClient,
    channel_id: *const AurixUuid,
    out: *mut AurixChannelInfo,
) -> bool {
    if out.is_null() {
        return false;
    }
    match (self::client(client), uuid_arg(channel_id, "channel_id")) {
        (Ok(c), Ok(id)) => {
            let id = ChannelId(id);
            match c.channel_role(id) {
                Some(role) => {
                    *out = AurixChannelInfo {
                        role: role.into(),
                        participant_count: c.participant_count(id).unwrap_or(0),
                        hidden_listeners: c.channel_hidden_listeners(id),
                        transcription: c.channel_transcribes(id),
                        safety_voice: c.channel_monitored(id),
                        priority: c.is_priority(id),
                        waiting_to_speak: c.is_waiting_to_speak(id),
                        ducking: c.channel_ducking(id).into(),
                    };
                    true
                }
                None => false,
            }
        }
        _ => false,
    }
}

/// `ChannelJoined` only: this session's role, the member count and the roster policy
/// (defaults for other events).
#[no_mangle]
pub unsafe extern "C" fn aurix_event_channel_info(event: *const AurixEvent) -> AurixChannelInfo {
    match self::event(event).map(|e| &e.event) {
        Some(Event::ChannelJoined {
            role,
            participant_count,
            hidden_listeners,
            transcription,
            safety_voice,
            ducking,
            priority,
            waiting_to_speak,
            ..
        }) => AurixChannelInfo {
            role: (*role).into(),
            participant_count: *participant_count,
            hidden_listeners: *hidden_listeners,
            transcription: *transcription,
            safety_voice: *safety_voice,
            priority: *priority,
            waiting_to_speak: *waiting_to_speak,
            ducking: (*ducking).into(),
        },
        _ => AurixChannelInfo::default(),
    }
}

/// `ParticipantRoleChanged` only: the member's effective role now (`AurixRoleListener` for
/// other events).
#[no_mangle]
pub unsafe extern "C" fn aurix_event_role(event: *const AurixEvent) -> AurixRole {
    match self::event(event).map(|e| &e.event) {
        Some(Event::ParticipantRoleChanged { role, .. }) => (*role).into(),
        _ => AurixRole::AurixRoleListener,
    }
}

/// `DuckingChanged` only: the channel's ducking depth and timings (disabled for other events).
#[no_mangle]
pub unsafe extern "C" fn aurix_event_ducking(event: *const AurixEvent) -> AurixDucking {
    match self::event(event).map(|e| &e.event) {
        Some(Event::DuckingChanged { config, .. }) => Some(*config).into(),
        _ => AurixDucking::default(),
    }
}

/// Whether another member's priority speech is ducking `channel_id` right now.
#[no_mangle]
pub unsafe extern "C" fn aurix_client_ducking_active(
    client: *const AurixClient,
    channel_id: *const AurixUuid,
) -> bool {
    match (self::client(client), uuid_arg(channel_id, "channel_id")) {
        (Ok(c), Ok(id)) => c.ducking_active(ChannelId(id)),
        _ => false,
    }
}

/// `ChannelJoined` only: the channel's presence / text range (zeros for other events).
#[no_mangle]
pub unsafe extern "C" fn aurix_event_channel_scope(event: *const AurixEvent) -> AurixChannelScope {
    match self::event(event).map(|e| &e.event) {
        Some(Event::ChannelJoined { scope, .. }) => (*scope).into(),
        _ => AurixChannelScope::default(),
    }
}

/// Copy up to `capacity` participants of `channel_id`; returns the total count.
#[no_mangle]
pub unsafe extern "C" fn aurix_client_participants(
    client: *const AurixClient,
    channel_id: *const AurixUuid,
    out: *mut AurixParticipant,
    capacity: usize,
) -> usize {
    let Ok(c) = self::client(client) else {
        return 0;
    };
    let Ok(ch) = uuid_arg(channel_id, "channel_id") else {
        return 0;
    };
    let ps = c.participants(ChannelId(ch));
    if !out.is_null() {
        for (i, p) in ps.iter().take(capacity).enumerate() {
            *out.add(i) = participant_to_c(p);
        }
    }
    ps.len()
}

/// Owner of `ssrc` (microphone or its TTS voice) across joined channels.
#[no_mangle]
pub unsafe extern "C" fn aurix_client_user_for_ssrc(
    client: *const AurixClient,
    ssrc: u32,
    out: *mut AurixUuid,
) -> bool {
    let Ok(c) = self::client(client) else {
        return false;
    };
    match c.user_for_ssrc(ssrc) {
        Some(u) if !out.is_null() => {
            *out = u.0.into();
            true
        }
        _ => false,
    }
}

// --------------------------------------------------------------------------------- audio

// ---------------------------------------------------------------------------------- e2ee

/// Fingerprint of this client's E2EE identity key (`xxxx-xxxx-…`), shown to peers as
/// `AurixEventE2eePeerKey`; same buffer contract as `aurix_client_endpoint`.
#[no_mangle]
pub unsafe extern "C" fn aurix_client_e2ee_fingerprint(
    client: *const AurixClient,
    buf: *mut c_char,
    capacity: usize,
) -> usize {
    match self::client(client) {
        Ok(c) => copy_out(&c.e2ee_fingerprint(), buf, capacity),
        Err(_) => 0,
    }
}

/// Fingerprint of `user_id`'s identity key if it announced one in a shared encrypted
/// channel; same buffer contract as `aurix_client_endpoint`, 0 when unknown.
#[no_mangle]
pub unsafe extern "C" fn aurix_client_e2ee_peer_fingerprint(
    client: *const AurixClient,
    user_id: AurixUuid,
    buf: *mut c_char,
    capacity: usize,
) -> usize {
    match self::client(client) {
        Ok(c) => match c.e2ee_peer_fingerprint(UserId(Uuid::from(user_id))) {
            Some(fp) => copy_out(&fp, buf, capacity),
            None => 0,
        },
        Err(_) => 0,
    }
}

/// Whether we hold `user_id`'s current sender key (their encrypted frames decode).
#[no_mangle]
pub unsafe extern "C" fn aurix_client_e2ee_peer_decryptable(
    client: *const AurixClient,
    user_id: AurixUuid,
) -> bool {
    match self::client(client) {
        Ok(c) => c.e2ee_peer_decryptable(UserId(Uuid::from(user_id))),
        Err(_) => false,
    }
}

/// Feed interleaved f32 capture PCM (`sample_count` total samples across channels) at any
/// sample rate. Encodes and sends 20 ms Opus frames. Audio-thread safe.
#[no_mangle]
pub unsafe extern "C" fn aurix_client_push_capture_f32(
    client: *mut AurixClient,
    pcm: *const f32,
    sample_count: usize,
    sample_rate: u32,
    channels: u8,
) {
    let Ok(c) = self::client(client) else {
        return;
    };
    if pcm.is_null() || sample_count == 0 {
        return;
    }
    c.push_capture_f32(
        std::slice::from_raw_parts(pcm, sample_count),
        sample_rate,
        channels,
    );
}

#[no_mangle]
pub unsafe extern "C" fn aurix_client_push_capture_i16(
    client: *mut AurixClient,
    pcm: *const i16,
    sample_count: usize,
    sample_rate: u32,
    channels: u8,
) {
    let Ok(c) = self::client(client) else {
        return;
    };
    if pcm.is_null() || sample_count == 0 {
        return;
    }
    c.push_capture_i16(
        std::slice::from_raw_parts(pcm, sample_count),
        sample_rate,
        channels,
    );
}

/// Send a pre-encoded 20 ms Opus frame (engine-side encoder). `level` is the RFC 6464 level
/// byte (0 = loudest … 127 = silence) or -1 if unknown.
#[no_mangle]
pub unsafe extern "C" fn aurix_client_send_opus(
    client: *mut AurixClient,
    opus: *const u8,
    len: usize,
    level: i32,
) -> AurixResult {
    let c = match self::client(client) {
        Ok(c) => c,
        Err(r) => return r,
    };
    if opus.is_null() || len == 0 {
        return null_ptr("opus");
    }
    let level = if (0..=127).contains(&level) {
        Some(level as u8)
    } else {
        None
    };
    ok(c.send_opus_frame(std::slice::from_raw_parts(opus, len), level))
}

/// Mix all remote voices **into** `out` (interleaved f32, `sample_count` total samples,
/// `channels` 1 or 2; existing content is preserved and voices are added). Returns the
/// number of active streams. Audio-thread safe.
#[no_mangle]
pub unsafe extern "C" fn aurix_client_mix_output_f32(
    client: *mut AurixClient,
    out: *mut f32,
    sample_count: usize,
    channels: u8,
) -> usize {
    let Ok(c) = self::client(client) else {
        return 0;
    };
    if out.is_null() || sample_count == 0 {
        return 0;
    }
    c.mix_output_f32(std::slice::from_raw_parts_mut(out, sample_count), channels)
}

/// Like `aurix_client_mix_output_f32` but **overwrites** `out` with i16 samples.
#[no_mangle]
pub unsafe extern "C" fn aurix_client_mix_output_i16(
    client: *mut AurixClient,
    out: *mut i16,
    sample_count: usize,
    channels: u8,
) -> usize {
    let Ok(c) = self::client(client) else {
        return 0;
    };
    if out.is_null() || sample_count == 0 {
        return 0;
    }
    c.mix_output_i16(std::slice::from_raw_parts_mut(out, sample_count), channels)
}

/// One downlink stream held by the jitter buffers (see `aurix_client_participant_streams`).
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct AurixParticipantStream {
    pub ssrc: u32,
    /// Owner across joined channels; nil UUID for a server mix or an unknown sender.
    pub user_id: AurixUuid,
    /// Server-synthesized voice (TTS / announcement) of `user_id`, not its microphone.
    pub synthesized: bool,
    /// Server-mixed channel downlink (`AurixDownlinkMode::Mixed`), not one participant.
    pub mixed: bool,
    /// Decoded two-wide (stereo uplink or server mix).
    pub stereo: bool,
    /// Has audio buffered or arriving; `false` between talk spurts.
    pub active: bool,
    pub buffered_frames: u32,
}

/// Copy up to `capacity` downlink streams with their owners; returns the total count.
/// For hosts that spawn one positioned emitter per talking participant.
#[no_mangle]
pub unsafe extern "C" fn aurix_client_participant_streams(
    client: *const AurixClient,
    out: *mut AurixParticipantStream,
    capacity: usize,
) -> usize {
    let Ok(c) = self::client(client) else {
        return 0;
    };
    let streams = c.participant_streams();
    if !out.is_null() {
        for (i, s) in streams.iter().take(capacity).enumerate() {
            *out.add(i) = AurixParticipantStream {
                ssrc: s.ssrc,
                user_id: s.user_id.map(|u| u.0).unwrap_or(uuid::Uuid::nil()).into(),
                synthesized: s.synthesized,
                mixed: s.mixed,
                stereo: s.stereo,
                active: s.active,
                buffered_frames: s.buffered_frames.min(u32::MAX as usize) as u32,
            };
        }
    }
    streams.len()
}

/// Per-participant playout for engine spatialization: **overwrite** `out` (interleaved f32,
/// `sample_count` total samples, `channels` 1 or 2) with `user_id`'s voice only — microphone
/// plus its TTS voice — with no local panning, so the host attaches it to a positioned
/// emitter (HRTF, occlusion, reverb). Per-participant volume, server gain and master volume
/// / mute still apply. Returns the frames that carried audio; the rest is silence. A stream
/// is consumed by whoever pulls it: pull every participant you want to hear, or mix the rest
/// with `aurix_client_mix_output_*`. Not fed to the AEC — push the engine's final output with
/// `aurix_client_push_render_*`. Audio-thread safe.
#[no_mangle]
pub unsafe extern "C" fn aurix_client_pull_participant_f32(
    client: *mut AurixClient,
    user_id: *const AurixUuid,
    out: *mut f32,
    sample_count: usize,
    channels: u8,
) -> usize {
    let Ok(c) = self::client(client) else {
        return 0;
    };
    if out.is_null() || sample_count == 0 {
        return 0;
    }
    let out = std::slice::from_raw_parts_mut(out, sample_count);
    let Ok(user) = uuid_arg(user_id, "user_id") else {
        out.fill(0.0);
        return 0;
    };
    c.pull_participant_f32(UserId(user), out, channels)
}

/// Like `aurix_client_pull_participant_f32` with i16 samples.
#[no_mangle]
pub unsafe extern "C" fn aurix_client_pull_participant_i16(
    client: *mut AurixClient,
    user_id: *const AurixUuid,
    out: *mut i16,
    sample_count: usize,
    channels: u8,
) -> usize {
    let Ok(c) = self::client(client) else {
        return 0;
    };
    if out.is_null() || sample_count == 0 {
        return 0;
    }
    let out = std::slice::from_raw_parts_mut(out, sample_count);
    let Ok(user) = uuid_arg(user_id, "user_id") else {
        out.fill(0);
        return 0;
    };
    c.pull_participant_i16(UserId(user), out, channels)
}

/// Keep `user_id`'s microphone and TTS streams out of `aurix_client_mix_output_*` while the
/// host renders them itself with `aurix_client_pull_participant_*`, so one spatialized
/// emitter per talker and the aggregate mix for everyone else never double-play. The claim
/// follows the user across SSRC changes; `claimed = false` returns the user to the mix.
#[no_mangle]
pub unsafe extern "C" fn aurix_client_set_participant_claimed(
    client: *mut AurixClient,
    user_id: *const AurixUuid,
    claimed: bool,
) -> AurixResult {
    let c = match self::client(client) {
        Ok(c) => c,
        Err(r) => return r,
    };
    match uuid_arg(user_id, "user_id") {
        Ok(u) => {
            c.set_participant_claimed(UserId(u), claimed);
            AurixResult::AurixOk
        }
        Err(r) => r,
    }
}

#[no_mangle]
pub unsafe extern "C" fn aurix_client_set_muted(client: *mut AurixClient, muted: bool) {
    if let Ok(c) = self::client(client) {
        c.set_muted(muted);
    }
}

#[no_mangle]
pub unsafe extern "C" fn aurix_client_is_muted(client: *const AurixClient) -> bool {
    self::client(client).map(|c| c.is_muted()).unwrap_or(false)
}

#[no_mangle]
pub unsafe extern "C" fn aurix_client_is_speaking(client: *const AurixClient) -> bool {
    self::client(client)
        .map(|c| c.is_speaking())
        .unwrap_or(false)
}

/// Microphone software gain `0..=4`.
#[no_mangle]
pub unsafe extern "C" fn aurix_client_set_input_gain(client: *mut AurixClient, gain: f32) {
    if let Ok(c) = self::client(client) {
        c.set_input_gain(gain);
    }
}

/// RMS energy `0..=1` of the last captured frame.
#[no_mangle]
pub unsafe extern "C" fn aurix_client_input_energy(client: *const AurixClient) -> f32 {
    self::client(client)
        .map(|c| c.input_energy())
        .unwrap_or(0.0)
}

/// Voice activity detector: RMS threshold `0..=1` and hangover in 20 ms frames.
#[no_mangle]
pub unsafe extern "C" fn aurix_client_set_vad(
    client: *mut AurixClient,
    threshold: f32,
    hangover_frames: u32,
) {
    if let Ok(c) = self::client(client) {
        c.set_vad(threshold, hangover_frames);
    }
}

#[no_mangle]
pub unsafe extern "C" fn aurix_client_set_vad_gate(client: *mut AurixClient, enabled: bool) {
    if let Ok(c) = self::client(client) {
        c.set_vad_gate(enabled);
    }
}

/// Replace the capture DSP configuration (applies to the next frame; values are clamped —
/// read back with `aurix_client_dsp`). Changing the echo tail or delay hint resets the
/// echo canceller's filter.
#[no_mangle]
pub unsafe extern "C" fn aurix_client_set_dsp(
    client: *mut AurixClient,
    config: *const AurixDspConfig,
) -> AurixResult {
    let c = match self::client(client) {
        Ok(c) => c,
        Err(r) => return r,
    };
    if config.is_null() {
        return null_ptr("config");
    }
    c.set_dsp((*config).into());
    AurixResult::AurixOk
}

#[no_mangle]
pub unsafe extern "C" fn aurix_client_dsp(
    client: *const AurixClient,
    out: *mut AurixDspConfig,
) -> AurixResult {
    let c = match self::client(client) {
        Ok(c) => c,
        Err(r) => return r,
    };
    if out.is_null() {
        return null_ptr("out");
    }
    *out = c.dsp().into();
    AurixResult::AurixOk
}

#[no_mangle]
pub unsafe extern "C" fn aurix_client_dsp_stats(
    client: *const AurixClient,
    out: *mut AurixDspStats,
) -> AurixResult {
    let c = match self::client(client) {
        Ok(c) => c,
        Err(r) => return r,
    };
    if out.is_null() {
        return null_ptr("out");
    }
    *out = c.dsp_stats().into();
    AurixResult::AurixOk
}

/// Mouth-shape buckets of `AurixVisemeFrame.weights` (index = value). `PP`/`FF`/`SS` follow
/// the common lip-sync naming (lips closed, labiodental, sibilant); the rest are vowels.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AurixViseme {
    AurixVisemeSilence = 0,
    AurixVisemePP = 1,
    AurixVisemeFF = 2,
    AurixVisemeSS = 3,
    AurixVisemeAA = 4,
    AurixVisemeE = 5,
    AurixVisemeIH = 6,
    AurixVisemeOH = 7,
    AurixVisemeOU = 8,
}

/// Number of `AurixViseme` buckets.
pub const AURIX_VISEME_COUNT: usize = 9;
const _: () = assert!(AURIX_VISEME_COUNT == VISEME_COUNT);

impl From<Viseme> for AurixViseme {
    fn from(v: Viseme) -> Self {
        match v {
            Viseme::Silence => AurixViseme::AurixVisemeSilence,
            Viseme::PP => AurixViseme::AurixVisemePP,
            Viseme::FF => AurixViseme::AurixVisemeFF,
            Viseme::SS => AurixViseme::AurixVisemeSS,
            Viseme::AA => AurixViseme::AurixVisemeAA,
            Viseme::E => AurixViseme::AurixVisemeE,
            Viseme::IH => AurixViseme::AurixVisemeIH,
            Viseme::OH => AurixViseme::AurixVisemeOH,
            Viseme::OU => AurixViseme::AurixVisemeOU,
        }
    }
}

/// Mouth state derived locally from a participant's (or our own) most recent audio frame.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct AurixVisemeFrame {
    /// Smoothed weight per `AurixViseme`, summing to ~1.
    pub weights: [f32; AURIX_VISEME_COUNT],
    /// Heaviest bucket.
    pub dominant: AurixViseme,
    /// Jaw openness `0..=1`.
    pub mouth_open: f32,
    /// RMS level of the frame `0..=1`.
    pub energy: f32,
    /// Margin between the top two buckets `0..=1`.
    pub confidence: f32,
    /// Frames analysed so far: unchanged between two reads = no new audio.
    pub sequence: u64,
}

impl From<VisemeFrame> for AurixVisemeFrame {
    fn from(f: VisemeFrame) -> Self {
        Self {
            weights: f.weights,
            dominant: f.dominant.into(),
            mouth_open: f.mouth_open,
            energy: f.energy,
            confidence: f.confidence,
            sequence: f.sequence,
        }
    }
}

/// Analyse decoded participant audio and our own outgoing voice for lip-sync (off by
/// default; one small FFT per stream per 20 ms when on). Purely local: no audio or mouth
/// data leaves the machine, and E2EE channels work because frames are decrypted here. Read
/// with `aurix_client_participant_visemes` / `aurix_client_local_visemes` every render tick.
#[no_mangle]
pub unsafe extern "C" fn aurix_client_set_visemes(
    client: *mut AurixClient,
    enabled: bool,
) -> AurixResult {
    match self::client(client) {
        Ok(c) => {
            c.set_visemes(enabled);
            AurixResult::AurixOk
        }
        Err(r) => r,
    }
}

#[no_mangle]
pub unsafe extern "C" fn aurix_client_visemes_enabled(client: *const AurixClient) -> bool {
    self::client(client).is_ok_and(|c| c.visemes_enabled())
}

/// Mouth state of `user_id` from the audio most recently played for them (voice or their
/// TTS). `false` (and `out` untouched) with analysis off or for a participant whose audio
/// is not decoded here (unknown, or only inside the server mix).
#[no_mangle]
pub unsafe extern "C" fn aurix_client_participant_visemes(
    client: *const AurixClient,
    user_id: *const AurixUuid,
    out: *mut AurixVisemeFrame,
) -> bool {
    if out.is_null() {
        return false;
    }
    match (self::client(client), uuid_arg(user_id, "user_id")) {
        (Ok(c), Ok(id)) => match c.participant_visemes(UserId(id)) {
            Some(f) => {
                *out = f.into();
                true
            }
            None => false,
        },
        _ => false,
    }
}

/// Mouth state of our own voice as sent (after DSP, gain and effects); `false` with analysis
/// off. Frozen while nothing is captured — watch `sequence`.
#[no_mangle]
pub unsafe extern "C" fn aurix_client_local_visemes(
    client: *const AurixClient,
    out: *mut AurixVisemeFrame,
) -> bool {
    if out.is_null() {
        return false;
    }
    match self::client(client).ok().and_then(|c| c.local_visemes()) {
        Some(f) => {
            *out = f.into();
            true
        }
        None => false,
    }
}

/// Built-in voice effects on the microphone (after the DSP and input gain, before VAD and
/// encoding; injected audio and the downlink are untouched). `NULL` or all-zero = off.
/// Read back (clamped) with `aurix_client_voice_effects`. Runs before the host callback.
#[no_mangle]
pub unsafe extern "C" fn aurix_client_set_voice_effects(
    client: *mut AurixClient,
    effects: *const AurixVoiceEffects,
) -> AurixResult {
    if client.is_null() {
        return null_ptr("client");
    }
    let requested = if effects.is_null() {
        AurixVoiceEffects::default()
    } else {
        *effects
    };
    let handle = &*client;
    let mut state = handle.effects.lock();
    state.builtin = VoiceEffectParams::from(requested).sanitized();
    handle.client.set_voice_effects(state.chain());
    AurixResult::AurixOk
}

/// The parameters of a built-in preset (a starting point: tweak and pass to
/// `aurix_client_set_voice_effects`). Unknown `preset` values yield the bypass.
#[no_mangle]
pub unsafe extern "C" fn aurix_voice_effects_preset(preset: AurixVoicePreset) -> AurixVoiceEffects {
    EffectPreset::from(preset).params().into()
}

/// Apply a preset directly (`aurix_voice_effects_preset` + `aurix_client_set_voice_effects`).
#[no_mangle]
pub unsafe extern "C" fn aurix_client_set_voice_preset(
    client: *mut AurixClient,
    preset: AurixVoicePreset,
) -> AurixResult {
    let effects = aurix_voice_effects_preset(preset);
    aurix_client_set_voice_effects(client, &effects)
}

#[no_mangle]
pub unsafe extern "C" fn aurix_client_voice_effects(
    client: *const AurixClient,
    out: *mut AurixVoiceEffects,
) -> AurixResult {
    if client.is_null() {
        return null_ptr("client");
    }
    if out.is_null() {
        return null_ptr("out");
    }
    *out = (*client).effects.lock().builtin.into();
    AurixResult::AurixOk
}

/// Install (or, with `NULL`, remove) a host voice effect that runs on every microphone frame
/// after the built-in stages. `user_data` is handed back verbatim on the capture thread and
/// must stay valid until the callback is removed or the client destroyed.
#[no_mangle]
pub unsafe extern "C" fn aurix_client_set_voice_effect_callback(
    client: *mut AurixClient,
    callback: AurixVoiceEffectFn,
    user_data: *mut c_void,
) -> AurixResult {
    if client.is_null() {
        return null_ptr("client");
    }
    let handle = &*client;
    let mut state = handle.effects.lock();
    state.host = callback.map(|f| HostEffect { f, user_data });
    handle.client.set_voice_effects(state.chain());
    AurixResult::AurixOk
}

/// Feed the echo canceller with audio the host plays through its own path (48 kHz,
/// interleaved, `sample_count` total samples). Audio the host obtains from
/// `aurix_client_mix_output_*` is fed automatically — do not push it again. Audio-thread safe.
#[no_mangle]
pub unsafe extern "C" fn aurix_client_push_render_f32(
    client: *mut AurixClient,
    pcm: *const f32,
    sample_count: usize,
    channels: u8,
) {
    let Ok(c) = self::client(client) else {
        return;
    };
    if pcm.is_null() || sample_count == 0 {
        return;
    }
    c.push_render_f32(std::slice::from_raw_parts(pcm, sample_count), channels);
}

#[no_mangle]
pub unsafe extern "C" fn aurix_client_push_render_i16(
    client: *mut AurixClient,
    pcm: *const i16,
    sample_count: usize,
    channels: u8,
) {
    let Ok(c) = self::client(client) else {
        return;
    };
    if pcm.is_null() || sample_count == 0 {
        return;
    }
    c.push_render_i16(std::slice::from_raw_parts(pcm, sample_count), channels);
}

#[no_mangle]
pub unsafe extern "C" fn aurix_client_set_bitrate(
    client: *mut AurixClient,
    bitrate_bps: u32,
) -> AurixResult {
    match self::client(client) {
        Ok(c) => ok(c.set_bitrate(bitrate_bps)),
        Err(r) => r,
    }
}

/// Replace the app's baseline encoder settings and re-apply them (under the current channel
/// policy when `follow_channel_policy` is on). Takes effect from the next frame.
#[no_mangle]
pub unsafe extern "C" fn aurix_client_set_encoder_settings(
    client: *mut AurixClient,
    settings: *const AurixEncoderSettings,
) -> AurixResult {
    if settings.is_null() {
        return null_ptr("settings");
    }
    match self::client(client) {
        Ok(c) => ok(c.set_encoder_settings((*settings).into())),
        Err(r) => r,
    }
}

/// Settings the encoder is running with right now (after policy and server bitrate commands).
#[no_mangle]
pub unsafe extern "C" fn aurix_client_encoder_settings(
    client: *const AurixClient,
    out: *mut AurixEncoderSettings,
) -> bool {
    let Ok(c) = self::client(client) else {
        return false;
    };
    if out.is_null() {
        return false;
    }
    *out = c.encoder_settings().into();
    true
}

/// Pin Opus complexity `0..=10` regardless of channel hints (e.g. lower it on a weak CPU);
/// a negative value unpins and returns to the channel hint / config value.
#[no_mangle]
pub unsafe extern "C" fn aurix_client_set_complexity(
    client: *mut AurixClient,
    complexity: i8,
) -> AurixResult {
    match self::client(client) {
        Ok(c) => ok(c.set_complexity(u8::try_from(complexity).ok().map(|v| v.min(10)))),
        Err(r) => r,
    }
}

/// Merged audio policy of the joined channels; `false` before the first join.
#[no_mangle]
pub unsafe extern "C" fn aurix_client_audio_policy(
    client: *const AurixClient,
    out: *mut AurixAudioPolicy,
) -> bool {
    let Ok(c) = self::client(client) else {
        return false;
    };
    if out.is_null() {
        return false;
    }
    match c.audio_policy() {
        Some(p) => {
            *out = p.into();
            true
        }
        None => false,
    }
}

/// Choose the uplink redundancy tier from the server's loss reports (`Auto`, default) or
/// pin one; FEC tuning / DRED are applied to the encoder at once and
/// `AurixEventLossProfileChanged` reports later moves.
#[no_mangle]
pub unsafe extern "C" fn aurix_client_set_loss_adaptation(
    client: *mut AurixClient,
    adaptation: AurixLossAdaptation,
) -> AurixResult {
    match self::client(client) {
        Ok(c) => ok(c.set_loss_adaptation(adaptation.into())),
        Err(r) => r,
    }
}

#[no_mangle]
pub unsafe extern "C" fn aurix_client_loss_adaptation(
    client: *const AurixClient,
) -> AurixLossAdaptation {
    match self::client(client) {
        Ok(c) => c.loss_adaptation().into(),
        Err(_) => AurixLossAdaptation::AurixLossAdaptationAuto,
    }
}

/// Redundancy tier the encoder runs with right now.
#[no_mangle]
pub unsafe extern "C" fn aurix_client_loss_profile(client: *const AurixClient) -> AurixLossProfile {
    match self::client(client) {
        Ok(c) => c.loss_profile().into(),
        Err(_) => AurixLossProfile::AurixLossProfileLow,
    }
}

/// Retune every downlink decoder (complexity → neural PLC / OSCE, OSCE bandwidth extension);
/// streams already playing switch immediately. Out-of-range values are clamped.
#[no_mangle]
pub unsafe extern "C" fn aurix_client_set_decoder_settings(
    client: *mut AurixClient,
    settings: *const AurixDecoderSettings,
) -> AurixResult {
    if settings.is_null() {
        return AurixResult::AurixInvalidArgument;
    }
    match self::client(client) {
        Ok(c) => ok(c.set_decoder_settings((*settings).into())),
        Err(r) => r,
    }
}

#[no_mangle]
pub unsafe extern "C" fn aurix_client_decoder_settings(
    client: *const AurixClient,
    out: *mut AurixDecoderSettings,
) -> bool {
    let Ok(c) = self::client(client) else {
        return false;
    };
    if out.is_null() {
        return false;
    }
    *out = c.decoder_settings().into();
    true
}

/// Whether this library's libopus codes and decodes Deep REDundancy (the bundled libopus
/// 1.6 does; without it lost frames get FEC + PLC only and `dred_duration_ms` reads back 0).
#[no_mangle]
pub unsafe extern "C" fn aurix_dred_supported() -> bool {
    crate::audio::dred_supported()
}

/// Payload of `AurixEventAudioPolicyChanged`.
#[no_mangle]
pub unsafe extern "C" fn aurix_event_audio_policy(
    event: *const AurixEvent,
    out: *mut AurixAudioPolicy,
) -> bool {
    if out.is_null() {
        return false;
    }
    match self::event(event).map(|e| &e.event) {
        Some(Event::AudioPolicyChanged(p)) => {
            *out = (*p).into();
            true
        }
        _ => false,
    }
}

/// Master playback volume `0..=2`.
#[no_mangle]
pub unsafe extern "C" fn aurix_client_set_output_volume(client: *mut AurixClient, volume: f32) {
    if let Ok(c) = self::client(client) {
        c.set_output_volume(volume);
    }
}

#[no_mangle]
pub unsafe extern "C" fn aurix_client_set_output_muted(client: *mut AurixClient, muted: bool) {
    if let Ok(c) = self::client(client) {
        c.set_output_muted(muted);
    }
}

/// Drop buffered capture samples (call after switching input devices).
#[no_mangle]
pub unsafe extern "C" fn aurix_client_reset_capture(client: *mut AurixClient) {
    if let Ok(c) = self::client(client) {
        c.reset_capture();
    }
}

// --------------------------------------------------------------------------- preferences

/// Receiver-local mute of `user_id` in `channel_id` (or everywhere when `NULL`).
#[no_mangle]
pub unsafe extern "C" fn aurix_client_set_participant_mute(
    client: *mut AurixClient,
    user_id: *const AurixUuid,
    channel_id: *const AurixUuid,
    muted: bool,
) -> AurixResult {
    let c = match self::client(client) {
        Ok(c) => c,
        Err(r) => return r,
    };
    let user = match uuid_arg(user_id, "user_id") {
        Ok(u) => UserId(u),
        Err(r) => return r,
    };
    ok(c.set_participant_mute(user, opt_uuid_arg(channel_id).map(ChannelId), muted))
}

/// Per-participant gain `0..=2` for this listener.
#[no_mangle]
pub unsafe extern "C" fn aurix_client_set_participant_volume(
    client: *mut AurixClient,
    user_id: *const AurixUuid,
    volume: f32,
) -> AurixResult {
    let c = match self::client(client) {
        Ok(c) => c,
        Err(r) => return r,
    };
    match uuid_arg(user_id, "user_id") {
        Ok(u) => ok(c.set_participant_volume(UserId(u), volume)),
        Err(r) => r,
    }
}

/// Persistent mutual block; acked by `UserBlockChanged`.
#[no_mangle]
pub unsafe extern "C" fn aurix_client_set_user_block(
    client: *mut AurixClient,
    user_id: *const AurixUuid,
    blocked: bool,
) -> AurixResult {
    let c = match self::client(client) {
        Ok(c) => c,
        Err(r) => return r,
    };
    match uuid_arg(user_id, "user_id") {
        Ok(u) => ok(c.set_user_block(UserId(u), blocked)),
        Err(r) => r,
    }
}

/// Make `user_id` (ourselves when `NULL`) a priority speaker of `channel_id`, or revoke it.
/// Others need a moderator role; our own flag can be raised with a `priority` grant and
/// always lowered. Everyone learns the outcome through `ParticipantPriorityChanged`.
#[no_mangle]
pub unsafe extern "C" fn aurix_client_set_priority(
    client: *mut AurixClient,
    channel_id: *const AurixUuid,
    user_id: *const AurixUuid,
    priority: bool,
) -> AurixResult {
    let c = match self::client(client) {
        Ok(c) => c,
        Err(r) => return r,
    };
    match uuid_arg(channel_id, "channel_id") {
        Ok(ch) => ok(c.set_priority(ChannelId(ch), opt_uuid_arg(user_id).map(UserId), priority)),
        Err(r) => r,
    }
}

/// Which joined channels receive the microphone. `channel_id` is required for `Single`.
#[no_mangle]
pub unsafe extern "C" fn aurix_client_set_transmission(
    client: *mut AurixClient,
    mode: AurixTransmissionMode,
    channel_id: *const AurixUuid,
) -> AurixResult {
    let c = match self::client(client) {
        Ok(c) => c,
        Err(r) => return r,
    };
    let mode = match mode {
        AurixTransmissionMode::AurixTransmitNone => TransmissionMode::None,
        AurixTransmissionMode::AurixTransmitAll => TransmissionMode::All,
        AurixTransmissionMode::AurixTransmitSingle => match uuid_arg(channel_id, "channel_id") {
            Ok(u) => TransmissionMode::Single {
                channel_id: ChannelId(u),
            },
            Err(r) => return r,
        },
    };
    ok(c.set_transmission(mode))
}

/// Focus one channel (others attenuated server-side); `NULL` clears.
#[no_mangle]
pub unsafe extern "C" fn aurix_client_set_channel_focus(
    client: *mut AurixClient,
    channel_id: *const AurixUuid,
) -> AurixResult {
    match self::client(client) {
        Ok(c) => ok(c.set_channel_focus(opt_uuid_arg(channel_id).map(ChannelId))),
        Err(r) => r,
    }
}

/// Ask the server to run this session on `codec`. PCMU (G.711 μ-law) is a low-CPU fallback
/// for weak devices: the node transcodes, so Opus participants of the same channel are
/// unaffected. Requires `media.pcmu_fallback` on the node (otherwise `ServerError`
/// `CODEC_NOT_AVAILABLE`); the switch takes effect on `AurixEventAudioCodecChanged`.
#[no_mangle]
pub unsafe extern "C" fn aurix_client_set_audio_codec(
    client: *mut AurixClient,
    codec: AurixAudioCodec,
) -> AurixResult {
    match self::client(client) {
        Ok(c) => ok(c.set_audio_codec(codec.into())),
        Err(r) => r,
    }
}

/// Codec the session currently uses (server-acknowledged).
#[no_mangle]
pub unsafe extern "C" fn aurix_client_audio_codec(client: *const AurixClient) -> AurixAudioCodec {
    self::client(client)
        .map(|c| c.audio_codec().into())
        .unwrap_or(AurixAudioCodec::AurixCodecOpus)
}

/// Ask the server to deliver other speakers as one mixed stereo stream per channel
/// (`AurixDownlinkMixed`) instead of one stream per speaker: constant downlink bandwidth and
/// decode cost in large channels. Mutes, volumes, focus, positional attenuation and panning
/// are applied by the node; E2EE speakers still arrive as separate streams. Requires
/// `media.downlink_mix` on the node (`AurixSessionInfo.downlink_mix`, otherwise `ServerError`
/// `DOWNLINK_MIX_NOT_AVAILABLE`); takes effect on `AurixEventDownlinkModeChanged`.
#[no_mangle]
pub unsafe extern "C" fn aurix_client_set_downlink_mode(
    client: *mut AurixClient,
    mode: AurixDownlinkMode,
) -> AurixResult {
    match self::client(client) {
        Ok(c) => ok(c.set_downlink_mode(mode.into())),
        Err(r) => r,
    }
}

/// Downlink mode the server acknowledged.
#[no_mangle]
pub unsafe extern "C" fn aurix_client_downlink_mode(
    client: *const AurixClient,
) -> AurixDownlinkMode {
    self::client(client)
        .map(|c| c.downlink_mode().into())
        .unwrap_or(AurixDownlinkMode::AurixDownlinkStreams)
}

#[no_mangle]
pub unsafe extern "C" fn aurix_client_set_transcripts(
    client: *mut AurixClient,
    enabled: bool,
) -> AurixResult {
    match self::client(client) {
        Ok(c) => ok(c.set_transcripts(enabled)),
        Err(r) => r,
    }
}

/// Receive peers' transcripts translated into `language` (BCP-47, `NULL` = original only);
/// `speech` also has them spoken into this session's downlink
/// (`AurixSessionInfo.translation_speech`). `spoken_language` (may be `NULL`) declares the
/// language this participant speaks. Applied on `AurixEventTranslationChanged`; unsupported
/// tags come back as `AurixEventServerError`.
#[no_mangle]
pub unsafe extern "C" fn aurix_client_set_translation(
    client: *mut AurixClient,
    language: *const c_char,
    spoken_language: *const c_char,
    speech: bool,
) -> AurixResult {
    let c = match self::client(client) {
        Ok(c) => c,
        Err(r) => return r,
    };
    let language = match opt_cstr_arg(language, "language") {
        Ok(v) => v,
        Err(r) => return r,
    };
    let spoken_language = match opt_cstr_arg(spoken_language, "spoken_language") {
        Ok(v) => v,
        Err(r) => return r,
    };
    ok(c.set_translation(language.as_deref(), spoken_language.as_deref(), speech))
}

/// Pose of one user for positional channels (engine coordinates; the channel config decides
/// handedness).
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct AurixPosition {
    pub user_id: AurixUuid,
    pub x: f32,
    pub y: f32,
    pub z: f32,
    pub forward_x: f32,
    pub forward_y: f32,
    pub forward_z: f32,
    pub up_x: f32,
    pub up_y: f32,
    pub up_z: f32,
}

/// Report 1..=64 poses for a positional channel.
#[no_mangle]
pub unsafe extern "C" fn aurix_client_update_positions(
    client: *mut AurixClient,
    channel_id: *const AurixUuid,
    positions: *const AurixPosition,
    count: usize,
) -> AurixResult {
    let c = match self::client(client) {
        Ok(c) => c,
        Err(r) => return r,
    };
    let ch = match uuid_arg(channel_id, "channel_id") {
        Ok(u) => ChannelId(u),
        Err(r) => return r,
    };
    if positions.is_null() || count == 0 {
        return null_ptr("positions");
    }
    let list: Vec<UserPosition> = std::slice::from_raw_parts(positions, count)
        .iter()
        .map(|p| UserPosition {
            user_id: UserId(p.user_id.into()),
            position: Position3D::new(p.x, p.y, p.z),
            orientation: Orientation3D {
                forward_x: p.forward_x,
                forward_y: p.forward_y,
                forward_z: p.forward_z,
                up_x: p.up_x,
                up_y: p.up_y,
                up_z: p.up_z,
            },
        })
        .collect();
    ok(c.update_positions(ch, list))
}

#[no_mangle]
pub unsafe extern "C" fn aurix_client_respond_recording_consent(
    client: *mut AurixClient,
    recording_id: *const AurixUuid,
    consent: AurixRecordingConsent,
) -> AurixResult {
    let c = match self::client(client) {
        Ok(c) => c,
        Err(r) => return r,
    };
    let id = match uuid_arg(recording_id, "recording_id") {
        Ok(u) => u,
        Err(r) => return r,
    };
    let consent = match consent {
        AurixRecordingConsent::AurixConsentAccepted => RecordingConsent::Accepted,
        AurixRecordingConsent::AurixConsentDeclined => RecordingConsent::Declined,
    };
    ok(c.respond_recording_consent(id, consent))
}

/// Escape hatch: send a raw client→server control message as JSON.
#[no_mangle]
pub unsafe extern "C" fn aurix_client_send_control_json(
    client: *mut AurixClient,
    json: *const c_char,
) -> AurixResult {
    let c = match self::client(client) {
        Ok(c) => c,
        Err(r) => return r,
    };
    let text = match cstr_arg(json, "json") {
        Ok(t) => t,
        Err(r) => return r,
    };
    match serde_json::from_str(&text) {
        Ok(msg) => ok(c.send_control(msg)),
        Err(e) => fail(ClientError::from(e)),
    }
}

// ---------------------------------------------------------------------------- moderation

/// Kick / mute / unmute with a one-time action token minted by the game backend.
#[no_mangle]
pub unsafe extern "C" fn aurix_client_moderate(
    client: *mut AurixClient,
    channel_id: *const AurixUuid,
    user_id: *const AurixUuid,
    action: AurixModerationAction,
    action_token: *const c_char,
    reason: *const c_char,
    request_id_out: *mut u64,
) -> AurixResult {
    let c = match self::client(client) {
        Ok(c) => c,
        Err(r) => return r,
    };
    let ch = match uuid_arg(channel_id, "channel_id") {
        Ok(u) => ChannelId(u),
        Err(r) => return r,
    };
    let user = match uuid_arg(user_id, "user_id") {
        Ok(u) => UserId(u),
        Err(r) => return r,
    };
    let token = match cstr_arg(action_token, "action_token") {
        Ok(t) => t,
        Err(r) => return r,
    };
    let reason = match opt_cstr_arg(reason, "reason") {
        Ok(r) => r,
        Err(r) => return r,
    };
    match c.moderate(ch, user, action.into(), &token, reason.as_deref()) {
        Ok(id) => {
            if !request_id_out.is_null() {
                *request_id_out = id;
            }
            AurixResult::AurixOk
        }
        Err(e) => fail(e),
    }
}

// ---------------------------------------------------------------------------------- chat

fn parse_metadata(json: Option<String>) -> Result<Option<serde_json::Value>, AurixResult> {
    match json {
        None => Ok(None),
        Some(j) => serde_json::from_str(&j).map(Some).map_err(|e| {
            set_error(&format!("metadata_json: {e}"));
            AurixResult::AurixInvalidArgument
        }),
    }
}

/// Text message to a joined channel; `metadata_json` optional.
#[no_mangle]
pub unsafe extern "C" fn aurix_client_send_chat(
    client: *mut AurixClient,
    channel_id: *const AurixUuid,
    text: *const c_char,
    metadata_json: *const c_char,
    request_id_out: *mut u64,
) -> AurixResult {
    let c = match self::client(client) {
        Ok(c) => c,
        Err(r) => return r,
    };
    let ch = match uuid_arg(channel_id, "channel_id") {
        Ok(u) => ChannelId(u),
        Err(r) => return r,
    };
    let text = match cstr_arg(text, "text") {
        Ok(t) => t,
        Err(r) => return r,
    };
    let meta = match opt_cstr_arg(metadata_json, "metadata_json").and_then(parse_metadata) {
        Ok(m) => m,
        Err(r) => return r,
    };
    match c.send_chat(ch, &text, meta) {
        Ok(id) => {
            if !request_id_out.is_null() {
                *request_id_out = id;
            }
            AurixResult::AurixOk
        }
        Err(e) => fail(e),
    }
}

/// Directed message to one online user of the same application.
#[no_mangle]
pub unsafe extern "C" fn aurix_client_send_direct_chat(
    client: *mut AurixClient,
    user_id: *const AurixUuid,
    text: *const c_char,
    metadata_json: *const c_char,
    request_id_out: *mut u64,
) -> AurixResult {
    let c = match self::client(client) {
        Ok(c) => c,
        Err(r) => return r,
    };
    let user = match uuid_arg(user_id, "user_id") {
        Ok(u) => UserId(u),
        Err(r) => return r,
    };
    let text = match cstr_arg(text, "text") {
        Ok(t) => t,
        Err(r) => return r,
    };
    let meta = match opt_cstr_arg(metadata_json, "metadata_json").and_then(parse_metadata) {
        Ok(m) => m,
        Err(r) => return r,
    };
    match c.send_direct_chat(user, &text, meta) {
        Ok(id) => {
            if !request_id_out.is_null() {
                *request_id_out = id;
            }
            AurixResult::AurixOk
        }
        Err(e) => fail(e),
    }
}

#[no_mangle]
pub unsafe extern "C" fn aurix_client_set_typing(
    client: *mut AurixClient,
    channel_id: *const AurixUuid,
    typing: bool,
) -> AurixResult {
    let c = match self::client(client) {
        Ok(c) => c,
        Err(r) => return r,
    };
    match uuid_arg(channel_id, "channel_id") {
        Ok(u) => ok(c.set_typing(ChannelId(u), typing)),
        Err(r) => r,
    }
}

/// Exactly one of `channel_id` (a joined channel) / `user_id` (direct conversation) selects
/// a stored conversation; the other must be `NULL`.
unsafe fn chat_scope_arg(
    channel_id: *const AurixUuid,
    user_id: *const AurixUuid,
) -> Result<ChatScope, AurixResult> {
    match (channel_id.is_null(), user_id.is_null()) {
        (false, true) => Ok(ChatScope::Channel(ChannelId((*channel_id).into()))),
        (true, false) => Ok(ChatScope::Direct(UserId((*user_id).into()))),
        _ => {
            set_error("exactly one of channel_id / user_id must be set");
            Err(AurixResult::AurixInvalidArgument)
        }
    }
}

/// One page of stored history, newest first, answered by `AurixEventChatHistory` (or
/// `RequestFailed`). `before` / `after` are cursors from an earlier page or from
/// `AurixChatMessage.cursor` (`NULL` = from the present); `limit` 0 = server default.
#[no_mangle]
pub unsafe extern "C" fn aurix_client_chat_history(
    client: *mut AurixClient,
    channel_id: *const AurixUuid,
    user_id: *const AurixUuid,
    before: *const c_char,
    after: *const c_char,
    limit: u32,
    request_id_out: *mut u64,
) -> AurixResult {
    let c = match self::client(client) {
        Ok(c) => c,
        Err(r) => return r,
    };
    let scope = match chat_scope_arg(channel_id, user_id) {
        Ok(s) => s,
        Err(r) => return r,
    };
    let before = match opt_cstr_arg(before, "before") {
        Ok(b) => b,
        Err(r) => return r,
    };
    let after = match opt_cstr_arg(after, "after") {
        Ok(a) => a,
        Err(r) => return r,
    };
    match c.chat_history(
        scope,
        before.as_deref(),
        after.as_deref(),
        (limit > 0).then_some(limit),
    ) {
        Ok(id) => {
            if !request_id_out.is_null() {
                *request_id_out = id;
            }
            AurixResult::AurixOk
        }
        Err(e) => fail(e),
    }
}

/// Moves this user's read marker in the conversation to `message_id` (never backwards);
/// every device of the user receives `AurixEventChatReadMarker`.
#[no_mangle]
pub unsafe extern "C" fn aurix_client_mark_chat_read(
    client: *mut AurixClient,
    channel_id: *const AurixUuid,
    user_id: *const AurixUuid,
    message_id: *const AurixUuid,
) -> AurixResult {
    let c = match self::client(client) {
        Ok(c) => c,
        Err(r) => return r,
    };
    let scope = match chat_scope_arg(channel_id, user_id) {
        Ok(s) => s,
        Err(r) => return r,
    };
    match uuid_arg(message_id, "message_id") {
        Ok(m) => ok(c.mark_chat_read(scope, m)),
        Err(r) => r,
    }
}

/// Asks for the read markers and unread count of a conversation; answered by
/// `AurixEventChatReadMarkers`.
#[no_mangle]
pub unsafe extern "C" fn aurix_client_chat_read_markers(
    client: *mut AurixClient,
    channel_id: *const AurixUuid,
    user_id: *const AurixUuid,
) -> AurixResult {
    let c = match self::client(client) {
        Ok(c) => c,
        Err(r) => return r,
    };
    match chat_scope_arg(channel_id, user_id) {
        Ok(scope) => ok(c.chat_read_markers(scope)),
        Err(r) => r,
    }
}

/// Replaces the text of this user's own message (within the server's edit window);
/// `metadata_json` `NULL` clears the metadata. Answered by `AurixEventChatMessageUpdated`
/// with this `request_id`, or `RequestFailed`.
#[no_mangle]
pub unsafe extern "C" fn aurix_client_edit_chat(
    client: *mut AurixClient,
    message_id: *const AurixUuid,
    text: *const c_char,
    metadata_json: *const c_char,
    request_id_out: *mut u64,
) -> AurixResult {
    let c = match self::client(client) {
        Ok(c) => c,
        Err(r) => return r,
    };
    let message_id = match uuid_arg(message_id, "message_id") {
        Ok(m) => m,
        Err(r) => return r,
    };
    let text = match cstr_arg(text, "text") {
        Ok(t) => t,
        Err(r) => return r,
    };
    let meta = match opt_cstr_arg(metadata_json, "metadata_json").and_then(parse_metadata) {
        Ok(m) => m,
        Err(r) => return r,
    };
    match c.edit_chat(message_id, &text, meta) {
        Ok(id) => {
            if !request_id_out.is_null() {
                *request_id_out = id;
            }
            AurixResult::AurixOk
        }
        Err(e) => fail(e),
    }
}

/// Deletes a message: this user's own (within the edit window) or, in a channel this user
/// moderates, anyone's. Answered by `AurixEventChatMessageUpdated` (a tombstone) with this
/// `request_id`, or `RequestFailed`.
#[no_mangle]
pub unsafe extern "C" fn aurix_client_delete_chat(
    client: *mut AurixClient,
    message_id: *const AurixUuid,
    request_id_out: *mut u64,
) -> AurixResult {
    let c = match self::client(client) {
        Ok(c) => c,
        Err(r) => return r,
    };
    let message_id = match uuid_arg(message_id, "message_id") {
        Ok(m) => m,
        Err(r) => return r,
    };
    match c.delete_chat(message_id) {
        Ok(id) => {
            if !request_id_out.is_null() {
                *request_id_out = id;
            }
            AurixResult::AurixOk
        }
        Err(e) => fail(e),
    }
}

/// Adds (`add`) or removes this user's `reaction` (≤ 32 bytes, no whitespace) on a message
/// of a joined channel or own direct conversation. The result reaches everyone — this
/// client included — as `AurixEventChatReactionChanged`; refusals as `ServerError`.
#[no_mangle]
pub unsafe extern "C" fn aurix_client_react_chat(
    client: *mut AurixClient,
    message_id: *const AurixUuid,
    reaction: *const c_char,
    add: bool,
) -> AurixResult {
    let c = match self::client(client) {
        Ok(c) => c,
        Err(r) => return r,
    };
    let message_id = match uuid_arg(message_id, "message_id") {
        Ok(m) => m,
        Err(r) => return r,
    };
    let reaction = match cstr_arg(reaction, "reaction") {
        Ok(r) => r,
        Err(r) => return r,
    };
    ok(c.react_chat(message_id, &reaction, add))
}

/// Full-text search over stored history: a joined `channel_id`, the direct conversation
/// with `user_id`, or every direct conversation of this user (both `NULL`). Optional
/// `from_user_id` keeps only that author's messages; `before` continues from a previous
/// result's `next_before`; `limit` 0 = server default. Answered by
/// `AurixEventChatSearchResult` (newest match first) or `RequestFailed`.
#[no_mangle]
pub unsafe extern "C" fn aurix_client_search_chat(
    client: *mut AurixClient,
    channel_id: *const AurixUuid,
    user_id: *const AurixUuid,
    query: *const c_char,
    from_user_id: *const AurixUuid,
    before: *const c_char,
    limit: u32,
    request_id_out: *mut u64,
) -> AurixResult {
    let c = match self::client(client) {
        Ok(c) => c,
        Err(r) => return r,
    };
    let scope = if channel_id.is_null() && user_id.is_null() {
        None
    } else {
        match chat_scope_arg(channel_id, user_id) {
            Ok(s) => Some(s),
            Err(r) => return r,
        }
    };
    let query = match cstr_arg(query, "query") {
        Ok(q) => q,
        Err(r) => return r,
    };
    let from_user_id = if from_user_id.is_null() {
        None
    } else {
        Some(UserId((*from_user_id).into()))
    };
    let before = match opt_cstr_arg(before, "before") {
        Ok(b) => b,
        Err(r) => return r,
    };
    match c.search_chat(
        scope,
        &query,
        from_user_id,
        before.as_deref(),
        (limit > 0).then_some(limit),
    ) {
        Ok(id) => {
            if !request_id_out.is_null() {
                *request_id_out = id;
            }
            AurixResult::AurixOk
        }
        Err(e) => fail(e),
    }
}

// ----------------------------------------------------------------------------------- TTS

/// Server-side text-to-speech as this participant's voice. `channel_id` may be `NULL` for
/// `Local`; `voice` may be `NULL` for the server default.
#[no_mangle]
pub unsafe extern "C" fn aurix_client_speak(
    client: *mut AurixClient,
    text: *const c_char,
    channel_id: *const AurixUuid,
    destination: AurixTtsDestination,
    voice: *const c_char,
    request_id_out: *mut u64,
) -> AurixResult {
    let c = match self::client(client) {
        Ok(c) => c,
        Err(r) => return r,
    };
    let text = match cstr_arg(text, "text") {
        Ok(t) => t,
        Err(r) => return r,
    };
    let voice = match opt_cstr_arg(voice, "voice") {
        Ok(v) => v,
        Err(r) => return r,
    };
    match c.speak(
        &text,
        opt_uuid_arg(channel_id).map(ChannelId),
        destination.into(),
        voice.as_deref(),
    ) {
        Ok(id) => {
            if !request_id_out.is_null() {
                *request_id_out = id;
            }
            AurixResult::AurixOk
        }
        Err(e) => fail(e),
    }
}

#[no_mangle]
pub unsafe extern "C" fn aurix_client_cancel_speech(client: *mut AurixClient) -> AurixResult {
    match self::client(client) {
        Ok(c) => ok(c.cancel_speech()),
        Err(r) => r,
    }
}

// --------------------------------------------------------------------------------- stats

/// Server-side view of the connection in both directions. `bars` is `1..=5`
/// (R ≥ 80/70/60/50 → 5/4/3/2, else 1); loss values are percentages.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct AurixNetworkQuality {
    pub bars: u8,
    pub r_factor: f32,
    pub mos: f32,
    pub rtt_ms: f32,
    pub downlink_jitter_ms: f32,
    pub downlink_loss_percent: f32,
    pub uplink_jitter_ms: f32,
    pub uplink_loss_percent: f32,
    /// Worst downlink loss among the local receivers of our audio; the loss profile follows
    /// the higher of this and `uplink_loss_percent`.
    pub receivers_loss_percent: f32,
    pub uplink_bitrate_kbps: u32,
    pub uplink_packets_received: u64,
    pub uplink_packets_lost: u64,
}

impl From<NetworkQuality> for AurixNetworkQuality {
    fn from(q: NetworkQuality) -> Self {
        Self {
            bars: q.bars,
            r_factor: q.r_factor,
            mos: q.mos,
            rtt_ms: q.rtt_ms,
            downlink_jitter_ms: q.downlink_jitter_ms,
            downlink_loss_percent: q.downlink_loss_percent,
            uplink_jitter_ms: q.uplink_jitter_ms,
            uplink_loss_percent: q.uplink_loss_percent,
            receivers_loss_percent: q.receivers_loss_percent,
            uplink_bitrate_kbps: q.uplink_bitrate_kbps,
            uplink_packets_received: q.uplink_packets_received,
            uplink_packets_lost: q.uplink_packets_lost,
        }
    }
}

/// Transport and codec counters for a network-quality indicator. Counters are lifetime
/// totals; `loss_percent`, `r_factor`, `mos` and `bars` describe the last quality period.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct AurixStats {
    pub packets_sent: u64,
    pub bytes_sent: u64,
    pub packets_received: u64,
    pub bytes_received: u64,
    pub audio_frames_received: u64,
    /// Packets that failed authentication (tampering or a stale key).
    pub bad_auth: u64,
    pub replayed: u64,
    pub heartbeats_lost: u64,
    /// Downlink frames concealed (PLC), discarded as late, and jitter-buffer underruns.
    pub frames_lost: u64,
    pub frames_late: u64,
    pub underruns: u64,
    pub frames_encoded: u64,
    pub frames_sent: u64,
    pub frames_gated: u64,
    /// Last heartbeat RTT plus min/avg/max over the session (0 until the first ack).
    pub rtt_ms: f32,
    pub rtt_min_ms: f32,
    pub rtt_avg_ms: f32,
    pub rtt_max_ms: f32,
    pub jitter_ms: f32,
    /// Downlink loss over the last quality period, as a percentage (`0..=100`).
    pub loss_percent: f32,
    /// Client-measured downlink quality; `bars` is `1..=5`.
    pub r_factor: f32,
    pub mos: f32,
    pub bars: u8,
    /// `true` when `server` holds the latest server-reported quality.
    pub has_server: bool,
    pub server: AurixNetworkQuality,
    /// Remote streams currently decoding.
    pub active_streams: u32,
    /// Link the media currently uses.
    pub media_path: AurixMediaPath,
    /// Tunnel only: uplink packets dropped because the WebSocket could not keep up.
    pub uplink_dropped: u64,
    /// Heartbeats unanswered in a row on the current link (0 = healthy).
    pub heartbeats_lost_consecutive: u32,
    /// Frames sent end-to-end encrypted (subset of `frames_sent`).
    pub frames_e2ee: u64,
    /// Encrypted downlink frames dropped: unknown sender, key not yet received, or replay.
    pub e2ee_undecryptable: u64,
    /// Of `frames_lost`, frames rebuilt from the next packet's in-band FEC / from DRED.
    pub frames_fec_recovered: u64,
    pub frames_dred_recovered: u64,
    /// Uplink redundancy tier in force.
    pub loss_profile: AurixLossProfile,
}

#[no_mangle]
pub unsafe extern "C" fn aurix_client_stats(
    client: *const AurixClient,
    out: *mut AurixStats,
) -> AurixResult {
    let c = match self::client(client) {
        Ok(c) => c,
        Err(r) => return r,
    };
    if out.is_null() {
        return null_ptr("out");
    }
    let s = c.stats();
    *out = AurixStats {
        packets_sent: s.media.packets_sent,
        bytes_sent: s.media.bytes_sent,
        packets_received: s.media.packets_received,
        bytes_received: s.media.bytes_received,
        audio_frames_received: s.media.audio_frames_received,
        bad_auth: s.media.bad_auth,
        replayed: s.media.replayed,
        heartbeats_lost: s.media.heartbeats_lost,
        frames_lost: s.frames_lost,
        frames_late: s.frames_late,
        underruns: s.underruns,
        frames_encoded: s.transmit.frames_encoded,
        frames_sent: s.transmit.frames_sent,
        frames_gated: s.transmit.frames_gated,
        rtt_ms: s.media.rtt_ms,
        rtt_min_ms: s.media.rtt_min_ms,
        rtt_avg_ms: s.media.rtt_avg_ms,
        rtt_max_ms: s.media.rtt_max_ms,
        jitter_ms: s.jitter_ms,
        loss_percent: s.loss_percent,
        r_factor: s.r_factor,
        mos: s.mos,
        bars: s.bars,
        has_server: s.server.is_some(),
        server: s.server.map(Into::into).unwrap_or_default(),
        active_streams: s.streams.len() as u32,
        media_path: s.media_path.into(),
        uplink_dropped: s.media.uplink_dropped,
        heartbeats_lost_consecutive: s.media.heartbeats_lost_consecutive,
        frames_e2ee: s.transmit.frames_e2ee,
        e2ee_undecryptable: s.transmit.e2ee_undecryptable,
        frames_fec_recovered: s.frames_fec_recovered,
        frames_dred_recovered: s.frames_dred_recovered,
        loss_profile: s.loss_profile.into(),
    };
    AurixResult::AurixOk
}

/// Latest server-reported quality. Returns `false` (and leaves `out` untouched) until the
/// server has sent its first report.
#[no_mangle]
pub unsafe extern "C" fn aurix_client_network_quality(
    client: *const AurixClient,
    out: *mut AurixNetworkQuality,
) -> bool {
    let Ok(c) = self::client(client) else {
        return false;
    };
    if out.is_null() {
        return false;
    }
    match c.network_quality() {
        Some(q) => {
            *out = q.into();
            true
        }
        None => false,
    }
}

/// Payload of `AurixEventNetworkQuality`.
#[no_mangle]
pub unsafe extern "C" fn aurix_event_network_quality(
    event: *const AurixEvent,
    out: *mut AurixNetworkQuality,
) -> bool {
    if out.is_null() {
        return false;
    }
    match self::event(event).map(|e| &e.event) {
        Some(Event::NetworkQuality(q)) => {
            *out = (*q).into();
            true
        }
        _ => false,
    }
}

// ------------------------------------------------------------------------------- regions

/// Capacity of fixed-size URL buffers in this ABI (including the NUL).
pub const AURIX_URL_LEN: usize = 512;

/// One advertised region (`GET /v1/me/regions`, `endpoint` of `POST /v1/tokens`): the
/// least-loaded healthy node of the region that has a public WebSocket URL.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct AurixRegionEndpoint {
    /// Region name as the API spells it (`us_east`, `eu_west`, ...), NUL-terminated.
    pub region: [c_char; AURIX_NAME_LEN],
    pub node_id: AurixUuid,
    /// Direct `wss://` URL of the node; use it as `AurixClientConfig::ws_url`.
    pub ws_url: [c_char; AURIX_URL_LEN],
    /// `GET` target for RTT probing (answered by exactly this node); empty when not public.
    pub probe_url: [c_char; AURIX_URL_LEN],
    pub has_location: bool,
    pub latitude: f64,
    pub longitude: f64,
    /// Great-circle distance from the location hint when both were known, else `false`.
    pub has_distance: bool,
    pub distance_km: f64,
    /// Nodes with capacity in the region.
    pub nodes: u32,
    /// Load of the advertised node, `0..1`.
    pub load_factor: f32,
    /// `true` after `aurix_regions_set_rtt` with a non-negative value.
    pub has_rtt: bool,
    pub rtt_ms: f64,
    /// `true` after `aurix_regions_set_rtt` with a negative value (unreachable).
    pub probe_failed: bool,
}

/// Opaque, mutable list of regions; release with `aurix_regions_free`.
pub struct AurixRegionList {
    regions: Vec<crate::regions::ProbedRegion>,
}

fn url_buf(s: &str) -> [c_char; AURIX_URL_LEN] {
    let mut out = [0 as c_char; AURIX_URL_LEN];
    let mut end = s.len().min(AURIX_URL_LEN - 1);
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    for (dst, src) in out.iter_mut().zip(s.as_bytes()[..end].iter()) {
        *dst = *src as c_char;
    }
    out
}

fn region_to_c(r: &crate::regions::ProbedRegion) -> AurixRegionEndpoint {
    let e = &r.endpoint;
    AurixRegionEndpoint {
        region: name_buf(&e.region.as_str().replace('-', "_")),
        node_id: e.node_id.0.into(),
        ws_url: url_buf(&e.ws_url),
        probe_url: url_buf(e.probe_url.as_deref().unwrap_or("")),
        has_location: e.location.is_some(),
        latitude: e.location.map(|l| l.latitude).unwrap_or(0.0),
        longitude: e.location.map(|l| l.longitude).unwrap_or(0.0),
        has_distance: e.distance_km.is_some(),
        distance_km: e.distance_km.unwrap_or(0.0),
        nodes: e.nodes,
        load_factor: e.load_factor,
        has_rtt: r.rtt_ms.is_some(),
        rtt_ms: r.rtt_ms.unwrap_or(0.0),
        probe_failed: r.probe_failed,
    }
}

unsafe fn regions<'a>(h: *const AurixRegionList) -> Result<&'a AurixRegionList, AurixResult> {
    if h.is_null() {
        Err(null_ptr("regions"))
    } else {
        Ok(&*h)
    }
}

/// Player-scoped discovery URL: `<api_url>/v1/me/regions` with optional `region` and
/// `latitude`/`longitude` hints (`has_location`). `preferred_region` may be `NULL`. Send it with
/// `Authorization: Bearer <player token>` from the engine's HTTP client, then pass the body to
/// `aurix_regions_parse`. Returns the number of bytes needed (excluding NUL); `buf` may be `NULL`.
#[no_mangle]
pub unsafe extern "C" fn aurix_regions_discovery_url(
    api_url: *const c_char,
    preferred_region: *const c_char,
    has_location: bool,
    latitude: f64,
    longitude: f64,
    buf: *mut c_char,
    capacity: usize,
) -> usize {
    let Ok(api) = cstr_arg(api_url, "api_url") else {
        return 0;
    };
    let Ok(preferred) = opt_cstr_arg(preferred_region, "preferred_region") else {
        return 0;
    };
    let preferred = match preferred {
        Some(name) => match crate::regions::parse_region(&name) {
            Some(r) => Some(r),
            None => {
                set_error(&format!("unknown region {name:?}"));
                return 0;
            }
        },
        None => None,
    };
    let location = has_location.then_some(crate::regions::GeoLocation {
        latitude,
        longitude,
    });
    let url = crate::regions::discovery_url(&api, preferred, location);
    if !buf.is_null() && capacity > 0 {
        let n = url.len().min(capacity - 1);
        ptr::copy_nonoverlapping(url.as_ptr(), buf as *mut u8, n);
        *buf.add(n) = 0;
    }
    url.len()
}

/// Parse a `GET /v1/me/regions` (or `/v1/regions`) body. Returns `NULL` (see `aurix_last_error`)
/// on malformed input. The list keeps the server order: preferred region first, then distance
/// (when a location hint was sent), then load.
#[no_mangle]
pub unsafe extern "C" fn aurix_regions_parse(json: *const c_char) -> *mut AurixRegionList {
    let Ok(body) = cstr_arg(json, "json") else {
        return ptr::null_mut();
    };
    match crate::regions::parse_regions(&body) {
        Ok(parsed) => Box::into_raw(Box::new(AurixRegionList {
            regions: parsed
                .regions
                .into_iter()
                .map(crate::regions::ProbedRegion::unprobed)
                .collect(),
        })),
        Err(e) => {
            fail(e);
            ptr::null_mut()
        }
    }
}

#[no_mangle]
pub unsafe extern "C" fn aurix_regions_free(regions: *mut AurixRegionList) {
    if !regions.is_null() {
        drop(Box::from_raw(regions));
    }
}

#[no_mangle]
pub unsafe extern "C" fn aurix_regions_len(regions: *const AurixRegionList) -> usize {
    self::regions(regions).map(|l| l.regions.len()).unwrap_or(0)
}

/// Copy entry `index` into `out`; `false` when out of range.
#[no_mangle]
pub unsafe extern "C" fn aurix_regions_get(
    regions: *const AurixRegionList,
    index: usize,
    out: *mut AurixRegionEndpoint,
) -> bool {
    let Ok(l) = self::regions(regions) else {
        return false;
    };
    match l.regions.get(index) {
        Some(r) if !out.is_null() => {
            *out = region_to_c(r);
            true
        }
        _ => false,
    }
}

/// Record the host's probe of entry `index` (best of a few `GET probe_url` samples after one
/// discarded warm-up): `rtt_ms >= 0` for a measurement, negative when every request failed.
#[no_mangle]
pub unsafe extern "C" fn aurix_regions_set_rtt(
    regions: *mut AurixRegionList,
    index: usize,
    rtt_ms: f64,
) -> AurixResult {
    if regions.is_null() {
        return null_ptr("regions");
    }
    let list = &mut *regions;
    match list.regions.get_mut(index) {
        Some(r) => {
            r.set_rtt((rtt_ms >= 0.0).then_some(rtt_ms));
            AurixResult::AurixOk
        }
        None => {
            set_error("region index out of range");
            AurixResult::AurixInvalidArgument
        }
    }
}

/// Re-rank in place after probing: `preferred_region` (may be `NULL`) first unless its probe
/// failed, then measured regions in ascending `rtt_tolerance_ms` buckets (`<= 0` selects the
/// default 15 ms), then unprobed regions, then unreachable ones; ties keep the server order.
/// Entry 0 is the recommendation.
#[no_mangle]
pub unsafe extern "C" fn aurix_regions_rank(
    regions: *mut AurixRegionList,
    preferred_region: *const c_char,
    rtt_tolerance_ms: f64,
) -> AurixResult {
    if regions.is_null() {
        return null_ptr("regions");
    }
    let preferred = match opt_cstr_arg(preferred_region, "preferred_region") {
        Ok(Some(name)) => match crate::regions::parse_region(&name) {
            Some(r) => Some(r),
            None => {
                set_error(&format!("unknown region {name:?}"));
                return AurixResult::AurixInvalidArgument;
            }
        },
        Ok(None) => None,
        Err(r) => return r,
    };
    let tolerance = if rtt_tolerance_ms > 0.0 {
        rtt_tolerance_ms
    } else {
        crate::regions::DEFAULT_RTT_TOLERANCE_MS
    };
    let list = &mut *regions;
    list.regions =
        crate::regions::rank_regions(std::mem::take(&mut list.regions), preferred, tolerance);
    AurixResult::AurixOk
}

// -------------------------------------------------------------------------------- bare codec
//
// A standalone Opus encoder/decoder pair for hosts that run their own capture/playback
// pipeline (the Unity SDK's `NativeOpusCodec`). libopus is linked statically into this
// library, and every control is a plain non-variadic function, so P/Invoke works on every
// platform including Apple arm64 (where calling `opus_encoder_ctl` through a fixed-arity
// P/Invoke signature is undefined).

/// Opaque bare Opus encoder (see `aurix_opus_encoder_create`).
pub struct AurixOpusEncoder {
    inner: crate::audio::OpusEncoder,
}

/// Opaque bare Opus decoder (see `aurix_opus_decoder_create`).
pub struct AurixOpusDecoder {
    inner: crate::audio::OpusDecoder,
}

fn codec_fail(e: crate::audio::CodecError) -> AurixResult {
    set_error(&e.to_string());
    match e {
        crate::audio::CodecError::BadChannels(_) | crate::audio::CodecError::BadFrame => {
            AurixResult::AurixInvalidArgument
        }
        crate::audio::CodecError::Opus(_) => AurixResult::AurixCodec,
    }
}

/// Encoder-side error code for the `encode`/`decode` calls: negative `AurixResult`.
fn codec_err_i32(e: crate::audio::CodecError) -> i32 {
    -(codec_fail(e) as i32)
}

/// Create an Opus encoder for `sample_rate_hz` (8000/12000/16000/24000/48000) and 1 or 2
/// channels with `settings` (NULL = defaults). Returns NULL on error (see `aurix_last_error`).
#[no_mangle]
pub unsafe extern "C" fn aurix_opus_encoder_create(
    sample_rate_hz: u32,
    channels: u8,
    settings: *const AurixEncoderSettings,
) -> *mut AurixOpusEncoder {
    let settings = if settings.is_null() {
        EncoderSettings::default()
    } else {
        (*settings).into()
    };
    match crate::audio::OpusEncoder::new(sample_rate_hz, channels, settings) {
        Ok(inner) => Box::into_raw(Box::new(AurixOpusEncoder { inner })),
        Err(e) => {
            codec_fail(e);
            ptr::null_mut()
        }
    }
}

#[no_mangle]
pub unsafe extern "C" fn aurix_opus_encoder_destroy(encoder: *mut AurixOpusEncoder) {
    if !encoder.is_null() {
        drop(Box::from_raw(encoder));
    }
}

/// Push new settings into the encoder; takes effect from the next frame. Out-of-range values
/// are clamped (read them back with `aurix_opus_encoder_settings`).
#[no_mangle]
pub unsafe extern "C" fn aurix_opus_encoder_apply(
    encoder: *mut AurixOpusEncoder,
    settings: *const AurixEncoderSettings,
) -> AurixResult {
    if encoder.is_null() {
        return null_ptr("encoder");
    }
    if settings.is_null() {
        return null_ptr("settings");
    }
    match (*encoder).inner.apply((*settings).into()) {
        Ok(()) => AurixResult::AurixOk,
        Err(e) => codec_fail(e.into()),
    }
}

/// The settings the encoder is running with (as clamped).
#[no_mangle]
pub unsafe extern "C" fn aurix_opus_encoder_settings(
    encoder: *const AurixOpusEncoder,
    out: *mut AurixEncoderSettings,
) -> bool {
    if encoder.is_null() || out.is_null() {
        return false;
    }
    *out = (*encoder).inner.settings().into();
    true
}

/// Encode one interleaved f32 frame of `frame_samples_per_channel` samples per channel (a
/// valid Opus frame size: 2.5/5/10/20/40/60 ms). Returns the packet length written to `out`,
/// or a negative `AurixResult` code.
#[no_mangle]
pub unsafe extern "C" fn aurix_opus_encoder_encode_f32(
    encoder: *mut AurixOpusEncoder,
    pcm: *const f32,
    frame_samples_per_channel: usize,
    out: *mut u8,
    out_len: usize,
) -> i32 {
    if encoder.is_null() || pcm.is_null() || out.is_null() {
        return -(null_ptr("encoder/pcm/out") as i32);
    }
    let enc = &mut (*encoder).inner;
    let n = frame_samples_per_channel.saturating_mul(enc.channels());
    let pcm = std::slice::from_raw_parts(pcm, n);
    let out = std::slice::from_raw_parts_mut(out, out_len);
    match enc.encode_f32(pcm, out) {
        Ok(len) => len as i32,
        Err(e) => codec_err_i32(e),
    }
}

/// `aurix_opus_encoder_encode_f32` for interleaved i16 PCM.
#[no_mangle]
pub unsafe extern "C" fn aurix_opus_encoder_encode_i16(
    encoder: *mut AurixOpusEncoder,
    pcm: *const i16,
    frame_samples_per_channel: usize,
    out: *mut u8,
    out_len: usize,
) -> i32 {
    if encoder.is_null() || pcm.is_null() || out.is_null() {
        return -(null_ptr("encoder/pcm/out") as i32);
    }
    let enc = &mut (*encoder).inner;
    let n = frame_samples_per_channel.saturating_mul(enc.channels());
    let pcm = std::slice::from_raw_parts(pcm, n);
    let out = std::slice::from_raw_parts_mut(out, out_len);
    match enc.encode_i16(pcm, out) {
        Ok(len) => len as i32,
        Err(e) => codec_err_i32(e),
    }
}

/// Create an Opus decoder for `sample_rate_hz` and 1 or 2 channels. NULL on error.
#[no_mangle]
pub unsafe extern "C" fn aurix_opus_decoder_create(
    sample_rate_hz: u32,
    channels: u8,
) -> *mut AurixOpusDecoder {
    match crate::audio::OpusDecoder::new(sample_rate_hz, channels) {
        Ok(inner) => Box::into_raw(Box::new(AurixOpusDecoder { inner })),
        Err(e) => {
            codec_fail(e);
            ptr::null_mut()
        }
    }
}

#[no_mangle]
pub unsafe extern "C" fn aurix_opus_decoder_destroy(decoder: *mut AurixOpusDecoder) {
    if !decoder.is_null() {
        drop(Box::from_raw(decoder));
    }
}

/// Decode one packet into interleaved f32 PCM with room for `max_frame_samples_per_channel`
/// samples per channel. `packet == NULL` or `packet_len == 0` runs packet-loss concealment
/// for exactly `max_frame_samples_per_channel` samples. `fec` decodes the in-band FEC data
/// carried by `packet` for the *previous* (lost) frame instead of the packet's own audio.
/// Returns samples per channel written, or a negative `AurixResult` code.
#[no_mangle]
pub unsafe extern "C" fn aurix_opus_decoder_decode_f32(
    decoder: *mut AurixOpusDecoder,
    packet: *const u8,
    packet_len: usize,
    pcm: *mut f32,
    max_frame_samples_per_channel: usize,
    fec: bool,
) -> i32 {
    if decoder.is_null() || pcm.is_null() {
        return -(null_ptr("decoder/pcm") as i32);
    }
    let dec = &mut (*decoder).inner;
    let packet: &[u8] = if packet.is_null() || packet_len == 0 {
        &[]
    } else {
        std::slice::from_raw_parts(packet, packet_len)
    };
    let n = max_frame_samples_per_channel.saturating_mul(dec.channels());
    let pcm = std::slice::from_raw_parts_mut(pcm, n);
    match dec.decode_f32(packet, pcm, fec) {
        Ok(len) => len as i32,
        Err(e) => codec_err_i32(e),
    }
}

/// `aurix_opus_decoder_decode_f32` for interleaved i16 PCM.
#[no_mangle]
pub unsafe extern "C" fn aurix_opus_decoder_decode_i16(
    decoder: *mut AurixOpusDecoder,
    packet: *const u8,
    packet_len: usize,
    pcm: *mut i16,
    max_frame_samples_per_channel: usize,
    fec: bool,
) -> i32 {
    if decoder.is_null() || pcm.is_null() {
        return -(null_ptr("decoder/pcm") as i32);
    }
    let dec = &mut (*decoder).inner;
    let packet: &[u8] = if packet.is_null() || packet_len == 0 {
        &[]
    } else {
        std::slice::from_raw_parts(packet, packet_len)
    };
    let n = max_frame_samples_per_channel.saturating_mul(dec.channels());
    let pcm = std::slice::from_raw_parts_mut(pcm, n);
    match dec.decode_i16(packet, pcm, fec) {
        Ok(len) => len as i32,
        Err(e) => codec_err_i32(e),
    }
}

/// Retune a bare decoder (complexity → neural PLC / OSCE, OSCE bandwidth extension); takes
/// effect from the next frame. Out-of-range values are clamped.
#[no_mangle]
pub unsafe extern "C" fn aurix_opus_decoder_apply(
    decoder: *mut AurixOpusDecoder,
    settings: *const AurixDecoderSettings,
) -> AurixResult {
    if decoder.is_null() {
        return null_ptr("decoder");
    }
    if settings.is_null() {
        return null_ptr("settings");
    }
    match (*decoder).inner.apply((*settings).into()) {
        Ok(()) => AurixResult::AurixOk,
        Err(e) => codec_fail(e),
    }
}

/// The settings a bare decoder is running with.
#[no_mangle]
pub unsafe extern "C" fn aurix_opus_decoder_settings(
    decoder: *mut AurixOpusDecoder,
    out: *mut AurixDecoderSettings,
) -> bool {
    if decoder.is_null() || out.is_null() {
        return false;
    }
    *out = (*decoder).inner.settings().into();
    true
}

/// Rebuild the frame `frames_before` frames before `packet` (1 = the one right before it, 2
/// = the one before that, …) from the Deep REDundancy the packet carries, into one frame of
/// `frame_samples_per_channel` samples per channel. Use it for the frames of a gap that the
/// next packet's in-band FEC does not cover, before falling back to PLC. Returns samples per
/// channel written, `0` when the packet's DRED does not reach that far (or this libopus has
/// none — see `aurix_dred_supported`), or a negative `AurixResult` code.
#[no_mangle]
pub unsafe extern "C" fn aurix_opus_decoder_dred_decode_f32(
    decoder: *mut AurixOpusDecoder,
    packet: *const u8,
    packet_len: usize,
    frames_before: u32,
    pcm: *mut f32,
    frame_samples_per_channel: usize,
) -> i32 {
    if decoder.is_null() || packet.is_null() || pcm.is_null() {
        return -(null_ptr("decoder/packet/pcm") as i32);
    }
    let dec = &mut (*decoder).inner;
    let packet = std::slice::from_raw_parts(packet, packet_len);
    let n = frame_samples_per_channel.saturating_mul(dec.channels());
    let pcm = std::slice::from_raw_parts_mut(pcm, n);
    match dec.dred_decode_f32(packet, frames_before as usize, pcm) {
        Ok(len) => len as i32,
        Err(e) => codec_err_i32(e),
    }
}

/// `aurix_opus_decoder_dred_decode_f32` for interleaved i16 PCM.
#[no_mangle]
pub unsafe extern "C" fn aurix_opus_decoder_dred_decode_i16(
    decoder: *mut AurixOpusDecoder,
    packet: *const u8,
    packet_len: usize,
    frames_before: u32,
    pcm: *mut i16,
    frame_samples_per_channel: usize,
) -> i32 {
    if decoder.is_null() || packet.is_null() || pcm.is_null() {
        return -(null_ptr("decoder/packet/pcm") as i32);
    }
    let dec = &mut (*decoder).inner;
    let packet = std::slice::from_raw_parts(packet, packet_len);
    let n = frame_samples_per_channel.saturating_mul(dec.channels());
    let pcm = std::slice::from_raw_parts_mut(pcm, n);
    match dec.dred_decode_i16(packet, frames_before as usize, pcm) {
        Ok(len) => len as i32,
        Err(e) => codec_err_i32(e),
    }
}

/// Whether an Opus packet carries in-band FEC (LBRR) for the frame before it — the cheap
/// check before `aurix_opus_decoder_decode_*` with `fec = true`.
#[no_mangle]
pub unsafe extern "C" fn aurix_opus_packet_has_fec(packet: *const u8, packet_len: usize) -> bool {
    if packet.is_null() || packet_len == 0 {
        return false;
    }
    let packet = std::slice::from_raw_parts(packet, packet_len);
    opus::packet::has_lbrr(packet).unwrap_or(false)
}

// ---------------------------------------------------------------------- standalone DSP
//
// The capture DSP chain as a bare processor for hosts that run their own encoder (the Unity
// SDK, engines with their own Opus path): mono 48 kHz in place, 10 ms granularity.

/// Opaque standalone DSP processor (see `aurix_dsp_create`).
pub struct AurixDsp {
    inner: crate::dsp::Dsp,
    far_end: crate::dsp::FarEndHandle,
}

/// Samples per 10 ms DSP block at `AURIX_SAMPLE_RATE`; `aurix_dsp_process_f32` takes whole
/// multiples of this.
pub const AURIX_DSP_BLOCK_SAMPLES: u32 = crate::dsp::BLOCK as u32;

/// Create a standalone DSP processor with `config` (NULL = `aurix_dsp_config_default`).
#[no_mangle]
pub unsafe extern "C" fn aurix_dsp_create(config: *const AurixDspConfig) -> *mut AurixDsp {
    let cfg = if config.is_null() {
        DspConfig::default()
    } else {
        (*config).into()
    };
    let inner = crate::dsp::Dsp::new(cfg);
    let far_end = inner.far_end();
    Box::into_raw(Box::new(AurixDsp { inner, far_end }))
}

#[no_mangle]
pub unsafe extern "C" fn aurix_dsp_destroy(dsp: *mut AurixDsp) {
    if !dsp.is_null() {
        drop(Box::from_raw(dsp));
    }
}

/// Replace the configuration (clamped; read back with `aurix_dsp_config`).
#[no_mangle]
pub unsafe extern "C" fn aurix_dsp_set_config(
    dsp: *mut AurixDsp,
    config: *const AurixDspConfig,
) -> AurixResult {
    if dsp.is_null() || config.is_null() {
        return null_ptr("dsp/config");
    }
    (*dsp).inner.set_config((*config).into());
    AurixResult::AurixOk
}

#[no_mangle]
pub unsafe extern "C" fn aurix_dsp_config(
    dsp: *const AurixDsp,
    out: *mut AurixDspConfig,
) -> AurixResult {
    if dsp.is_null() || out.is_null() {
        return null_ptr("dsp/out");
    }
    *out = (*dsp).inner.config().into();
    AurixResult::AurixOk
}

#[no_mangle]
pub unsafe extern "C" fn aurix_dsp_stats(
    dsp: *const AurixDsp,
    out: *mut AurixDspStats,
) -> AurixResult {
    if dsp.is_null() || out.is_null() {
        return null_ptr("dsp/out");
    }
    *out = (*dsp).inner.stats().into();
    AurixResult::AurixOk
}

/// Process `sample_count` mono 48 kHz samples in place; `sample_count` must be a non-zero
/// multiple of `AURIX_DSP_BLOCK_SAMPLES`. Not thread-safe against itself; safe to call
/// concurrently with `aurix_dsp_push_render_f32`.
#[no_mangle]
pub unsafe extern "C" fn aurix_dsp_process_f32(
    dsp: *mut AurixDsp,
    pcm: *mut f32,
    sample_count: usize,
) -> AurixResult {
    if dsp.is_null() || pcm.is_null() {
        return null_ptr("dsp/pcm");
    }
    if sample_count == 0 || !sample_count.is_multiple_of(crate::dsp::BLOCK) {
        set_error("sample_count must be a non-zero multiple of AURIX_DSP_BLOCK_SAMPLES");
        return AurixResult::AurixInvalidArgument;
    }
    (*dsp)
        .inner
        .process_frame(std::slice::from_raw_parts_mut(pcm, sample_count));
    AurixResult::AurixOk
}

/// Echo-canceller reference: what the host is playing (48 kHz, interleaved `channels`,
/// `sample_count` total samples). Real-time thread safe; no-op while AEC is off.
#[no_mangle]
pub unsafe extern "C" fn aurix_dsp_push_render_f32(
    dsp: *const AurixDsp,
    pcm: *const f32,
    sample_count: usize,
    channels: u8,
) {
    if dsp.is_null() || pcm.is_null() || sample_count == 0 {
        return;
    }
    (*dsp)
        .far_end
        .push(std::slice::from_raw_parts(pcm, sample_count), channels);
}

// ----------------------------------------------- standalone voice effects / visemes
//
// The same built-in effect stages and lip-sync analyser the client runs internally, as bare
// processors for hosts with their own capture/playback path (the Unity SDK, engines with
// their own Opus pipeline). 48 kHz interleaved PCM, in place.

/// Opaque standalone voice-effect chain (see `aurix_voice_effects_create`).
pub struct AurixVoiceEffectsProcessor {
    params: VoiceEffectParams,
    chain: EffectChain,
}

/// Create a standalone effect chain from `effects` (NULL or all-zero = bypass, which passes
/// audio through untouched).
#[no_mangle]
pub unsafe extern "C" fn aurix_voice_effects_create(
    effects: *const AurixVoiceEffects,
) -> *mut AurixVoiceEffectsProcessor {
    let requested = if effects.is_null() {
        AurixVoiceEffects::default()
    } else {
        *effects
    };
    let params = VoiceEffectParams::from(requested).sanitized();
    let chain = params.chain();
    Box::into_raw(Box::new(AurixVoiceEffectsProcessor { params, chain }))
}

#[no_mangle]
pub unsafe extern "C" fn aurix_voice_effects_destroy(processor: *mut AurixVoiceEffectsProcessor) {
    if !processor.is_null() {
        drop(Box::from_raw(processor));
    }
}

/// Replace the parameters (clamped; read back with `aurix_voice_effects_get`). Rebuilds the
/// stages, so any tails (reverb, pitch buffers) restart. Not thread-safe against
/// `aurix_voice_effects_process_f32`.
#[no_mangle]
pub unsafe extern "C" fn aurix_voice_effects_set(
    processor: *mut AurixVoiceEffectsProcessor,
    effects: *const AurixVoiceEffects,
) -> AurixResult {
    if processor.is_null() {
        return null_ptr("processor");
    }
    let requested = if effects.is_null() {
        AurixVoiceEffects::default()
    } else {
        *effects
    };
    let params = VoiceEffectParams::from(requested).sanitized();
    let p = &mut *processor;
    if params != p.params {
        p.params = params;
        p.chain = params.chain();
    }
    AurixResult::AurixOk
}

#[no_mangle]
pub unsafe extern "C" fn aurix_voice_effects_get(
    processor: *const AurixVoiceEffectsProcessor,
    out: *mut AurixVoiceEffects,
) -> AurixResult {
    if processor.is_null() || out.is_null() {
        return null_ptr("processor/out");
    }
    *out = (*processor).params.into();
    AurixResult::AurixOk
}

/// True when every stage is off (processing is a no-op).
#[no_mangle]
pub unsafe extern "C" fn aurix_voice_effects_is_bypass(
    processor: *const AurixVoiceEffectsProcessor,
) -> bool {
    !processor.is_null() && (*processor).params.is_bypass()
}

/// Process `sample_count` interleaved 48 kHz samples (`channels` 1 or 2) in place; the chain
/// is stateful, so feed consecutive frames of one stream. `sample_count` must be a non-zero
/// multiple of `channels`. Not thread-safe against itself or `aurix_voice_effects_set`.
#[no_mangle]
pub unsafe extern "C" fn aurix_voice_effects_process_f32(
    processor: *mut AurixVoiceEffectsProcessor,
    pcm: *mut f32,
    sample_count: usize,
    channels: u8,
) -> AurixResult {
    if processor.is_null() || pcm.is_null() {
        return null_ptr("processor/pcm");
    }
    if !(1..=2).contains(&channels) {
        set_error("channels must be 1 or 2");
        return AurixResult::AurixInvalidArgument;
    }
    if sample_count == 0 || !sample_count.is_multiple_of(channels as usize) {
        set_error("sample_count must be a non-zero multiple of channels");
        return AurixResult::AurixInvalidArgument;
    }
    (*processor)
        .chain
        .process(std::slice::from_raw_parts_mut(pcm, sample_count), channels);
    AurixResult::AurixOk
}

/// Clear the stages' internal state (tails, phases) without changing parameters — call on
/// a stream discontinuity such as a device switch.
#[no_mangle]
pub unsafe extern "C" fn aurix_voice_effects_reset(processor: *mut AurixVoiceEffectsProcessor) {
    if !processor.is_null() {
        (*processor).chain.reset();
    }
}

/// Opaque standalone lip-sync analyser (see `aurix_viseme_analyzer_create`).
pub struct AurixVisemeAnalyzer {
    inner: VisemeAnalyzer,
}

/// Create a standalone lip-sync analyser for one audio stream (one per participant, one for
/// the local microphone).
#[no_mangle]
pub extern "C" fn aurix_viseme_analyzer_create() -> *mut AurixVisemeAnalyzer {
    Box::into_raw(Box::new(AurixVisemeAnalyzer {
        inner: VisemeAnalyzer::new(),
    }))
}

#[no_mangle]
pub unsafe extern "C" fn aurix_viseme_analyzer_destroy(analyzer: *mut AurixVisemeAnalyzer) {
    if !analyzer.is_null() {
        drop(Box::from_raw(analyzer));
    }
}

/// Analyse one 20 ms frame of interleaved 48 kHz PCM (`sample_count` total samples across
/// `channels`, downmixed to mono; shorter frames are zero-padded, longer ones truncated to
/// `AURIX_FRAME_SAMPLES` per channel). Not thread-safe against itself.
#[no_mangle]
pub unsafe extern "C" fn aurix_viseme_analyzer_push_f32(
    analyzer: *mut AurixVisemeAnalyzer,
    pcm: *const f32,
    sample_count: usize,
    channels: u8,
) -> AurixResult {
    if analyzer.is_null() || pcm.is_null() {
        return null_ptr("analyzer/pcm");
    }
    if channels == 0 || sample_count == 0 || !sample_count.is_multiple_of(channels as usize) {
        set_error("sample_count must be a non-zero multiple of channels (>= 1)");
        return AurixResult::AurixInvalidArgument;
    }
    (*analyzer)
        .inner
        .push(std::slice::from_raw_parts(pcm, sample_count), channels);
    AurixResult::AurixOk
}

/// The smoothed mouth state after the last pushed frame (`sequence` counts pushes).
#[no_mangle]
pub unsafe extern "C" fn aurix_viseme_analyzer_frame(
    analyzer: *const AurixVisemeAnalyzer,
    out: *mut AurixVisemeFrame,
) -> AurixResult {
    if analyzer.is_null() || out.is_null() {
        return null_ptr("analyzer/out");
    }
    *out = (*analyzer).inner.frame().into();
    AurixResult::AurixOk
}

/// Back to silence (keeps `sequence` so readers still see a change) — call when the stream
/// stops or switches to another speaker.
#[no_mangle]
pub unsafe extern "C" fn aurix_viseme_analyzer_reset(analyzer: *mut AurixVisemeAnalyzer) {
    if !analyzer.is_null() {
        (*analyzer).inner.reset();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn null_handles_are_rejected_not_dereferenced() {
        unsafe {
            assert_eq!(
                aurix_client_connect(ptr::null_mut()),
                AurixResult::AurixNullPointer
            );
            assert_eq!(
                aurix_client_state(ptr::null()),
                AurixConnectionState::AurixStateDisconnected
            );
            assert!(aurix_client_poll_event(ptr::null_mut()).is_null());
            aurix_client_destroy(ptr::null_mut());
            aurix_event_free(ptr::null_mut());
            let msg = CStr::from_ptr(aurix_last_error()).to_str().unwrap();
            assert!(msg.contains("NULL"), "{msg}");
        }
    }

    #[test]
    fn create_requires_url_and_token() {
        unsafe {
            let mut cfg = std::mem::MaybeUninit::<AurixClientConfig>::uninit();
            aurix_client_config_default(cfg.as_mut_ptr());
            let mut cfg = cfg.assume_init();
            assert!(aurix_client_create(&cfg).is_null());
            let url = CString::new("http://not-a-websocket").unwrap();
            let token = CString::new("t").unwrap();
            cfg.ws_url = url.as_ptr();
            cfg.token = token.as_ptr();
            let client = aurix_client_create(&cfg);
            assert!(!client.is_null());
            assert_eq!(
                aurix_client_state(client),
                AurixConnectionState::AurixStateDisconnected
            );
            assert_eq!(
                aurix_client_join_channel(client, ptr::null(), ptr::null(), ptr::null_mut()),
                AurixResult::AurixNullPointer
            );
            let ch = AurixUuid::from(Uuid::new_v4());
            assert_eq!(
                aurix_client_join_channel(client, &ch, ptr::null(), ptr::null_mut()),
                AurixResult::AurixNotConnected
            );
            let mut stats = AurixStats::default();
            assert_eq!(aurix_client_stats(client, &mut stats), AurixResult::AurixOk);
            let mut out = vec![0f32; 960 * 2];
            assert_eq!(
                aurix_client_mix_output_f32(client, out.as_mut_ptr(), out.len(), 2),
                0
            );
            let mut dsp = std::mem::MaybeUninit::<AurixDspConfig>::uninit();
            assert_eq!(
                aurix_client_dsp(client, dsp.as_mut_ptr()),
                AurixResult::AurixOk
            );
            let mut dsp = dsp.assume_init();
            assert_eq!(dsp, AurixDspConfig::from(DspConfig::default()));
            dsp.echo_tail_ms = 5000;
            dsp.noise_suppression = AurixNoiseSuppression::AurixNoiseSuppressionLow;
            dsp.agc = false;
            assert_eq!(aurix_client_set_dsp(client, &dsp), AurixResult::AurixOk);
            let mut back = std::mem::MaybeUninit::<AurixDspConfig>::uninit();
            assert_eq!(
                aurix_client_dsp(client, back.as_mut_ptr()),
                AurixResult::AurixOk
            );
            let back = back.assume_init();
            assert_eq!(back.echo_tail_ms, AURIX_DSP_MAX_ECHO_TAIL_MS);
            assert_eq!(
                back.noise_suppression,
                AurixNoiseSuppression::AurixNoiseSuppressionLow
            );
            assert!(!back.agc);
            let mut stats = std::mem::MaybeUninit::<AurixDspStats>::uninit();
            assert_eq!(
                aurix_client_dsp_stats(client, stats.as_mut_ptr()),
                AurixResult::AurixOk
            );
            assert_eq!(stats.assume_init().agc_gain_db, 0.0);
            aurix_client_push_render_f32(client, out.as_ptr(), out.len(), 2);
            aurix_client_destroy(client);
        }
    }

    #[test]
    fn standalone_dsp_processes_blocks_and_rejects_partial_ones() {
        unsafe {
            let mut cfg = std::mem::MaybeUninit::<AurixDspConfig>::uninit();
            aurix_dsp_config_bypass(cfg.as_mut_ptr());
            let mut cfg = cfg.assume_init();
            assert_eq!(cfg, AurixDspConfig::from(DspConfig::BYPASS));
            cfg.high_pass = true;
            let dsp = aurix_dsp_create(&cfg);
            assert!(!dsp.is_null());

            let mut back = std::mem::MaybeUninit::<AurixDspConfig>::uninit();
            assert_eq!(
                aurix_dsp_config(dsp, back.as_mut_ptr()),
                AurixResult::AurixOk
            );
            assert!(back.assume_init().high_pass);

            let mut pcm = vec![0.5f32; AURIX_DSP_BLOCK_SAMPLES as usize * 2];
            assert_eq!(
                aurix_dsp_process_f32(dsp, pcm.as_mut_ptr(), pcm.len() - 1),
                AurixResult::AurixInvalidArgument
            );
            assert!(pcm.iter().all(|s| *s == 0.5));
            for _ in 0..50 {
                pcm.fill(0.5);
                assert_eq!(
                    aurix_dsp_process_f32(dsp, pcm.as_mut_ptr(), pcm.len()),
                    AurixResult::AurixOk
                );
            }
            let dc = pcm.iter().sum::<f32>() / pcm.len() as f32;
            assert!(dc.abs() < 0.01, "high-pass left DC {dc}");

            aurix_dsp_push_render_f32(dsp, pcm.as_ptr(), pcm.len(), 1);
            let mut stats = std::mem::MaybeUninit::<AurixDspStats>::uninit();
            assert_eq!(
                aurix_dsp_stats(dsp, stats.as_mut_ptr()),
                AurixResult::AurixOk
            );
            assert!(!stats.assume_init().far_end_active);

            assert_eq!(
                aurix_dsp_set_config(dsp, ptr::null()),
                AurixResult::AurixNullPointer
            );
            aurix_dsp_destroy(dsp);
            aurix_dsp_destroy(ptr::null_mut());
        }
    }

    #[test]
    fn encoder_settings_round_trip_through_the_abi() {
        unsafe {
            let mut cfg = std::mem::MaybeUninit::<AurixClientConfig>::uninit();
            aurix_client_config_default(cfg.as_mut_ptr());
            let mut cfg = cfg.assume_init();
            assert_eq!(
                cfg.encoder,
                AurixEncoderSettings::from(EncoderSettings::default())
            );
            assert!(cfg.follow_channel_policy);
            let url = CString::new("ws://localhost:1/ws").unwrap();
            let token = CString::new("t").unwrap();
            cfg.ws_url = url.as_ptr();
            cfg.token = token.as_ptr();
            cfg.encoder.bitrate_bps = 20_000;
            cfg.encoder.complexity = 99; // clamped to 10
            cfg.encoder.max_bandwidth = AurixOpusBandwidth::AurixBandwidthWideband;
            cfg.encoder.signal = AurixOpusSignal::AurixSignalMusic;
            cfg.encoder.vbr = false;
            cfg.encoder.dtx = true;
            let client = aurix_client_create(&cfg);
            assert!(!client.is_null());

            let mut got = cfg.encoder;
            assert!(aurix_client_encoder_settings(client, &mut got));
            assert_eq!(
                (
                    got.bitrate_bps,
                    got.complexity,
                    got.max_bandwidth,
                    got.signal,
                    got.vbr,
                    got.dtx
                ),
                (
                    20_000,
                    10,
                    AurixOpusBandwidth::AurixBandwidthWideband,
                    AurixOpusSignal::AurixSignalMusic,
                    false,
                    true
                )
            );

            assert_eq!(aurix_client_set_complexity(client, 3), AurixResult::AurixOk);
            assert!(aurix_client_encoder_settings(client, &mut got));
            assert_eq!(got.complexity, 3);
            // Pinned complexity survives a baseline replacement; unpinning restores it.
            let mut next = got;
            next.complexity = 7;
            next.bitrate_bps = 1; // clamped to the floor
            assert_eq!(
                aurix_client_set_encoder_settings(client, &next),
                AurixResult::AurixOk
            );
            assert!(aurix_client_encoder_settings(client, &mut got));
            assert_eq!(
                (got.complexity, got.bitrate_bps),
                (3, EncoderSettings::MIN_BITRATE)
            );
            assert_eq!(
                aurix_client_set_complexity(client, -1),
                AurixResult::AurixOk
            );
            assert!(aurix_client_encoder_settings(client, &mut got));
            assert_eq!(got.complexity, 7);

            let mut policy = AurixAudioPolicy::from(AudioPolicy::default());
            assert!(
                !aurix_client_audio_policy(client, &mut policy),
                "no channel joined"
            );
            assert_eq!(
                aurix_client_set_encoder_settings(client, ptr::null()),
                AurixResult::AurixNullPointer
            );
            aurix_client_destroy(client);
        }
    }

    #[test]
    fn bare_codec_encodes_decodes_and_clamps() {
        unsafe {
            assert!(aurix_opus_encoder_create(48_000, 3, ptr::null()).is_null());
            assert!(aurix_opus_decoder_create(44_100, 1).is_null());
            let mut settings = AurixEncoderSettings::from(EncoderSettings::default());
            settings.bitrate_bps = 24_000;
            settings.complexity = 4;
            settings.max_bandwidth = AurixOpusBandwidth::AurixBandwidthWideband;
            let enc = aurix_opus_encoder_create(48_000, 2, &settings);
            assert!(!enc.is_null());
            let mut got = settings;
            assert!(aurix_opus_encoder_settings(enc, &mut got));
            assert_eq!((got.bitrate_bps, got.complexity), (24_000, 4));

            // 20 ms stereo frame of a 440 Hz tone.
            let frame = 960usize;
            let pcm: Vec<f32> = (0..frame)
                .flat_map(|i| {
                    let v = (i as f32 * 440.0 * std::f32::consts::TAU / 48_000.0).sin() * 0.5;
                    [v, v]
                })
                .collect();
            let mut packet = vec![0u8; 1275];
            let len = aurix_opus_encoder_encode_f32(
                enc,
                pcm.as_ptr(),
                frame,
                packet.as_mut_ptr(),
                packet.len(),
            );
            assert!(len > 0, "encode failed: {len}");
            // Not a valid Opus frame size -> invalid argument, negative.
            let bad = aurix_opus_encoder_encode_f32(
                enc,
                pcm.as_ptr(),
                1000,
                packet.as_mut_ptr(),
                packet.len(),
            );
            assert_eq!(bad, -(AurixResult::AurixCodec as i32));

            let dec = aurix_opus_decoder_create(48_000, 2);
            assert!(!dec.is_null());
            let mut out = vec![0f32; frame * 2];
            let n = aurix_opus_decoder_decode_f32(
                dec,
                packet.as_ptr(),
                len as usize,
                out.as_mut_ptr(),
                frame,
                false,
            );
            assert_eq!(n as usize, frame);
            // PLC frame: same length, finite audio.
            let n =
                aurix_opus_decoder_decode_f32(dec, ptr::null(), 0, out.as_mut_ptr(), frame, false);
            assert_eq!(n as usize, frame);
            assert!(out.iter().all(|v| v.is_finite()));

            settings.bitrate_bps = 999_999;
            settings.complexity = 42;
            assert_eq!(
                aurix_opus_encoder_apply(enc, &settings),
                AurixResult::AurixOk
            );
            assert!(aurix_opus_encoder_settings(enc, &mut got));
            assert_eq!(
                (got.bitrate_bps, got.complexity),
                (EncoderSettings::MAX_BITRATE, 10)
            );
            assert_eq!(
                aurix_opus_encoder_apply(enc, ptr::null()),
                AurixResult::AurixNullPointer
            );

            aurix_opus_encoder_destroy(enc);
            aurix_opus_decoder_destroy(dec);
            aurix_opus_encoder_destroy(ptr::null_mut());
            aurix_opus_decoder_destroy(ptr::null_mut());
        }
    }

    #[test]
    fn loss_adaptation_and_decoder_settings_round_trip_through_the_abi() {
        unsafe {
            let mut cfg = std::mem::MaybeUninit::<AurixClientConfig>::uninit();
            aurix_client_config_default(cfg.as_mut_ptr());
            let mut cfg = cfg.assume_init();
            assert_eq!(
                cfg.decoder,
                AurixDecoderSettings::from(DecoderSettings::default())
            );
            assert_eq!(
                cfg.loss_adaptation,
                AurixLossAdaptation::AurixLossAdaptationAuto
            );
            let url = CString::new("http://not-a-websocket").unwrap();
            let token = CString::new("t").unwrap();
            cfg.ws_url = url.as_ptr();
            cfg.token = token.as_ptr();
            cfg.loss_adaptation = AurixLossAdaptation::AurixLossAdaptationFixedHigh;
            let client = aurix_client_create(&cfg);
            assert!(!client.is_null());
            assert_eq!(
                aurix_client_loss_adaptation(client),
                AurixLossAdaptation::AurixLossAdaptationFixedHigh
            );
            assert_eq!(
                aurix_client_loss_profile(client),
                AurixLossProfile::AurixLossProfileHigh
            );
            assert_eq!(
                aurix_client_set_loss_adaptation(
                    client,
                    AurixLossAdaptation::AurixLossAdaptationAuto
                ),
                AurixResult::AurixOk
            );
            assert_eq!(
                aurix_client_loss_profile(client),
                AurixLossProfile::AurixLossProfileLow
            );
            let ds = AurixDecoderSettings {
                complexity: 42,
                osce_bwe: true,
            };
            assert_eq!(
                aurix_client_set_decoder_settings(client, &ds),
                AurixResult::AurixOk
            );
            assert_eq!(
                aurix_client_set_decoder_settings(client, ptr::null()),
                AurixResult::AurixInvalidArgument
            );
            let mut back = AurixDecoderSettings::from(DecoderSettings::default());
            assert!(aurix_client_decoder_settings(client, &mut back));
            assert_eq!((back.complexity, back.osce_bwe), (10, true));
            assert!(!aurix_client_decoder_settings(ptr::null(), &mut back));
            assert_eq!(
                aurix_client_loss_profile(ptr::null()),
                AurixLossProfile::AurixLossProfileLow
            );
            aurix_client_destroy(client);
        }
    }

    #[test]
    fn bare_decoder_rebuilds_a_burst_from_dred_through_the_abi() {
        unsafe {
            assert!(aurix_dred_supported());
            let mut settings = AurixEncoderSettings::from(EncoderSettings::default());
            settings.bitrate_bps = 40_000;
            settings.complexity = 10;
            settings.fec = true;
            settings.expected_loss_percent = 30;
            settings.dtx = false;
            settings.dred_duration_ms = 400;
            let enc = aurix_opus_encoder_create(48_000, 1, &settings);
            assert!(!enc.is_null());
            let mut got = settings;
            assert!(aurix_opus_encoder_settings(enc, &mut got));
            assert_eq!(got.dred_duration_ms, 400);

            let frame = 960usize;
            let n = 50;
            let pcm = opus::testing::speech_like_f32(n);
            let mut packets = Vec::with_capacity(n);
            let mut buf = vec![0u8; 1275];
            for i in 0..n {
                let len = aurix_opus_encoder_encode_f32(
                    enc,
                    pcm[i * frame..].as_ptr(),
                    frame,
                    buf.as_mut_ptr(),
                    buf.len(),
                );
                assert!(len > 0, "encode {i}: {len}");
                packets.push(buf[..len as usize].to_vec());
            }

            let dec = aurix_opus_decoder_create(48_000, 1);
            assert!(!dec.is_null());
            let ds = AurixDecoderSettings {
                complexity: 6,
                osce_bwe: false,
            };
            assert_eq!(aurix_opus_decoder_apply(dec, &ds), AurixResult::AurixOk);
            assert_eq!(
                aurix_opus_decoder_apply(dec, ptr::null()),
                AurixResult::AurixNullPointer
            );
            let mut back = AurixDecoderSettings::from(DecoderSettings::default());
            assert!(aurix_opus_decoder_settings(dec, &mut back));
            assert_eq!(back.complexity, 6);
            let mut out = vec![0f32; frame];
            for p in &packets[..30] {
                let n = aurix_opus_decoder_decode_f32(
                    dec,
                    p.as_ptr(),
                    p.len(),
                    out.as_mut_ptr(),
                    frame,
                    false,
                );
                assert_eq!(n as usize, frame);
            }
            // Frames 30..33 lost: 30..32 from packet 34's DRED, 33 from its FEC (or PLC).
            let later = &packets[34];
            let rms = |v: &[f32]| (v.iter().map(|x| x * x).sum::<f32>() / v.len() as f32).sqrt();
            let mut rebuilt = Vec::new();
            for back in (2..=4u32).rev() {
                let n = aurix_opus_decoder_dred_decode_f32(
                    dec,
                    later.as_ptr(),
                    later.len(),
                    back,
                    out.as_mut_ptr(),
                    frame,
                );
                assert_eq!(n as usize, frame, "{back} frames back");
                rebuilt.extend_from_slice(&out);
            }
            let reference = rms(&pcm[30 * frame..33 * frame]);
            let got = rms(&rebuilt);
            assert!(
                got > reference * 0.3 && got < reference * 3.0,
                "DRED rms {got} vs {reference}"
            );
            let mut out16 = vec![0i16; frame];
            assert_eq!(
                aurix_opus_decoder_dred_decode_i16(
                    dec,
                    later.as_ptr(),
                    later.len(),
                    2,
                    out16.as_mut_ptr(),
                    frame,
                ) as usize,
                frame
            );
            assert!(out16.iter().any(|&v| v != 0));
            let fec = aurix_opus_packet_has_fec(later.as_ptr(), later.len());
            let n = aurix_opus_decoder_decode_f32(
                dec,
                later.as_ptr(),
                later.len(),
                out.as_mut_ptr(),
                frame,
                fec,
            );
            assert_eq!(n as usize, frame);
            // Beyond the packet's history → 0; bad arguments → negative codes.
            assert_eq!(
                aurix_opus_decoder_dred_decode_f32(
                    dec,
                    later.as_ptr(),
                    later.len(),
                    60,
                    out.as_mut_ptr(),
                    frame,
                ),
                0
            );
            assert_eq!(
                aurix_opus_decoder_dred_decode_f32(
                    dec,
                    later.as_ptr(),
                    later.len(),
                    0,
                    out.as_mut_ptr(),
                    frame,
                ),
                -(AurixResult::AurixInvalidArgument as i32)
            );
            assert_eq!(
                aurix_opus_decoder_dred_decode_f32(dec, ptr::null(), 0, 2, out.as_mut_ptr(), frame),
                -(AurixResult::AurixNullPointer as i32)
            );
            assert!(!aurix_opus_packet_has_fec(ptr::null(), 0));
            aurix_opus_encoder_destroy(enc);
            aurix_opus_decoder_destroy(dec);
        }
    }

    #[test]
    fn uuid_round_trip_and_name_truncation() {
        unsafe {
            let u = Uuid::new_v4();
            let text = CString::new(u.to_string()).unwrap();
            let mut parsed = zero();
            assert_eq!(
                aurix_uuid_parse(text.as_ptr(), &mut parsed),
                AurixResult::AurixOk
            );
            assert_eq!(Uuid::from(parsed), u);
            let mut buf = [0 as c_char; AURIX_UUID_STRING_LEN];
            assert_eq!(
                aurix_uuid_format(&parsed, buf.as_mut_ptr(), buf.len()),
                AurixResult::AurixOk
            );
            assert_eq!(
                CStr::from_ptr(buf.as_ptr()).to_str().unwrap(),
                u.to_string()
            );
            assert_eq!(
                aurix_uuid_format(&parsed, buf.as_mut_ptr(), 10),
                AurixResult::AurixInvalidArgument
            );
        }
        let long = "é".repeat(200);
        let buf = name_buf(&long);
        let s = unsafe { CStr::from_ptr(buf.as_ptr()) }.to_str().unwrap();
        assert!(s.len() < AURIX_NAME_LEN && s.chars().all(|c| c == 'é'));
    }

    #[test]
    fn chat_event_exposes_owned_strings() {
        let msg = ChatMessage {
            id: Uuid::new_v4(),
            channel_id: Some(ChannelId::new()),
            from_user_id: UserId::new(),
            display_name: "Alice".into(),
            to_user_id: None,
            text: "hi\0there".into(),
            metadata: Some(serde_json::json!({"k": 1})),
            client_ref: None,
            sent_at: chrono::Utc::now(),
            offline: true,
            edited_at: None,
            deleted_at: None,
            deleted_by: None,
            reactions: Vec::new(),
        };
        let ev = Box::into_raw(Box::new(AurixEvent::new(Event::ChatMessage {
            request_id: Some(7),
            message: msg,
        })));
        unsafe {
            assert_eq!(aurix_event_type(ev), AurixEventType::AurixEventChatMessage);
            assert_eq!(aurix_event_request_id(ev), 7);
            let mut chat = std::mem::MaybeUninit::<AurixChatMessage>::uninit();
            assert!(aurix_event_chat(ev, chat.as_mut_ptr()));
            let chat = chat.assume_init();
            assert_eq!(CStr::from_ptr(chat.text).to_str().unwrap(), "hi there");
            assert_eq!(CStr::from_ptr(chat.sender_name).to_str().unwrap(), "Alice");
            assert_eq!(
                CStr::from_ptr(chat.metadata_json).to_str().unwrap(),
                "{\"k\":1}"
            );
            assert!(chat.offline);
            assert_eq!(chat.edited_at_ms, 0);
            assert_eq!(chat.deleted_at_ms, 0);
            assert!(chat.reactions_json.is_null());
            let cursor = CStr::from_ptr(chat.cursor).to_str().unwrap();
            let (sent_at, id) = aurix_common::protocol::decode_chat_cursor(cursor).unwrap();
            assert_eq!(id.as_bytes(), &chat.message_id.bytes);
            assert_eq!(sent_at.timestamp_millis(), chat.sent_at_ms);
            let json = CStr::from_ptr(aurix_event_json(ev)).to_str().unwrap();
            assert!(json.contains("\"type\":\"chat_message\""), "{json}");
            assert!(!aurix_event_transcript(ev, ptr::null_mut()));
            aurix_event_free(ev);
        }
    }

    #[test]
    fn chat_update_search_and_reaction_events_map() {
        let deleted_by = UserId::new();
        let tombstone = ChatMessage {
            id: Uuid::new_v4(),
            channel_id: None,
            from_user_id: UserId::new(),
            display_name: "Alice".into(),
            to_user_id: Some(deleted_by),
            text: String::new(),
            metadata: None,
            client_ref: Some("u9-1".into()),
            sent_at: chrono::Utc::now(),
            offline: false,
            edited_at: Some(chrono::Utc::now()),
            deleted_at: Some(chrono::Utc::now()),
            deleted_by: Some(deleted_by),
            reactions: Vec::new(),
        };
        let ev = Box::into_raw(Box::new(AurixEvent::new(Event::ChatMessageUpdated {
            request_id: Some(9),
            message: tombstone.clone(),
        })));
        unsafe {
            assert_eq!(
                aurix_event_type(ev),
                AurixEventType::AurixEventChatMessageUpdated
            );
            assert_eq!(aurix_event_request_id(ev), 9);
            assert_eq!(aurix_event_object_id(ev).bytes, *tombstone.id.as_bytes());
            let mut chat = std::mem::MaybeUninit::<AurixChatMessage>::uninit();
            assert!(aurix_event_chat(ev, chat.as_mut_ptr()));
            let chat = chat.assume_init();
            assert_eq!(CStr::from_ptr(chat.text).to_str().unwrap(), "");
            assert!(chat.edited_at_ms > 0 && chat.deleted_at_ms > 0);
            assert_eq!(chat.deleted_by.bytes, *deleted_by.0.as_bytes());
            assert!(chat.metadata_json.is_null());
            aurix_event_free(ev);
        }

        let reacted = ChatMessage {
            text: "gg".into(),
            edited_at: None,
            deleted_at: None,
            deleted_by: None,
            client_ref: None,
            reactions: vec![aurix_common::protocol::ChatReaction {
                reaction: "👍".into(),
                count: 2,
                user_ids: vec![deleted_by],
            }],
            ..tombstone
        };
        let ev = Box::into_raw(Box::new(AurixEvent::new(Event::ChatSearchResult {
            request_id: Some(4),
            scope: None,
            query: "gg".into(),
            messages: vec![reacted.clone()],
            next_before: Some("cursor".into()),
        })));
        unsafe {
            assert_eq!(
                aurix_event_type(ev),
                AurixEventType::AurixEventChatSearchResult
            );
            assert_eq!(aurix_event_request_id(ev), 4);
            assert_eq!(aurix_event_channel_id(ev).bytes, [0; 16]);
            assert_eq!(aurix_event_user_id(ev).bytes, [0; 16]);
            assert_eq!(
                CStr::from_ptr(aurix_event_message(ev)).to_str().unwrap(),
                "gg"
            );
            let mut page = std::mem::MaybeUninit::<AurixChatHistory>::uninit();
            assert!(aurix_event_chat_history(ev, page.as_mut_ptr()));
            let page = page.assume_init();
            assert_eq!(page.count, 1);
            assert_eq!(CStr::from_ptr(page.next_before).to_str().unwrap(), "cursor");
            assert!(page.next_after.is_null());
            let mut chat = std::mem::MaybeUninit::<AurixChatMessage>::uninit();
            assert!(aurix_event_chat_history_message(ev, 0, chat.as_mut_ptr()));
            let chat = chat.assume_init();
            assert_eq!(CStr::from_ptr(chat.text).to_str().unwrap(), "gg");
            let reactions: Vec<aurix_common::protocol::ChatReaction> =
                serde_json::from_str(CStr::from_ptr(chat.reactions_json).to_str().unwrap())
                    .unwrap();
            assert_eq!(reactions, reacted.reactions);
            let mut sink = std::mem::MaybeUninit::<AurixChatMessage>::uninit();
            assert!(!aurix_event_chat_history_message(ev, 1, sink.as_mut_ptr()));
            aurix_event_free(ev);
        }

        let channel_id = ChannelId::new();
        let ev = Box::into_raw(Box::new(AurixEvent::new(Event::ChatReactionChanged {
            message_id: reacted.id,
            channel_id: Some(channel_id),
            message_from_user_id: reacted.from_user_id,
            message_to_user_id: None,
            user_id: deleted_by,
            reaction: "👍".into(),
            added: true,
            count: 3,
            timestamp: chrono::Utc::now(),
        })));
        unsafe {
            assert_eq!(
                aurix_event_type(ev),
                AurixEventType::AurixEventChatReactionChanged
            );
            assert!(aurix_event_flag(ev));
            assert_eq!(aurix_event_number(ev), 3);
            assert_eq!(aurix_event_channel_id(ev).bytes, *channel_id.0.as_bytes());
            assert_eq!(aurix_event_user_id(ev).bytes, *deleted_by.0.as_bytes());
            assert_eq!(aurix_event_object_id(ev).bytes, *reacted.id.as_bytes());
            let mut r = std::mem::MaybeUninit::<AurixChatReaction>::uninit();
            assert!(aurix_event_reaction(ev, r.as_mut_ptr()));
            let r = r.assume_init();
            assert_eq!(CStr::from_ptr(r.reaction).to_str().unwrap(), "👍");
            assert!(r.added);
            assert_eq!(r.count, 3);
            assert_eq!(r.message_recipient_id.bytes, [0; 16]);
            let mut sink = std::mem::MaybeUninit::<AurixChatMessage>::uninit();
            assert!(!aurix_event_chat(ev, sink.as_mut_ptr()));
            aurix_event_free(ev);
        }
    }

    #[test]
    fn channel_scope_event_maps_none_to_zero() {
        let channel_id = ChannelId::new();
        let scoped = Box::into_raw(Box::new(AurixEvent::new(Event::ChannelJoined {
            request_id: 3,
            channel_id,
            participants: Vec::new(),
            transcription: false,
            safety_voice: false,
            scope: ChannelScope {
                roster_radius: Some(25.0),
                text_radius: None,
            },
            role: ChannelRole::Listener,
            participant_count: 1200,
            hidden_listeners: true,
            ducking: None,
            priority: false,
            waiting_to_speak: false,
        })));
        let other = Box::into_raw(Box::new(AurixEvent::new(Event::ChannelLeft { channel_id })));
        unsafe {
            let info = aurix_event_channel_info(scoped);
            assert_eq!(info.role, AurixRole::AurixRoleListener);
            assert_eq!(info.participant_count, 1200);
            assert!(info.hidden_listeners);
            assert_eq!(aurix_event_channel_info(other), AurixChannelInfo::default());
            let scope = aurix_event_channel_scope(scoped);
            assert_eq!(scope.roster_radius, 25.0);
            assert_eq!(scope.text_radius, 0.0, "unscoped text = whole channel");
            let none = aurix_event_channel_scope(other);
            assert_eq!((none.roster_radius, none.text_radius), (0.0, 0.0));
            aurix_event_free(scoped);
            aurix_event_free(other);
            let raw = AurixUuid::from(channel_id.0);
            let mut out = AurixChannelScope::default();
            assert!(!aurix_client_channel_scope(ptr::null(), &raw, &mut out));
            assert!(!aurix_client_channel_scope(
                ptr::null(),
                &raw,
                ptr::null_mut()
            ));
        }
    }

    #[test]
    fn regions_parse_probe_and_rank() {
        let body = CString::new(format!(
            r#"{{"regions":[
                {{"region":"eu_west","node_id":"{}","ws_url":"wss://eu1.example/ws","probe_url":"https://eu1.example/health","location":{{"latitude":48.8,"longitude":2.3}},"distance_km":12.5,"nodes":2,"load_factor":0.25}},
                {{"region":"us_east","node_id":"{}","ws_url":"wss://us1.example/ws","probe_url":"https://us1.example/health","location":null,"distance_km":null,"nodes":1,"load_factor":0.5}},
                {{"region":"africa","node_id":"{}","ws_url":"wss://af1.example/ws","probe_url":null,"location":null,"distance_km":null,"nodes":1,"load_factor":0.0}}],
                "recommended":null}}"#,
            Uuid::new_v4(),
            Uuid::new_v4(),
            Uuid::new_v4()
        ))
        .unwrap();
        let region_of = |ep: &AurixRegionEndpoint| unsafe {
            CStr::from_ptr(ep.region.as_ptr())
                .to_str()
                .unwrap()
                .to_string()
        };
        unsafe {
            assert!(aurix_regions_parse(ptr::null()).is_null());
            let bad = CString::new("{}").unwrap();
            assert!(aurix_regions_parse(bad.as_ptr()).is_null());
            assert!(!CStr::from_ptr(aurix_last_error()).to_bytes().is_empty());

            let list = aurix_regions_parse(body.as_ptr());
            assert!(!list.is_null());
            assert_eq!(aurix_regions_len(list), 3);
            let mut ep = std::mem::MaybeUninit::<AurixRegionEndpoint>::uninit();
            assert!(aurix_regions_get(list, 0, ep.as_mut_ptr()));
            let ep0 = ep.assume_init();
            assert_eq!(region_of(&ep0), "eu_west");
            assert_eq!(
                CStr::from_ptr(ep0.ws_url.as_ptr()).to_str().unwrap(),
                "wss://eu1.example/ws"
            );
            assert!(ep0.has_location && ep0.latitude == 48.8);
            assert!(ep0.has_distance && ep0.distance_km == 12.5);
            assert_eq!(ep0.nodes, 2);
            assert!(!ep0.has_rtt && !ep0.probe_failed);
            assert!(!aurix_regions_get(list, 3, ep.as_mut_ptr()));

            assert_eq!(aurix_regions_set_rtt(list, 0, 80.0), AurixResult::AurixOk);
            assert_eq!(aurix_regions_set_rtt(list, 1, 20.0), AurixResult::AurixOk);
            assert_eq!(
                aurix_regions_set_rtt(list, 9, 1.0),
                AurixResult::AurixInvalidArgument
            );
            assert_eq!(
                aurix_regions_rank(list, ptr::null(), 0.0),
                AurixResult::AurixOk
            );
            let order = |list: *const AurixRegionList| -> Vec<String> {
                (0..aurix_regions_len(list))
                    .map(|i| {
                        let mut ep = std::mem::MaybeUninit::<AurixRegionEndpoint>::uninit();
                        assert!(aurix_regions_get(list, i, ep.as_mut_ptr()));
                        region_of(&ep.assume_init())
                    })
                    .collect()
            };
            assert_eq!(order(list), ["us_east", "eu_west", "africa"]);

            let pref = CString::new("eu-west").unwrap();
            assert_eq!(
                aurix_regions_rank(list, pref.as_ptr(), 0.0),
                AurixResult::AurixOk
            );
            assert_eq!(order(list), ["eu_west", "us_east", "africa"]);
            let unknown = CString::new("mars").unwrap();
            assert_eq!(
                aurix_regions_rank(list, unknown.as_ptr(), 0.0),
                AurixResult::AurixInvalidArgument
            );

            // The preferred region loses its bonus once its probe fails; unreachable ranks last.
            assert_eq!(aurix_regions_set_rtt(list, 0, -1.0), AurixResult::AurixOk);
            assert_eq!(
                aurix_regions_rank(list, pref.as_ptr(), 0.0),
                AurixResult::AurixOk
            );
            assert_eq!(order(list), ["us_east", "africa", "eu_west"]);
            assert!(aurix_regions_get(list, 2, ep.as_mut_ptr()));
            assert!(ep.assume_init().probe_failed);
            aurix_regions_free(list);
            aurix_regions_free(ptr::null_mut());

            let api = CString::new("https://api.example/").unwrap();
            let pref = CString::new("us_east").unwrap();
            let mut buf = [0 as c_char; 128];
            let n = aurix_regions_discovery_url(
                api.as_ptr(),
                pref.as_ptr(),
                true,
                1.5,
                -2.0,
                buf.as_mut_ptr(),
                buf.len(),
            );
            let url = CStr::from_ptr(buf.as_ptr()).to_str().unwrap();
            assert_eq!(
                url,
                "https://api.example/v1/me/regions?region=us_east&latitude=1.5&longitude=-2"
            );
            assert_eq!(n, url.len());
            assert_eq!(
                aurix_regions_discovery_url(
                    api.as_ptr(),
                    unknown.as_ptr(),
                    false,
                    0.0,
                    0.0,
                    ptr::null_mut(),
                    0
                ),
                0
            );
        }
    }

    #[test]
    fn standalone_effects_and_visemes_round_trip() {
        unsafe {
            // Bypass passes audio through untouched.
            let fx = aurix_voice_effects_create(ptr::null());
            assert!(!fx.is_null());
            assert!(aurix_voice_effects_is_bypass(fx));
            let mut pcm: Vec<f32> = (0..960)
                .map(|i| (i as f32 * 220.0 * std::f32::consts::TAU / 48_000.0).sin() * 0.5)
                .collect();
            let original = pcm.clone();
            assert_eq!(
                aurix_voice_effects_process_f32(fx, pcm.as_mut_ptr(), pcm.len(), 1),
                AurixResult::AurixOk
            );
            assert_eq!(pcm, original);

            // Bad geometry is rejected before touching the buffer.
            assert_eq!(
                aurix_voice_effects_process_f32(fx, pcm.as_mut_ptr(), 0, 1),
                AurixResult::AurixInvalidArgument
            );
            assert_eq!(
                aurix_voice_effects_process_f32(fx, pcm.as_mut_ptr(), 3, 2),
                AurixResult::AurixInvalidArgument
            );
            assert_eq!(
                aurix_voice_effects_process_f32(fx, pcm.as_mut_ptr(), pcm.len(), 3),
                AurixResult::AurixInvalidArgument
            );

            // A preset changes the signal and reads back clamped.
            let mut robot = aurix_voice_effects_preset(AurixVoicePreset::AurixVoicePresetRobot);
            robot.pitch_semitones = 99.0;
            assert_eq!(aurix_voice_effects_set(fx, &robot), AurixResult::AurixOk);
            assert!(!aurix_voice_effects_is_bypass(fx));
            let mut back = AurixVoiceEffects::default();
            assert_eq!(aurix_voice_effects_get(fx, &mut back), AurixResult::AurixOk);
            assert_eq!(back.ring_mod_hz, robot.ring_mod_hz);
            assert_eq!(back.pitch_semitones, MAX_PITCH_SEMITONES);
            assert_eq!(
                aurix_voice_effects_process_f32(fx, pcm.as_mut_ptr(), pcm.len(), 1),
                AurixResult::AurixOk
            );
            assert_ne!(pcm, original);
            assert!(pcm.iter().all(|s| s.abs() <= 1.0));
            aurix_voice_effects_reset(fx);
            aurix_voice_effects_destroy(fx);
            aurix_voice_effects_destroy(ptr::null_mut());

            // Standalone lip-sync analyser: silence, then a loud vowel-like tone.
            let an = aurix_viseme_analyzer_create();
            assert!(!an.is_null());
            let mut frame = AurixVisemeFrame {
                weights: [0.0; AURIX_VISEME_COUNT],
                dominant: AurixViseme::AurixVisemeAA,
                mouth_open: 1.0,
                energy: 1.0,
                confidence: 0.0,
                sequence: 7,
            };
            assert_eq!(
                aurix_viseme_analyzer_frame(an, &mut frame),
                AurixResult::AurixOk
            );
            assert_eq!(frame.dominant, AurixViseme::AurixVisemeSilence);
            assert_eq!(frame.sequence, 0);
            let silence = vec![0.0f32; 960];
            assert_eq!(
                aurix_viseme_analyzer_push_f32(an, silence.as_ptr(), silence.len(), 1),
                AurixResult::AurixOk
            );
            assert_eq!(
                aurix_viseme_analyzer_push_f32(an, silence.as_ptr(), 0, 1),
                AurixResult::AurixInvalidArgument
            );
            assert_eq!(
                aurix_viseme_analyzer_push_f32(an, silence.as_ptr(), silence.len(), 0),
                AurixResult::AurixInvalidArgument
            );
            let voiced: Vec<f32> = (0..1920)
                .flat_map(|i| {
                    let t = i as f32 / 48_000.0;
                    let v = (t * 150.0 * std::f32::consts::TAU).sin() * 0.2
                        + (t * 750.0 * std::f32::consts::TAU).sin() * 0.2
                        + (t * 1200.0 * std::f32::consts::TAU).sin() * 0.1;
                    [v, v]
                })
                .collect();
            for _ in 0..10 {
                assert_eq!(
                    aurix_viseme_analyzer_push_f32(an, voiced.as_ptr(), voiced.len(), 2),
                    AurixResult::AurixOk
                );
            }
            assert_eq!(
                aurix_viseme_analyzer_frame(an, &mut frame),
                AurixResult::AurixOk
            );
            assert_eq!(frame.sequence, 11);
            assert_ne!(frame.dominant, AurixViseme::AurixVisemeSilence);
            assert!(frame.mouth_open > 0.0 && frame.energy > 0.0);
            aurix_viseme_analyzer_reset(an);
            assert_eq!(
                aurix_viseme_analyzer_frame(an, &mut frame),
                AurixResult::AurixOk
            );
            assert_eq!(frame.dominant, AurixViseme::AurixVisemeSilence);
            assert_eq!(frame.sequence, 11);
            aurix_viseme_analyzer_destroy(an);
            aurix_viseme_analyzer_destroy(ptr::null_mut());
        }
    }
}
