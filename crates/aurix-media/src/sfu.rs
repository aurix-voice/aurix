use aurix_common::crypto::CryptoProvider;
use aurix_common::error::{AurixError, Result};
use aurix_common::types::*;
use crate::audio_pipeline::AudioAnalysisPipeline;
use crate::cascade::CascadeRelay;
use crate::channel::MediaChannel;
use crate::router::PacketRouter;
use crate::session::MediaSession;
use crate::webrtc::{WebRtcManager, WebRtcMediaEvent};
use chrono::Utc;
use dashmap::DashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use tokio::net::UdpSocket;
use tokio::sync::mpsc;
use tracing::{error, info, warn};

pub struct SfuNode {
    pub node_id: MediaNodeId,
    pub region: Region,
    channels: Arc<DashMap<ChannelId, Arc<MediaChannel>>>,
    sessions_by_id: Arc<DashMap<SessionId, Arc<MediaSession>>>,
    sessions_by_ssrc: Arc<DashMap<u32, Arc<MediaSession>>>,
    sessions_by_addr: Arc<DashMap<std::net::SocketAddr, Arc<MediaSession>>>,
    sessions_by_user: Arc<DashMap<UserId, Arc<MediaSession>>>,
    crypto: Arc<CryptoProvider>,
    active_participant_count: Arc<AtomicU32>,
    max_participants: u32,
    webrtc_manager: Option<Arc<WebRtcManager>>,
    cascade: Option<Arc<CascadeRelay>>,
    audio_pipeline: Option<Arc<AudioAnalysisPipeline>>,
}

impl SfuNode {
    pub fn new(node_id: MediaNodeId, region: Region, max_participants: u32) -> Self {
        Self {
            node_id, region,
            channels: Arc::new(DashMap::new()),
            sessions_by_id: Arc::new(DashMap::new()),
            sessions_by_ssrc: Arc::new(DashMap::new()),
            sessions_by_addr: Arc::new(DashMap::new()),
            sessions_by_user: Arc::new(DashMap::new()),
            crypto: Arc::new(CryptoProvider::new()),
            active_participant_count: Arc::new(AtomicU32::new(0)),
            max_participants,
            webrtc_manager: None,
            cascade: None,
            audio_pipeline: None,
        }
    }

    pub fn set_audio_pipeline(&mut self, pipeline: Arc<AudioAnalysisPipeline>) {
        self.audio_pipeline = Some(pipeline);
    }

    pub fn webrtc_manager(&self) -> Option<&Arc<WebRtcManager>> {
        self.webrtc_manager.as_ref()
    }

    pub async fn start(&mut self, bind_addr: &str) -> Result<()> {
        let socket = UdpSocket::bind(bind_addr).await
            .map_err(|e| AurixError::Transport(format!("Failed to bind UDP: {e}")))?;

        #[cfg(unix)]
        {
            use std::os::unix::io::AsRawFd;
            let fd = socket.as_raw_fd();
            let buf_size: libc::c_int = 4 * 1024 * 1024;
            unsafe {
                libc::setsockopt(fd, libc::SOL_SOCKET, libc::SO_RCVBUF, &buf_size as *const _ as *const libc::c_void, std::mem::size_of::<libc::c_int>() as libc::socklen_t);
                libc::setsockopt(fd, libc::SOL_SOCKET, libc::SO_SNDBUF, &buf_size as *const _ as *const libc::c_void, std::mem::size_of::<libc::c_int>() as libc::socklen_t);
            }
        }

        let local_addr = socket.local_addr()
            .map_err(|e| AurixError::Transport(format!("local_addr: {e}")))?;
        let socket = Arc::new(socket);

        // ── Initialize WebRTC manager ──
        let (webrtc_event_tx, mut webrtc_event_rx) = mpsc::channel::<WebRtcMediaEvent>(4096);
        let webrtc_mgr = Arc::new(WebRtcManager::new(socket.clone(), local_addr, webrtc_event_tx));
        self.webrtc_manager = Some(webrtc_mgr.clone());

        // ── Initialize Cascade Relay ──
        let cascade_port = local_addr.port() + 1;
        let cascade_addr = format!("{}:{}", local_addr.ip(), cascade_port);
        match CascadeRelay::new(&cascade_addr, self.node_id).await {
            Ok(cascade) => {
                let cascade = Arc::new(cascade);
                self.cascade = Some(cascade.clone());
                info!("Cascade relay started on {}", cascade_addr);
            }
            Err(e) => {
                warn!("Cascade relay init failed (cross-region disabled): {}", e);
            }
        }

        // ── Build router ──
        let router = Arc::new(PacketRouter::new(
            self.channels.clone(),
            self.sessions_by_ssrc.clone(),
            self.sessions_by_addr.clone(),
            socket.clone(),
            self.cascade.clone(),
            self.audio_pipeline.clone(),
        ));

        // ── Spawn WebRTC media event consumer ──
        {
            let router_for_webrtc = router.clone();
            let sessions_by_ssrc = self.sessions_by_ssrc.clone();
            let _sessions_by_addr = self.sessions_by_addr.clone();
            tokio::spawn(async move {
                while let Some(event) = webrtc_event_rx.recv().await {
                    match event {
                        WebRtcMediaEvent::AudioReceived { session_id: _, user_id: _, ssrc, sequence, timestamp, payload } => {
                            if let Some(session) = sessions_by_ssrc.get(&ssrc) {
                                session.record_packet_received(payload.len() as u64);
                                session.is_speaking.store(true, std::sync::atomic::Ordering::Relaxed);
                            }
                            let packet = aurix_common::protocol::AurixPacket::audio(
                                sequence as u32, timestamp, ssrc, 0,
                                bytes::Bytes::from(payload),
                            );
                            if let Err(e) = router_for_webrtc.route_webrtc_audio(&packet).await {
                                warn!("WebRTC audio route error: {}", e);
                            }
                        }
                        WebRtcMediaEvent::Connected { session_id } => {
                            info!("WebRTC session {} ICE connected", session_id);
                        }
                        WebRtcMediaEvent::Disconnected { session_id } => {
                            info!("WebRTC session {} disconnected", session_id);
                        }
                    }
                }
            });
        }

        // ── Start cascade receiver ──
        if let Some(ref cascade) = self.cascade {
            let router_for_cascade = router.clone();
            let cascade_clone = cascade.clone();
            cascade_clone.start_receiver(Arc::new(move |data: &[u8], src| {
                // Clone data into owned Vec so it can be moved into the spawned task
                let data_owned = data.to_vec();
                let rt = router_for_cascade.clone();
                tokio::spawn(async move {
                    if let Err(e) = rt.route_packet(&data_owned, src).await {
                        warn!("Cascade relay route error: {}", e);
                    }
                });
            }));
        }

        // ── Main UDP receive loop ──
        let router_clone = router.clone();
        let webrtc_for_recv = webrtc_mgr.clone();
        tokio::spawn(async move {
            let mut buf = vec![0u8; 2048];
            loop {
                match socket.recv_from(&mut buf).await {
                    Ok((len, src)) => {
                        let data = &buf[..len];
                        if aurix_common::protocol::is_aurix_packet(data) {
                            if let Err(e) = router_clone.route_packet(data, src).await {
                                warn!("Packet route error from {}: {}", src, e);
                            }
                        } else if WebRtcManager::is_webrtc_packet(data) {
                            webrtc_for_recv.handle_packet(data, src).await;
                        }
                    }
                    Err(e) => {
                        error!("UDP recv error: {}", e);
                        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                    }
                }
            }
        });

        self.start_session_cleanup();
        info!("SFU node {} started in region {:?}, listening on {}", self.node_id, self.region, bind_addr);
        Ok(())
    }

    pub fn create_session(&self, session_id: SessionId, user_id: UserId, app_id: AppId, display_name: String) -> Result<Arc<MediaSession>> {
        let count = self.active_participant_count.load(Ordering::Relaxed);
        if count >= self.max_participants {
            return Err(AurixError::MediaNodeUnavailable("Node at capacity".into()));
        }
        if let Some((_, old_session)) = self.sessions_by_user.remove(&user_id) {
            old_session.deactivate();
            self.sessions_by_id.remove(&old_session.session_id);
            self.sessions_by_ssrc.remove(&old_session.ssrc);
            if let Some(addr) = old_session.get_remote_addr() { self.sessions_by_addr.remove(&addr); }
            for ch in &old_session.get_channels() {
                if let Some(channel) = self.channels.get(ch) { channel.remove_participant(&user_id); }
            }
            self.active_participant_count.fetch_sub(1, Ordering::Relaxed);
            info!("Replaced existing session for user {}", user_id);
        }
        let ssrc = self.crypto.generate_ssrc();
        let session = MediaSession::new(session_id, user_id, app_id, display_name, ssrc);
        self.sessions_by_id.insert(session_id, session.clone());
        self.sessions_by_ssrc.insert(ssrc, session.clone());
        self.sessions_by_user.insert(user_id, session.clone());
        self.active_participant_count.fetch_add(1, Ordering::Relaxed);
        aurix_metrics::SESSIONS_TOTAL.inc();
        aurix_metrics::ACTIVE_SESSIONS.inc();
        info!("Session created: {} for user {} (SSRC: {})", session_id, user_id, ssrc);
        Ok(session)
    }

    pub fn register_session_addr(&self, session_id: &SessionId, addr: std::net::SocketAddr) -> Result<()> {
        let session = self.sessions_by_id.get(session_id).ok_or_else(|| AurixError::SessionNotFound(session_id.to_string()))?.value().clone();
        session.set_remote_addr(addr);
        self.sessions_by_addr.insert(addr, session);
        Ok(())
    }

    pub fn destroy_session(&self, session_id: &SessionId) -> Result<()> {
        let session = self.sessions_by_id.remove(session_id).map(|(_, s)| s).ok_or_else(|| AurixError::SessionNotFound(session_id.to_string()))?;
        session.deactivate();
        if let Some(ref pipeline) = self.audio_pipeline { pipeline.remove_user(session.user_id); }
        for ch_id in &session.get_channels() {
            if let Some(channel) = self.channels.get(ch_id) {
                channel.remove_participant(&session.user_id);
                if channel.is_empty() { drop(channel); self.channels.remove(ch_id); aurix_metrics::ACTIVE_CHANNELS.dec(); }
            }
        }
        self.sessions_by_ssrc.remove(&session.ssrc);
        self.sessions_by_user.remove(&session.user_id);
        if let Some(addr) = session.get_remote_addr() { self.sessions_by_addr.remove(&addr); }
        if let Some(ref wm) = self.webrtc_manager { wm.remove_session(session_id); }
        self.active_participant_count.fetch_sub(1, Ordering::Relaxed);
        aurix_metrics::ACTIVE_SESSIONS.dec();
        let elapsed = Utc::now().signed_duration_since(session.created_at).num_seconds();
        aurix_metrics::SESSION_DURATION.observe(elapsed as f64);
        info!("Session destroyed: {} for user {}", session_id, session.user_id);
        Ok(())
    }

    pub fn join_channel(&self, session_id: &SessionId, channel_id: ChannelId, config: ChannelConfig, role: ChannelRole) -> Result<Vec<Arc<MediaSession>>> {
        let session = self.sessions_by_id.get(session_id).ok_or_else(|| AurixError::SessionNotFound(session_id.to_string()))?.value().clone();
        let is_new = !self.channels.contains_key(&channel_id);
        let channel = self.channels.entry(channel_id).or_insert_with(|| Arc::new(MediaChannel::new(channel_id, session.app_id, config))).value().clone();
        channel.add_participant(session.clone(), role)?;
        session.join_channel(channel_id);
        if is_new { aurix_metrics::ACTIVE_CHANNELS.inc(); }
        let existing = channel.get_other_participants(&session.user_id);
        info!("User {} joined channel {} as {:?} (now {} participants)", session.user_id, channel_id, role, channel.participant_count());
        Ok(existing)
    }

    pub fn update_position(&self, user_id: &UserId, channel_id: &ChannelId, position: Position3D) {
        if let Some(channel) = self.channels.get(channel_id) { channel.update_position(user_id, position); }
    }

    pub fn leave_channel(&self, session_id: &SessionId, channel_id: &ChannelId) -> Result<()> {
        let session = self.sessions_by_id.get(session_id).ok_or_else(|| AurixError::SessionNotFound(session_id.to_string()))?.value().clone();
        if let Some(channel) = self.channels.get(channel_id) {
            channel.remove_participant(&session.user_id);
            session.leave_channel(channel_id);
            if channel.is_empty() { drop(channel); self.channels.remove(channel_id); aurix_metrics::ACTIVE_CHANNELS.dec(); }
        }
        Ok(())
    }

    pub fn server_mute_user(&self, user_id: &UserId, muted: bool) -> Result<()> {
        let session = self.sessions_by_user.get(user_id).ok_or_else(|| AurixError::UserNotFound(user_id.to_string()))?.value().clone();
        session.is_server_muted.store(muted, Ordering::Relaxed);
        Ok(())
    }

    pub fn kick_user_from_channel(&self, user_id: &UserId, channel_id: &ChannelId) -> Result<()> {
        let session = self.sessions_by_user.get(user_id).ok_or_else(|| AurixError::UserNotFound(user_id.to_string()))?.value().clone();
        if let Some(channel) = self.channels.get(channel_id) {
            channel.remove_participant(user_id);
            session.leave_channel(channel_id);
            if channel.is_empty() { drop(channel); self.channels.remove(channel_id); aurix_metrics::ACTIVE_CHANNELS.dec(); }
        }
        Ok(())
    }

    pub fn get_channel_participants(&self, channel_id: &ChannelId) -> Vec<Arc<MediaSession>> {
        self.channels.get(channel_id).map(|ch| ch.get_all_participants()).unwrap_or_default()
    }
    pub fn get_session(&self, session_id: &SessionId) -> Option<Arc<MediaSession>> {
        self.sessions_by_id.get(session_id).map(|s| s.value().clone())
    }
    pub fn get_session_by_user(&self, user_id: &UserId) -> Option<Arc<MediaSession>> {
        self.sessions_by_user.get(user_id).map(|s| s.value().clone())
    }
    pub fn active_channels(&self) -> u32 { self.channels.len() as u32 }
    pub fn active_participants(&self) -> u32 { self.active_participant_count.load(Ordering::Relaxed) }
    pub fn get_user_channels(&self, user_id: &UserId) -> Vec<ChannelId> {
        self.sessions_by_user.get(user_id).map(|s| s.get_channels()).unwrap_or_default()
    }

    pub fn node_info(&self, address: &str, media_port: u16, api_port: u16) -> MediaNodeInfo {
        MediaNodeInfo {
            id: self.node_id, region: self.region, address: address.to_string(),
            media_port, api_port, active_channels: self.active_channels(),
            active_participants: self.active_participants(),
            cpu_usage: 0.0, memory_usage: 0.0, bandwidth_in_mbps: 0.0,
            bandwidth_out_mbps: 0.0, healthy: true, last_heartbeat: Utc::now(),
            capacity: self.max_participants,
        }
    }

    fn start_session_cleanup(&self) {
        let sessions = self.sessions_by_id.clone();
        let sessions_by_ssrc = self.sessions_by_ssrc.clone();
        let sessions_by_addr = self.sessions_by_addr.clone();
        let sessions_by_user = self.sessions_by_user.clone();
        let channels = self.channels.clone();
        let count = self.active_participant_count.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(10));
            loop {
                interval.tick().await;
                let mut to_remove = Vec::new();
                for entry in sessions.iter() {
                    if entry.value().heartbeat_age_secs() > 60 { to_remove.push(*entry.key()); }
                }
                for sid in to_remove {
                    if let Some((_, session)) = sessions.remove(&sid) {
                        session.deactivate();
                        for ch_id in &session.get_channels() {
                            if let Some(channel) = channels.get(ch_id) {
                                channel.remove_participant(&session.user_id);
                                if channel.is_empty() { drop(channel); channels.remove(ch_id); }
                            }
                        }
                        sessions_by_ssrc.remove(&session.ssrc);
                        sessions_by_user.remove(&session.user_id);
                        if let Some(addr) = session.get_remote_addr() { sessions_by_addr.remove(&addr); }
                        count.fetch_sub(1, Ordering::Relaxed);
                        warn!("Session {} timed out for user {}", sid, session.user_id);
                    }
                }
            }
        });
    }
}