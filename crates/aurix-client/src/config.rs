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
    /// Uplink Opus bitrate.
    pub bitrate_bps: u32,
    /// Frames buffered before a sender's playout starts (2 ≈ 40 ms).
    pub jitter_target_frames: usize,
    /// Hard cap of buffered frames per sender.
    pub jitter_max_frames: usize,
    /// Do not send frames the VAD classifies as silence (saves uplink; the server still learns
    /// the speaking state from the level byte of the frames that are sent).
    pub vad_gate: bool,
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
            bitrate_bps: 32_000,
            jitter_target_frames: 2,
            jitter_max_frames: 12,
            vad_gate: false,
            worker_threads: 1,
        }
    }
}
