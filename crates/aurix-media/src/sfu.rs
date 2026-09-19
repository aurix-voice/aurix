use crate::audio_pipeline::AudioAnalysisPipeline;
use crate::cascade::CascadeRelay;
use crate::channel::MediaChannel;
use crate::router::{MediaEvent, PacketRouter, RouterShared};
use crate::session::{MediaSession, ReceiverPrefs, Transport, DEFAULT_UNFOCUSED_GAIN};
use crate::webrtc::{WebRtcManager, WebRtcMediaEvent};
use aurix_common::crypto::CryptoProvider;
use aurix_common::error::{AurixError, Result};
use aurix_common::protocol::channel_id_hash;
use aurix_common::sink::AudioSink;
use aurix_common::types::*;
use chrono::Utc;
use dashmap::DashMap;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use tokio::net::UdpSocket;
use tokio::sync::{broadcast, mpsc};
use tracing::{error, info, warn};

/// Level change (dB) below which a participant is left out of the next `ChannelEnergy` report.
const ENERGY_REPORT_MIN_STEP_DB: u8 = 3;

/// Side effects of leaving a channel that the client must be told about.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ChannelLeft {
    /// `TransmissionMode::Single` pointed at the left channel and fell back to `None`.
    pub transmission_reset: bool,
    /// The left channel was the focused one; focus is now cleared.
    pub focus_reset: bool,
}

/// Media-plane tunables taken from `MediaConfig`.
#[derive(Debug, Clone)]
pub struct SfuOptions {
    pub max_participants: u32,
    pub max_channels: u32,
    pub require_packet_auth: bool,
    pub speaking_timeout_ms: u64,
    /// Minimum linear level of a labeled frame to count as voice (see `MediaConfig`).
    pub speaking_energy_threshold: f32,
    /// Period of `MediaEvent::ChannelEnergy` reports (0 = disabled).
    pub energy_interval_ms: u64,
    /// Channels a single session may be joined to at once.
    pub max_channels_per_session: u32,
    /// Positional channels a single session may be joined to at once (0 = unlimited).
    pub max_positional_channels_per_session: u32,
    /// Gain for channels other than the one a session focused (see `ReceiverPrefs`).
    pub unfocused_channel_gain: f32,
    pub session_timeout_secs: u64,
    pub cascade_secret: Option<String>,
    pub cascade_peers: Vec<String>,
    /// Address advertised to WebRTC clients (external IP + media port).
    pub advertised_addr: Option<SocketAddr>,
    pub downlink_bitrate: u32,
    /// Number of concurrent UDP receive workers (0 = derive from available CPUs).
    pub rx_workers: usize,
}

impl Default for SfuOptions {
    fn default() -> Self {
        Self {
            max_participants: 5000,
            max_channels: 1000,
            require_packet_auth: true,
            speaking_timeout_ms: 400,
            speaking_energy_threshold: 0.01,
            energy_interval_ms: 200,
            max_channels_per_session: 10,
            max_positional_channels_per_session: 1,
            unfocused_channel_gain: DEFAULT_UNFOCUSED_GAIN,
            session_timeout_secs: 60,
            cascade_secret: None,
            cascade_peers: Vec::new(),
            advertised_addr: None,
            downlink_bitrate: 32_000,
            rx_workers: 0,
        }
    }
}

pub struct SfuNode {
    pub node_id: MediaNodeId,
    pub region: Region,
    options: SfuOptions,
    channels: Arc<DashMap<ChannelId, Arc<MediaChannel>>>,
    channels_by_hash: Arc<DashMap<u32, ChannelId>>,
    sessions_by_id: Arc<DashMap<SessionId, Arc<MediaSession>>>,
    sessions_by_addr: Arc<DashMap<SocketAddr, Arc<MediaSession>>>,
    sessions_by_user: Arc<DashMap<UserId, Arc<MediaSession>>>,
    crypto: Arc<CryptoProvider>,
    active_participant_count: Arc<AtomicU32>,
    webrtc_manager: Option<Arc<WebRtcManager>>,
    router: Option<Arc<PacketRouter>>,
    cascade: Option<Arc<CascadeRelay>>,
    audio_pipeline: Option<Arc<AudioAnalysisPipeline>>,
    audio_sink: Option<Arc<dyn AudioSink>>,
    events: broadcast::Sender<MediaEvent>,
    local_addr: Option<SocketAddr>,
    started: bool,
}

impl SfuNode {
    pub fn new(node_id: MediaNodeId, region: Region, options: SfuOptions) -> Self {
        let (events, _) = broadcast::channel(4096);
        Self {
            node_id,
            region,
            options,
            channels: Arc::new(DashMap::new()),
            channels_by_hash: Arc::new(DashMap::new()),
            sessions_by_id: Arc::new(DashMap::new()),
            sessions_by_addr: Arc::new(DashMap::new()),
            sessions_by_user: Arc::new(DashMap::new()),
            crypto: Arc::new(CryptoProvider::new()),
            active_participant_count: Arc::new(AtomicU32::new(0)),
            webrtc_manager: None,
            router: None,
            cascade: None,
            audio_pipeline: None,
            audio_sink: None,
            events,
            local_addr: None,
            started: false,
        }
    }

    pub fn set_audio_pipeline(&mut self, pipeline: Arc<AudioAnalysisPipeline>) {
        self.audio_pipeline = Some(pipeline);
    }

    /// Attach a recording/analysis tap. Must be called before `start`.
    pub fn set_audio_sink(&mut self, sink: Arc<dyn AudioSink>) {
        self.audio_sink = Some(sink);
    }

    pub fn webrtc_manager(&self) -> Option<&Arc<WebRtcManager>> {
        self.webrtc_manager.as_ref()
    }

    pub fn cascade(&self) -> Option<&Arc<CascadeRelay>> {
        self.cascade.as_ref()
    }

    /// The packet router; `None` until `start`.
    pub fn router(&self) -> Option<&Arc<PacketRouter>> {
        self.router.as_ref()
    }

    /// Subscribe to media-plane notifications (speaking/mute changes, session binds).
    pub fn subscribe_events(&self) -> broadcast::Receiver<MediaEvent> {
        self.events.subscribe()
    }

    /// Bound UDP address once started.
    pub fn local_addr(&self) -> Option<SocketAddr> {
        self.local_addr
    }

    pub fn options(&self) -> &SfuOptions {
        &self.options
    }

    fn rx_worker_count(configured: usize) -> usize {
        if configured > 0 {
            return configured;
        }
        std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(2)
            .clamp(2, 8)
    }

    pub async fn start(&mut self, bind_addr: &str) -> Result<()> {
        if self.started {
            return Err(AurixError::Internal("SFU already started".into()));
        }
        let socket = UdpSocket::bind(bind_addr)
            .await
            .map_err(|e| AurixError::Transport(format!("Failed to bind UDP: {e}")))?;

        #[cfg(unix)]
        {
            use std::os::unix::io::AsRawFd;
            let fd = socket.as_raw_fd();
            let buf_size: libc::c_int = 4 * 1024 * 1024;
            // SAFETY: fd is a valid open socket; the pointer/len describe a live c_int.
            unsafe {
                libc::setsockopt(
                    fd,
                    libc::SOL_SOCKET,
                    libc::SO_RCVBUF,
                    &buf_size as *const _ as *const libc::c_void,
                    std::mem::size_of::<libc::c_int>() as libc::socklen_t,
                );
                libc::setsockopt(
                    fd,
                    libc::SOL_SOCKET,
                    libc::SO_SNDBUF,
                    &buf_size as *const _ as *const libc::c_void,
                    std::mem::size_of::<libc::c_int>() as libc::socklen_t,
                );
            }
        }

        let local_addr = socket
            .local_addr()
            .map_err(|e| AurixError::Transport(format!("local_addr: {e}")))?;
        let socket = Arc::new(socket);

        let advertised = self.options.advertised_addr.unwrap_or(local_addr);
        let (webrtc_event_tx, mut webrtc_event_rx) = mpsc::channel::<WebRtcMediaEvent>(4096);
        let webrtc_mgr = Arc::new(WebRtcManager::new(
            socket.clone(),
            advertised,
            webrtc_event_tx,
            self.options.downlink_bitrate,
        ));
        self.webrtc_manager = Some(webrtc_mgr.clone());

        if let Some(secret) = self.options.cascade_secret.clone() {
            let cascade_addr = format!("{}:{}", local_addr.ip(), local_addr.port().wrapping_add(1));
            match CascadeRelay::new(
                &cascade_addr,
                self.node_id,
                &secret,
                &self.options.cascade_peers,
            )
            .await
            {
                Ok(cascade) => self.cascade = Some(Arc::new(cascade)),
                Err(e) => {
                    return Err(AurixError::InvalidConfiguration(format!(
                        "Cascade relay init failed: {e}"
                    )))
                }
            }
        } else if !self.options.cascade_peers.is_empty() {
            return Err(AurixError::InvalidConfiguration(
                "media.cascade_peers set without media.cascade_secret".into(),
            ));
        }

        let router = Arc::new(PacketRouter::new(
            RouterShared {
                channels: self.channels.clone(),
                channels_by_hash: self.channels_by_hash.clone(),
                sessions_by_id: self.sessions_by_id.clone(),
                sessions_by_addr: self.sessions_by_addr.clone(),
            },
            socket.clone(),
            self.cascade.clone(),
            self.audio_pipeline.clone(),
            Some(webrtc_mgr.clone()),
            self.audio_sink.clone(),
            self.options.require_packet_auth,
            self.options.speaking_energy_threshold,
            self.events.clone(),
        ));

        // WebRTC media events -> router
        {
            let router = router.clone();
            let sessions_by_id = self.sessions_by_id.clone();
            let sessions_by_addr = self.sessions_by_addr.clone();
            let events = self.events.clone();
            tokio::spawn(async move {
                while let Some(event) = webrtc_event_rx.recv().await {
                    match event {
                        WebRtcMediaEvent::AudioReceived {
                            session_id,
                            user_id,
                            rtp_time,
                            payload,
                            level,
                        } => {
                            let session =
                                sessions_by_id.get(&session_id).map(|s| s.value().clone());
                            let Some(session) = session else { continue };
                            if session.user_id != user_id
                                || session.transport() != Transport::WebRtc
                            {
                                continue;
                            }
                            if let Err(e) = router
                                .route_webrtc_audio(&session, rtp_time, payload, level)
                                .await
                            {
                                warn!("WebRTC audio route error: {}", e);
                            }
                        }
                        WebRtcMediaEvent::Connected { session_id, remote } => {
                            if let Some(session) =
                                sessions_by_id.get(&session_id).map(|s| s.value().clone())
                            {
                                session.set_remote_addr(remote);
                                session.update_heartbeat();
                                sessions_by_addr.insert(remote, session.clone());
                                let _ = events.send(MediaEvent::SessionBound {
                                    session_id,
                                    addr: remote,
                                });
                            }
                            info!("WebRTC session {} connected from {}", session_id, remote);
                        }
                        WebRtcMediaEvent::Disconnected { session_id } => {
                            if let Some(session) =
                                sessions_by_id.get(&session_id).map(|s| s.value().clone())
                            {
                                if let Some(addr) = session.clear_remote_addr() {
                                    sessions_by_addr.remove(&addr);
                                }
                            }
                            info!("WebRTC session {} disconnected", session_id);
                        }
                    }
                }
            });
        }

        if let Some(ref cascade) = self.cascade {
            let router = router.clone();
            cascade
                .clone()
                .start_receiver(Arc::new(move |sender, packet| {
                    let router = router.clone();
                    tokio::spawn(async move {
                        if let Err(e) = router.route_relayed_audio(&sender, &packet).await {
                            warn!("Cascade relay route error: {}", e);
                        }
                    });
                }));
        }

        // UDP receive workers. Several tasks drain the same socket so that per-packet work
        // (HMAC verification, fan-out signing, sends) runs in parallel across the runtime
        // instead of serialising behind a single recv loop and overflowing SO_RCVBUF.
        let workers = Self::rx_worker_count(self.options.rx_workers);
        for worker in 0..workers {
            let router = router.clone();
            let webrtc = webrtc_mgr.clone();
            let socket = socket.clone();
            tokio::spawn(async move {
                let mut buf = vec![0u8; 2048];
                loop {
                    match socket.recv_from(&mut buf).await {
                        Ok((len, src)) => {
                            let data = &buf[..len];
                            if aurix_common::protocol::is_aurix_packet(data) {
                                if let Err(e) = router.route_packet(data, src).await {
                                    tracing::debug!("Packet dropped from {}: {}", src, e);
                                }
                            } else if WebRtcManager::is_webrtc_packet(data) {
                                webrtc.handle_packet(data, src).await;
                            }
                        }
                        Err(e) => {
                            error!("UDP recv error (worker {}): {}", worker, e);
                            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                        }
                    }
                }
            });
        }
        info!("SFU UDP receive workers: {}", workers);

        self.start_session_cleanup();
        self.start_speaking_timeout();
        self.start_energy_reports();
        if let Some(ref pipeline) = self.audio_pipeline {
            pipeline.spawn_idle_flusher(self.channels.clone());
        }
        self.local_addr = Some(local_addr);
        self.router = Some(router);
        self.started = true;
        info!(
            "SFU node {} started in region {:?}, listening on {} (advertised {})",
            self.node_id, self.region, local_addr, advertised
        );
        Ok(())
    }

    /// Create a session and return it. The per-session `media_key` must be delivered to the
    /// client over the authenticated control channel; it never appears on the media path.
    pub fn create_session(
        &self,
        session_id: SessionId,
        user_id: UserId,
        app_id: AppId,
        display_name: String,
    ) -> Result<Arc<MediaSession>> {
        if let Some((_, old)) = self.sessions_by_user.remove(&user_id) {
            self.teardown_session(&old);
            info!("Replaced existing session for user {}", user_id);
        }
        let max = self.options.max_participants;
        if self
            .active_participant_count
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |c| {
                if c >= max {
                    None
                } else {
                    Some(c + 1)
                }
            })
            .is_err()
        {
            return Err(AurixError::MediaNodeUnavailable("Node at capacity".into()));
        }
        let ssrc = self.crypto.generate_ssrc();
        let mut media_key = [0u8; 32];
        match self.crypto.generate_random_bytes(32) {
            Ok(k) => media_key.copy_from_slice(&k),
            Err(e) => {
                self.active_participant_count.fetch_sub(1, Ordering::AcqRel);
                return Err(e);
            }
        }
        let session = MediaSession::new(session_id, user_id, app_id, display_name, ssrc, media_key);
        *session.prefs.write() =
            ReceiverPrefs::with_unfocused_gain(self.options.unfocused_channel_gain);
        self.sessions_by_id.insert(session_id, session.clone());
        self.sessions_by_user.insert(user_id, session.clone());
        aurix_metrics::SESSIONS_TOTAL.inc();
        aurix_metrics::ACTIVE_SESSIONS.inc();
        info!(
            "Session created: {} for user {} (SSRC: {})",
            session_id, user_id, ssrc
        );
        Ok(session)
    }

    /// Accept a browser SDP offer for an existing session and switch it to the WebRTC transport.
    pub fn attach_webrtc(&self, session_id: &SessionId, offer_sdp: &str) -> Result<String> {
        let session = self
            .get_session(session_id)
            .ok_or_else(|| AurixError::SessionNotFound(session_id.to_string()))?;
        let manager = self
            .webrtc_manager
            .as_ref()
            .ok_or_else(|| AurixError::Internal("WebRTC not started".into()))?;
        if manager.has_session(session_id) {
            manager.remove_session(session_id);
        }
        if let Some(addr) = session.clear_remote_addr() {
            self.sessions_by_addr.remove(&addr);
        }
        let answer = manager.create_session(offer_sdp, *session_id, session.user_id)?;
        session.set_transport(Transport::WebRtc);
        session.update_heartbeat();
        Ok(answer)
    }

    fn teardown_session(&self, session: &Arc<MediaSession>) {
        session.deactivate();
        self.sessions_by_id.remove(&session.session_id);
        if let Some(addr) = session.clear_remote_addr() {
            self.sessions_by_addr.remove(&addr);
        }
        for ch in session.get_channels() {
            self.remove_from_channel(&ch, session);
        }
        if let Some(ref pipeline) = self.audio_pipeline {
            pipeline.remove_user(session.user_id);
        }
        if let Some(ref wm) = self.webrtc_manager {
            wm.remove_session(&session.session_id);
        }
        self.active_participant_count.fetch_sub(1, Ordering::AcqRel);
        aurix_metrics::ACTIVE_SESSIONS.dec();
        let elapsed = Utc::now()
            .signed_duration_since(session.created_at)
            .num_seconds();
        aurix_metrics::SESSION_DURATION.observe(elapsed as f64);
    }

    fn remove_from_channel(
        &self,
        channel_id: &ChannelId,
        session: &Arc<MediaSession>,
    ) -> ChannelLeft {
        session.leave_channel(channel_id);
        let (transmission_reset, focus_reset) = session.forget_channel(channel_id);
        let left = ChannelLeft {
            transmission_reset,
            focus_reset,
        };
        let Some(channel) = self.channels.get(channel_id).map(|c| c.value().clone()) else {
            return left;
        };
        channel.remove_participant(&session.user_id);
        if let Some(ref sink) = self.audio_sink {
            sink.on_participant_left(*channel_id, session.user_id);
        }
        if let Some(ref pipeline) = self.audio_pipeline {
            pipeline.participant_left(&channel, session.user_id);
        }
        if channel.is_empty() {
            self.channels.remove(channel_id);
            self.channels_by_hash.remove(&channel_id_hash(channel_id));
            if let Some(ref cascade) = self.cascade {
                cascade.remove_channel(channel_id);
            }
            aurix_metrics::ACTIVE_CHANNELS.dec();
        }
        left
    }

    pub fn destroy_session(&self, session_id: &SessionId) -> Result<()> {
        let session = self
            .sessions_by_id
            .get(session_id)
            .map(|s| s.value().clone())
            .ok_or_else(|| AurixError::SessionNotFound(session_id.to_string()))?;
        // Only drop the user index if it still points at this session.
        self.sessions_by_user
            .remove_if(&session.user_id, |_, s| s.session_id == *session_id);
        self.teardown_session(&session);
        info!(
            "Session destroyed: {} for user {}",
            session_id, session.user_id
        );
        Ok(())
    }

    pub fn join_channel(
        &self,
        session_id: &SessionId,
        channel_id: ChannelId,
        config: ChannelConfig,
        role: ChannelRole,
    ) -> Result<Vec<Arc<MediaSession>>> {
        let session = self
            .get_session(session_id)
            .ok_or_else(|| AurixError::SessionNotFound(session_id.to_string()))?;
        if !session.is_in_channel(&channel_id) {
            self.check_session_channel_limits(&session, config.channel_type)?;
        }
        if !self.channels.contains_key(&channel_id)
            && self.channels.len() as u32 >= self.options.max_channels
        {
            return Err(AurixError::MediaNodeUnavailable(
                "Node channel capacity reached".into(),
            ));
        }
        let hash = channel_id_hash(&channel_id);
        if let Some(existing) = self.channels_by_hash.get(&hash) {
            if *existing.value() != channel_id {
                return Err(AurixError::Internal(
                    "Channel id hash collision on this node".into(),
                ));
            }
        }
        let mut is_new = false;
        let channel = self
            .channels
            .entry(channel_id)
            .or_insert_with(|| {
                is_new = true;
                Arc::new(MediaChannel::new(channel_id, session.app_id, config))
            })
            .value()
            .clone();
        if channel.app_id != session.app_id {
            return Err(AurixError::AuthorizationDenied(
                "Channel belongs to a different application".into(),
            ));
        }
        channel.add_participant(session.clone(), role)?;
        session.join_channel(channel_id);
        if is_new {
            self.channels_by_hash.insert(hash, channel_id);
            if let Some(ref cascade) = self.cascade {
                cascade.add_all_peers(channel_id);
            }
            aurix_metrics::ACTIVE_CHANNELS.inc();
        }
        let existing = channel.get_other_participants(&session.user_id);
        for peer in &existing {
            peer.reset_energy_report();
        }
        info!(
            "User {} joined channel {} as {:?} (now {} participants)",
            session.user_id,
            channel_id,
            role,
            channel.participant_count()
        );
        Ok(existing)
    }

    fn check_session_channel_limits(
        &self,
        session: &MediaSession,
        joining: ChannelType,
    ) -> Result<()> {
        let joined = session.get_channels();
        if joined.len() as u32 >= self.options.max_channels_per_session {
            return Err(AurixError::ChannelLimitExceeded(format!(
                "session may join at most {} channels",
                self.options.max_channels_per_session
            )));
        }
        let max_positional = self.options.max_positional_channels_per_session;
        if joining == ChannelType::Positional && max_positional != 0 {
            let positional = joined
                .iter()
                .filter(|c| {
                    self.channels
                        .get(c)
                        .is_some_and(|ch| ch.channel_type == ChannelType::Positional)
                })
                .count() as u32;
            if positional >= max_positional {
                return Err(AurixError::ChannelLimitExceeded(format!(
                    "session may join at most {max_positional} positional channel(s)"
                )));
            }
        }
        Ok(())
    }

    pub fn update_position(
        &self,
        user_id: &UserId,
        channel_id: &ChannelId,
        position: Position3D,
        orientation: Orientation3D,
    ) {
        if let Some(channel) = self.channels.get(channel_id) {
            channel.update_position(user_id, position, orientation);
        }
    }

    pub fn leave_channel(
        &self,
        session_id: &SessionId,
        channel_id: &ChannelId,
    ) -> Result<ChannelLeft> {
        let session = self
            .get_session(session_id)
            .ok_or_else(|| AurixError::SessionNotFound(session_id.to_string()))?;
        Ok(self.remove_from_channel(channel_id, &session))
    }

    pub fn server_mute_user(&self, user_id: &UserId, muted: bool) -> Result<()> {
        let session = self
            .get_session_by_user(user_id)
            .ok_or_else(|| AurixError::UserNotFound(user_id.to_string()))?;
        session.is_server_muted.store(muted, Ordering::Relaxed);
        Ok(())
    }

    pub fn kick_user_from_channel(&self, user_id: &UserId, channel_id: &ChannelId) -> Result<()> {
        let session = self
            .get_session_by_user(user_id)
            .ok_or_else(|| AurixError::UserNotFound(user_id.to_string()))?;
        self.remove_from_channel(channel_id, &session);
        Ok(())
    }

    pub fn get_channel_participants(&self, channel_id: &ChannelId) -> Vec<Arc<MediaSession>> {
        self.channels
            .get(channel_id)
            .map(|ch| ch.get_all_participants())
            .unwrap_or_default()
    }
    pub fn get_channel(&self, channel_id: &ChannelId) -> Option<Arc<MediaChannel>> {
        self.channels.get(channel_id).map(|c| c.value().clone())
    }
    pub fn get_session(&self, session_id: &SessionId) -> Option<Arc<MediaSession>> {
        self.sessions_by_id
            .get(session_id)
            .map(|s| s.value().clone())
    }
    pub fn get_session_by_user(&self, user_id: &UserId) -> Option<Arc<MediaSession>> {
        self.sessions_by_user
            .get(user_id)
            .map(|s| s.value().clone())
    }
    /// Route one Opus frame on behalf of `session_id` as if it had arrived over its WebRTC
    /// uplink (one frame fans out to every channel the transmission mode allows).
    pub async fn route_webrtc_audio(
        &self,
        session_id: &SessionId,
        rtp_time: u32,
        payload: Vec<u8>,
        level: Option<u8>,
    ) -> Result<()> {
        let router = self
            .router
            .as_ref()
            .ok_or_else(|| AurixError::Internal("SFU not started".into()))?;
        let session = self
            .get_session(session_id)
            .ok_or_else(|| AurixError::SessionNotFound(session_id.to_string()))?;
        router
            .route_webrtc_audio(&session, rtp_time, payload, level)
            .await
    }
    /// Every live session of a user on this node (a user may hold a session per device).
    pub fn sessions_for_user(&self, user_id: &UserId) -> Vec<Arc<MediaSession>> {
        self.sessions_by_id
            .iter()
            .filter(|e| e.value().user_id == *user_id)
            .map(|e| e.value().clone())
            .collect()
    }
    pub fn active_channels(&self) -> u32 {
        self.channels.len() as u32
    }
    pub fn channel_ids(&self) -> Vec<ChannelId> {
        self.channels.iter().map(|e| *e.key()).collect()
    }
    pub fn active_participants(&self) -> u32 {
        self.active_participant_count.load(Ordering::Relaxed)
    }
    pub fn get_user_channels(&self, user_id: &UserId) -> Vec<ChannelId> {
        self.sessions_by_user
            .get(user_id)
            .map(|s| s.get_channels())
            .unwrap_or_default()
    }

    pub fn node_info(&self, address: &str, media_port: u16, api_port: u16) -> MediaNodeInfo {
        MediaNodeInfo {
            id: self.node_id,
            region: self.region,
            address: address.to_string(),
            media_port,
            api_port,
            cascade_port: self.cascade.as_ref().map(|c| c.local_addr().port()),
            active_channels: self.active_channels(),
            active_participants: self.active_participants(),
            cpu_usage: 0.0,
            memory_usage: 0.0,
            bandwidth_in_mbps: 0.0,
            bandwidth_out_mbps: 0.0,
            healthy: true,
            last_heartbeat: Utc::now(),
            capacity: self.options.max_participants,
        }
    }

    fn start_session_cleanup(&self) {
        let sessions = self.sessions_by_id.clone();
        let sessions_by_user = self.sessions_by_user.clone();
        let sessions_by_addr = self.sessions_by_addr.clone();
        let channels = self.channels.clone();
        let channels_by_hash = self.channels_by_hash.clone();
        let count = self.active_participant_count.clone();
        let webrtc = self.webrtc_manager.clone();
        let sink = self.audio_sink.clone();
        let timeout = self.options.session_timeout_secs as i64;
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(10));
            loop {
                interval.tick().await;
                let stale: Vec<SessionId> = sessions
                    .iter()
                    .filter(|e| e.value().heartbeat_age_secs() > timeout)
                    .map(|e| *e.key())
                    .collect();
                for sid in stale {
                    let Some((_, session)) = sessions.remove(&sid) else {
                        continue;
                    };
                    session.deactivate();
                    sessions_by_user.remove_if(&session.user_id, |_, s| s.session_id == sid);
                    if let Some(addr) = session.clear_remote_addr() {
                        sessions_by_addr.remove(&addr);
                    }
                    for ch_id in session.get_channels() {
                        if let Some(channel) = channels.get(&ch_id).map(|c| c.value().clone()) {
                            channel.remove_participant(&session.user_id);
                            if let Some(ref sink) = sink {
                                sink.on_participant_left(ch_id, session.user_id);
                            }
                            if channel.is_empty() {
                                channels.remove(&ch_id);
                                channels_by_hash.remove(&channel_id_hash(&ch_id));
                                aurix_metrics::ACTIVE_CHANNELS.dec();
                            }
                        }
                    }
                    if let Some(ref wm) = webrtc {
                        wm.remove_session(&sid);
                    }
                    count.fetch_sub(1, Ordering::AcqRel);
                    aurix_metrics::ACTIVE_SESSIONS.dec();
                    warn!("Session {} timed out for user {}", sid, session.user_id);
                }
            }
        });
    }

    /// Tears down every live session (graceful shutdown). Recording sinks get their
    /// `on_participant_left` callbacks so open files are finalized.
    pub fn shutdown(&self) -> usize {
        let ids: Vec<SessionId> = self.sessions_by_id.iter().map(|e| *e.key()).collect();
        let n = ids.len();
        for sid in ids {
            let _ = self.destroy_session(&sid);
        }
        n
    }

    fn start_speaking_timeout(&self) {
        let sessions = self.sessions_by_id.clone();
        let events = self.events.clone();
        let timeout_ms = self.options.speaking_timeout_ms as i64;
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_millis(100));
            loop {
                interval.tick().await;
                let stopped: Vec<Arc<MediaSession>> = sessions
                    .iter()
                    .filter(|e| e.value().expire_speaking(timeout_ms))
                    .map(|e| e.value().clone())
                    .collect();
                for s in stopped {
                    let _ = events.send(MediaEvent::SpeakingChanged {
                        session_id: s.session_id,
                        user_id: s.user_id,
                        channels: s.get_channels(),
                        speaking: false,
                    });
                }
            }
        });
    }

    /// Every `energy_interval_ms`, gather the level of each session that moved since the last
    /// report and emit one `ChannelEnergy` per channel those sessions are in.
    fn start_energy_reports(&self) {
        let interval_ms = self.options.energy_interval_ms;
        if interval_ms == 0 {
            return;
        }
        let sessions = self.sessions_by_id.clone();
        let events = self.events.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_millis(interval_ms));
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            // A level older than two report periods means the sender stopped labelling frames.
            let stale_ms = (interval_ms * 2) as i64;
            loop {
                interval.tick().await;
                let mut per_channel: HashMap<ChannelId, (AppId, Vec<(UserId, u8)>)> =
                    HashMap::new();
                for entry in sessions.iter() {
                    let s = entry.value();
                    if !s.is_active() {
                        continue;
                    }
                    let Some(level) = s.take_energy_report(stale_ms, ENERGY_REPORT_MIN_STEP_DB)
                    else {
                        continue;
                    };
                    for channel_id in s.get_channels() {
                        per_channel
                            .entry(channel_id)
                            .or_insert_with(|| (s.app_id, Vec::new()))
                            .1
                            .push((s.user_id, level));
                    }
                }
                for (channel_id, (app_id, levels)) in per_channel {
                    let _ = events.send(MediaEvent::ChannelEnergy {
                        app_id,
                        channel_id,
                        levels,
                    });
                }
            }
        });
    }
}
