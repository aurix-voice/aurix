use aurix_common::crypto::MediaKeys;
use aurix_common::protocol::ReplayWindow;
use aurix_common::types::*;
use chrono::{DateTime, Utc};
use parking_lot::{Mutex, RwLock};
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;

/// How a participant's media reaches the SFU.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Transport {
    /// Native AURX/UDP client (authenticated with the per-session media key).
    Aurx,
    /// Browser/WebRTC client (media arrives via str0m, downlink is a mixed track).
    WebRtc,
}

#[derive(Debug)]
pub struct MediaSession {
    pub session_id: SessionId,
    pub user_id: UserId,
    pub app_id: AppId,
    pub display_name: String,
    pub ssrc: u32,
    /// Per-session master media key handed to the client over the authenticated control channel.
    pub media_key: [u8; 32],
    /// Authentication/encryption keys derived from `media_key` (see `MediaKeys`).
    pub keys: MediaKeys,
    pub transport: RwLock<Transport>,
    pub remote_addr: RwLock<Option<SocketAddr>>,
    pub channels: RwLock<Vec<ChannelId>>,
    pub is_muted: AtomicBool,
    pub is_server_muted: AtomicBool,
    pub is_speaking: AtomicBool,
    pub sequence: AtomicU32,
    /// Sequence counter for server-originated packets addressed to this session
    /// (acks, commands); keeps their encryption IVs unique under the session key.
    pub downlink_sequence: AtomicU32,
    pub last_audio_timestamp: AtomicU64,
    /// Wall-clock ms of the last audio packet, used for the speaking timeout.
    pub last_audio_at_ms: AtomicI64,
    pub last_heartbeat: RwLock<DateTime<Utc>>,
    pub quality: RwLock<QualityMetrics>,
    pub created_at: DateTime<Utc>,
    pub replay: Mutex<ReplayWindow>,
    /// Highest `SessionBind` timestamp accepted so far (rejects replayed binds).
    pub last_bind_ms: AtomicI64,
    /// Browser RTP SSRC (WebRTC transport only), learned from the first RTP packet.
    pub webrtc_ssrc: AtomicU32,
    active: AtomicBool,
    packets_sent: AtomicU64,
    packets_received: AtomicU64,
    bytes_sent: AtomicU64,
    bytes_received: AtomicU64,
}

impl MediaSession {
    pub fn new(
        session_id: SessionId,
        user_id: UserId,
        app_id: AppId,
        display_name: String,
        ssrc: u32,
        media_key: [u8; 32],
    ) -> Arc<Self> {
        Arc::new(Self {
            session_id,
            user_id,
            app_id,
            display_name,
            ssrc,
            media_key,
            keys: MediaKeys::derive(&media_key),
            transport: RwLock::new(Transport::Aurx),
            remote_addr: RwLock::new(None),
            channels: RwLock::new(Vec::new()),
            is_muted: AtomicBool::new(false),
            is_server_muted: AtomicBool::new(false),
            is_speaking: AtomicBool::new(false),
            sequence: AtomicU32::new(0),
            downlink_sequence: AtomicU32::new(0),
            last_audio_timestamp: AtomicU64::new(0),
            last_audio_at_ms: AtomicI64::new(0),
            last_heartbeat: RwLock::new(Utc::now()),
            quality: RwLock::new(QualityMetrics {
                rtt_ms: 0.0,
                jitter_ms: 0.0,
                packet_loss_percent: 0.0,
                bitrate_kbps: 0,
                mos_score: 4.5,
            }),
            created_at: Utc::now(),
            replay: Mutex::new(ReplayWindow::default()),
            last_bind_ms: AtomicI64::new(i64::MIN),
            webrtc_ssrc: AtomicU32::new(0),
            active: AtomicBool::new(true),
            packets_sent: AtomicU64::new(0),
            packets_received: AtomicU64::new(0),
            bytes_sent: AtomicU64::new(0),
            bytes_received: AtomicU64::new(0),
        })
    }

    pub fn is_active(&self) -> bool {
        self.active.load(Ordering::Relaxed)
    }

    pub fn next_downlink_sequence(&self) -> u32 {
        self.downlink_sequence.fetch_add(1, Ordering::Relaxed)
    }

    pub fn deactivate(&self) {
        self.active.store(false, Ordering::Relaxed);
    }

    pub fn transport(&self) -> Transport {
        *self.transport.read()
    }

    pub fn set_transport(&self, t: Transport) {
        *self.transport.write() = t;
    }

    /// True once the UDP source address has been authenticated via `SessionBind`
    /// (or the WebRTC ICE session connected).
    pub fn is_bound(&self) -> bool {
        self.remote_addr.read().is_some()
    }

    pub fn set_remote_addr(&self, addr: SocketAddr) {
        *self.remote_addr.write() = Some(addr);
    }

    pub fn clear_remote_addr(&self) -> Option<SocketAddr> {
        self.remote_addr.write().take()
    }

    pub fn get_remote_addr(&self) -> Option<SocketAddr> {
        *self.remote_addr.read()
    }

    pub fn join_channel(&self, channel_id: ChannelId) {
        let mut channels = self.channels.write();
        if !channels.contains(&channel_id) {
            channels.push(channel_id);
        }
    }

    pub fn leave_channel(&self, channel_id: &ChannelId) {
        let mut channels = self.channels.write();
        channels.retain(|c| c != channel_id);
    }

    pub fn is_in_channel(&self, channel_id: &ChannelId) -> bool {
        self.channels.read().contains(channel_id)
    }

    pub fn get_channels(&self) -> Vec<ChannelId> {
        self.channels.read().clone()
    }

    pub fn update_heartbeat(&self) {
        *self.last_heartbeat.write() = Utc::now();
    }

    pub fn heartbeat_age_secs(&self) -> i64 {
        Utc::now()
            .signed_duration_since(*self.last_heartbeat.read())
            .num_seconds()
    }

    pub fn record_packet_sent(&self, bytes: u64) {
        self.packets_sent.fetch_add(1, Ordering::Relaxed);
        self.bytes_sent.fetch_add(bytes, Ordering::Relaxed);
    }

    pub fn record_packet_received(&self, bytes: u64) {
        self.packets_received.fetch_add(1, Ordering::Relaxed);
        self.bytes_received.fetch_add(bytes, Ordering::Relaxed);
    }

    /// Anti-replay check for an authenticated packet's sequence number.
    pub fn accept_sequence(&self, seq: u32) -> bool {
        self.replay.lock().check_and_update(seq)
    }

    /// Mark audio activity now. Returns `true` if the speaking state flipped to true.
    pub fn mark_audio_activity(&self) -> bool {
        self.last_audio_at_ms
            .store(Utc::now().timestamp_millis(), Ordering::Relaxed);
        !self.is_speaking.swap(true, Ordering::Relaxed)
    }

    /// Clear speaking if no audio arrived within `timeout_ms`. Returns `true` if it flipped to false.
    pub fn expire_speaking(&self, timeout_ms: i64) -> bool {
        if !self.is_speaking.load(Ordering::Relaxed) {
            return false;
        }
        let last = self.last_audio_at_ms.load(Ordering::Relaxed);
        if Utc::now().timestamp_millis() - last > timeout_ms {
            self.is_speaking.swap(false, Ordering::Relaxed)
        } else {
            false
        }
    }

    pub fn is_transmitting_allowed(&self) -> bool {
        self.is_active()
            && !self.is_muted.load(Ordering::Relaxed)
            && !self.is_server_muted.load(Ordering::Relaxed)
    }

    pub fn update_quality(&self, metrics: QualityMetrics) {
        *self.quality.write() = metrics;
    }

    pub fn get_quality(&self) -> QualityMetrics {
        self.quality.read().clone()
    }

    pub fn next_sequence(&self) -> u32 {
        self.sequence.fetch_add(1, Ordering::Relaxed)
    }

    pub fn stats(&self) -> SessionStats {
        SessionStats {
            packets_sent: self.packets_sent.load(Ordering::Relaxed),
            packets_received: self.packets_received.load(Ordering::Relaxed),
            bytes_sent: self.bytes_sent.load(Ordering::Relaxed),
            bytes_received: self.bytes_received.load(Ordering::Relaxed),
            quality: self.get_quality(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct SessionStats {
    pub packets_sent: u64,
    pub packets_received: u64,
    pub bytes_sent: u64,
    pub bytes_received: u64,
    pub quality: QualityMetrics,
}
