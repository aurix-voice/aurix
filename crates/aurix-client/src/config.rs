use crate::audio::{DecoderSettings, EncoderSettings};
use crate::dsp::DspConfig;
use crate::media::MediaPathPolicy;
use crate::resilience::{LossAdaptation, LossProfilePolicy};
use std::time::Duration;

/// Exponential backoff for automatic reconnects.
#[derive(Debug, Clone, PartialEq)]
pub struct ReconnectPolicy {
    pub initial_delay: Duration,
    pub max_delay: Duration,
    pub factor: f64,
    /// ±fraction of the computed delay.
    pub jitter: f64,
    pub max_attempts: u32,
}

impl Default for ReconnectPolicy {
    fn default() -> Self {
        Self {
            initial_delay: Duration::from_millis(500),
            max_delay: Duration::from_secs(15),
            factor: 2.0,
            jitter: 0.2,
            max_attempts: 10,
        }
    }
}

impl ReconnectPolicy {
    pub fn delay(&self, attempt: u32) -> Duration {
        let ms = self.initial_delay.as_millis() as f64
            * self.factor.powi(attempt.saturating_sub(1) as i32);
        let ms = ms.min(self.max_delay.as_millis() as f64);
        let jitter = (rand::random::<f64>() * 2.0 - 1.0) * self.jitter.clamp(0.0, 1.0);
        Duration::from_millis((ms * (1.0 + jitter)).max(0.0) as u64)
    }
}

/// Everything a native client needs to open a session.
#[derive(Debug, Clone, PartialEq)]
pub struct ClientConfig {
    /// `ws://host:8081/ws` or `wss://…`.
    pub ws_url: String,
    /// Session JWT (or a one-time `login` action token).
    pub token: String,
    pub auto_reconnect: bool,
    pub reconnect: ReconnectPolicy,
    /// Per-request timeout for joins, moderation and chat acks.
    pub request_timeout: Duration,
    /// Control-plane liveness ping.
    pub ping_interval: Duration,
    /// Media heartbeat (NAT keepalive + RTT probe).
    pub heartbeat_interval: Duration,
    /// Which link carries media: UDP first with the WebSocket tunnel as fallback (default),
    /// UDP only, or tunnel only.
    pub media_path: MediaPathPolicy,
    /// `Auto` only: unanswered heartbeats in a row on UDP before media moves to the tunnel
    /// (with the default 5 s heartbeat, 3 ≈ 15 s of silence).
    pub udp_fallback_lost_heartbeats: u32,
    /// `Auto` only: how often a tunnelled session re-probes UDP and moves back when it
    /// answers; zero disables re-probing (the session stays tunnelled until it reconnects).
    pub udp_reprobe_interval: Duration,
    /// Uplink Opus encoder before any channel policy applies (bitrate, complexity, bandwidth,
    /// VBR/FEC/DTX).
    pub encoder: EncoderSettings,
    /// Downlink Opus decoders: complexity (neural PLC from 5, OSCE speech enhancement from
    /// 6) and OSCE bandwidth extension. Changeable at runtime with
    /// `Client::set_decoder_settings`.
    pub decoder: DecoderSettings,
    /// How the uplink's redundancy (FEC tuning, DRED) follows the loss the server measures:
    /// automatically by [`LossProfilePolicy`] thresholds, or pinned to one profile.
    pub loss_adaptation: LossAdaptation,
    pub loss_profile_policy: LossProfilePolicy,
    /// Capture DSP (high-pass, echo cancellation, noise suppression, AGC) applied before the
    /// VAD and encoder; everything on by default, `DspConfig::BYPASS` for hosts with their own
    /// processing. Changeable at runtime with `Client::set_dsp`.
    pub dsp: DspConfig,
    /// Adopt each joined channel's `AudioPolicy` (bitrate, FEC/DTX, bandwidth, signal and the
    /// complexity hint unless pinned with `Client::set_complexity`). Off: the policy is only
    /// reported through `Event::AudioPolicyChanged`; server bitrate commands still apply.
    pub follow_channel_policy: bool,
    /// Frames buffered before a sender's playout starts (2 ≈ 40 ms).
    pub jitter_target_frames: usize,
    /// Hard cap of buffered frames per sender.
    pub jitter_max_frames: usize,
    /// Do not send frames the VAD classifies as silence (saves uplink; the server still learns
    /// the speaking state from the level byte of the frames that are sent).
    pub vad_gate: bool,
    /// Announce end-to-end encryption support and take part in the sender-key exchange of
    /// `e2ee` channels (`aurix_common::e2ee`). Off, joining such a channel is refused by the
    /// server (`E2EE_REQUIRED`).
    pub e2ee: bool,
    /// X25519 identity secret (32 bytes) shown to peers as [`crate::Client::e2ee_fingerprint`];
    /// `None` generates a fresh one per client. Persist it to keep a stable fingerprint across
    /// runs (peers see a `KeyChanged` otherwise).
    pub e2ee_identity: Option<[u8; 32]>,
    /// Number of tokio worker threads for the control plane (1 is plenty).
    pub worker_threads: usize,
}

impl ClientConfig {
    pub fn new(ws_url: impl Into<String>, token: impl Into<String>) -> Self {
        Self {
            ws_url: ws_url.into(),
            token: token.into(),
            auto_reconnect: true,
            reconnect: ReconnectPolicy::default(),
            request_timeout: Duration::from_secs(10),
            ping_interval: Duration::from_secs(15),
            heartbeat_interval: Duration::from_secs(5),
            media_path: MediaPathPolicy::Auto,
            udp_fallback_lost_heartbeats: 3,
            udp_reprobe_interval: Duration::from_secs(30),
            encoder: EncoderSettings::default(),
            decoder: DecoderSettings::default(),
            loss_adaptation: LossAdaptation::Auto,
            loss_profile_policy: LossProfilePolicy::default(),
            dsp: DspConfig::default(),
            follow_channel_policy: true,
            jitter_target_frames: 2,
            jitter_max_frames: 12,
            vad_gate: false,
            e2ee: true,
            e2ee_identity: None,
            worker_threads: 1,
        }
    }
}
