//! Aurix native voice client core.
//!
//! Layers, bottom up:
//! * [`audio`] — Opus capture encoder with VAD/level metadata, per-sender jitter buffers and a
//!   stereo mixer (`RemoteMixer`), pure CPU code with no I/O;
//! * [`dsp`] — capture DSP: high-pass, acoustic echo cancellation, RNNoise-based noise
//!   suppression and AGC, pure Rust;
//! * [`media`] — AURX v2 over UDP or tunnelled through the control WebSocket when UDP is
//!   blocked: authenticated `SessionBind`, AES-256-CTR + HMAC per packet, replay windows,
//!   heartbeats, quality reports;
//! * [`control`] — the WebSocket control plane (session open/resume, channels, chat,
//!   moderation, transcripts, TTS) as typed `ControlMessage`s;
//! * [`client`] — `Client`: one voice session that ties the layers together, reconnects with
//!   resume, re-joins channels and exposes a poll-based [`events::Event`] queue;
//! * [`regions`] — region discovery: parse `GET /v1/me/regions`, rank by measured RTT (the host
//!   does the HTTP);
//! * [`ffi`] — the stable C ABI (`aurix_*`), generated into `include/aurix_client.h` by
//!   cbindgen.

pub mod audio;
pub mod client;

pub mod config;
pub mod control;
pub mod dsp;

pub mod error;
pub mod events;
pub mod ffi;

pub mod media;
pub mod regions;

pub use audio::EncoderSettings;
pub use aurix_common::protocol::{
    ChatMessage, ParticipantEnergy, Transcript, TransmissionMode, TtsDestination, TtsState,
    UserPosition,
};
pub use aurix_common::types::{
    ActionKind, AudioPolicy, ChannelId, ChannelRole, Direction, OpusBandwidth, OpusSignal,
    Orientation3D, Position3D, SessionId, UserId,
};

pub use client::{Client, ClientStats, TransmitStats};
pub use config::{ClientConfig, ReconnectPolicy};
pub use dsp::{DspConfig, DspStats, NoiseSuppression};
pub use error::{ClientError, Result};
pub use events::{ChannelScope, ConnectionState, Event, Participant, RequestId, SessionInfo};
pub use media::{IncomingAudio, MediaPath, MediaPathPolicy, MediaStats};
pub use regions::{GeoLocation, ProbedRegion, Region, RegionEndpoint, RegionsResponse};

/// Top bit of a synthesized (TTS / announcement) stream SSRC; session SSRCs never have it.
pub const SYNTH_SSRC_FLAG: u32 = 0x8000_0000;

/// `true` for server-synthesized audio (TTS, announcements) rather than a participant microphone.
pub fn is_synthesized_ssrc(ssrc: u32) -> bool {
    ssrc & SYNTH_SSRC_FLAG != 0
}

/// Session SSRC of the participant whose TTS voice plays on `ssrc` (identity for microphones).
pub fn source_ssrc(ssrc: u32) -> u32 {
    ssrc & !SYNTH_SSRC_FLAG
}

/// Library version, also exported via `aurix_version()`.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
