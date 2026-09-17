use aurix_common::types::*;
use chrono::{DateTime, Utc};
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;
use parking_lot::RwLock;

#[derive(Debug)]
pub struct MediaSession {
    pub session_id: SessionId,
    pub user_id: UserId,
    pub app_id: AppId,
    pub display_name: String,
    pub ssrc: u32,
    pub remote_addr: RwLock<Option<SocketAddr>>,
    pub channels: RwLock<Vec<ChannelId>>,
    pub is_muted: AtomicBool,
    pub is_server_muted: AtomicBool,
    pub is_speaking: AtomicBool,
    pub sequence: AtomicU32,
    pub last_audio_timestamp: AtomicU64,
    pub last_heartbeat: RwLock<DateTime<Utc>>,
    pub quality: RwLock<QualityMetrics>,
    pub created_at: DateTime<Utc>,
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
    ) -> Arc<Self> {
        Arc::new(Self {
            session_id,
            user_id,
            app_id,
            display_name,
            ssrc,
            remote_addr: RwLock::new(None),
            channels: RwLock::new(Vec::new()),
            is_muted: AtomicBool::new(false),
            is_server_muted: AtomicBool::new(false),
            is_speaking: AtomicBool::new(false),
            sequence: AtomicU32::new(0),
            last_audio_timestamp: AtomicU64::new(0),
            last_heartbeat: RwLock::new(Utc::now()),
            quality: RwLock::new(QualityMetrics {
                rtt_ms: 0.0,
                jitter_ms: 0.0,
                packet_loss_percent: 0.0,
                bitrate_kbps: 0,
                mos_score: 4.5,
            }),
            created_at: Utc::now(),
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

    pub fn deactivate(&self) {
        self.active.store(false, Ordering::Relaxed);
    }

    pub fn set_remote_addr(&self, addr: SocketAddr) {
        *self.remote_addr.write() = Some(addr);
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