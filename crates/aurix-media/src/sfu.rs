use crate::audio_pipeline::AudioAnalysisPipeline;
use crate::cascade::{CascadeOptions, CascadeRelay};
use crate::cert::MediaCert;
use crate::channel::MediaChannel;
use crate::mix::MixHub;
use crate::mixer::MixerConfig;
use crate::quality::{MosAlertPolicy, QualityTick};
use crate::quic::{QuicLink, QuicOptions, QuicServer};
use crate::router::{MediaEvent, PacketRouter, RouterShared};
use crate::session::{MediaSession, ReceiverPrefs, Transport, DEFAULT_UNFOCUSED_GAIN};
use crate::tls::{TlsLink, TlsTunnelOptions, TlsTunnelServer};
use crate::transport::bind_media_socket;
use crate::tunnel::MediaTunnel;
use crate::webrtc::{WebRtcManager, WebRtcMediaEvent};
use aurix_common::crypto::CryptoProvider;
use aurix_common::error::{AurixError, Result};
use aurix_common::protocol::{channel_id_hash, QuicInfo, TlsTunnelInfo};
use aurix_common::sink::AudioSink;
use aurix_common::types::*;
use aurix_common::usage::{UsageMeter, UsageMetric};
use chrono::Utc;
use dashmap::DashMap;
use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use tokio::sync::{broadcast, mpsc};
use tracing::{error, info, warn};

/// Level change (dB) below which a participant is left out of the next `ChannelEnergy` report.
const ENERGY_REPORT_MIN_STEP_DB: u8 = 3;
/// `NetworkQuality` is sent unconditionally every this many quality periods.
const QUALITY_SUMMARY_EVERY: u64 = 5;
const BARS_LABELS: [&str; 5] = ["1", "2", "3", "4", "5"];

/// Band of the loss a sender has to protect against (the native core turns on FEC from 3 %
/// and DRED from 10 %); a report that crosses a band is sent at once rather than waiting for
/// the periodic one.
fn protect_tier(loss_percent: f32) -> u8 {
    if loss_percent >= 10.0 {
        2
    } else if loss_percent >= 3.0 {
        1
    } else {
        0
    }
}

/// Side effects of leaving a channel that the client must be told about.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ChannelLeft {
    /// `TransmissionMode::Single` pointed at the left channel and fell back to `None`.
    pub transmission_reset: bool,
    /// The left channel was the focused one; focus is now cleared.
    pub focus_reset: bool,
    /// Local members that had the leaver in their roster when the channel scopes presence
    /// by `roster_radius` (`None`: no radius, every member saw them).
    pub roster_observers: Option<Vec<UserId>>,
    /// The leaver was a hidden listener (`audience.hide_listeners`): nobody saw them join,
    /// so nobody is told they left.
    pub hidden: bool,
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
    /// Period of per-session `MediaEvent::NetworkQuality` evaluation (0 = disabled).
    pub quality_interval_ms: u64,
    /// Debounced per-session MOS alerting on those evaluations.
    pub mos_alert: MosAlertPolicy,
    /// Channels a single session may be joined to at once.
    pub max_channels_per_session: u32,
    /// Positional channels a single session may be joined to at once (0 = unlimited).
    pub max_positional_channels_per_session: u32,
    /// Gain for channels other than the one a session focused (see `ReceiverPrefs`).
    pub unfocused_channel_gain: f32,
    pub session_timeout_secs: u64,
    pub cascade_secret: Option<String>,
    pub cascade_peers: Vec<String>,
    pub cascade: CascadeOptions,
    /// Public addresses of the media socket (IPv4 and/or IPv6 with the media port), used as
    /// WebRTC ICE host candidates and for `SessionInitAck.media_addrs`. Empty: the bound
    /// address itself (single-host development).
    pub advertised_addrs: Vec<SocketAddr>,
    pub downlink_bitrate: u32,
    /// libopus decoder complexity of the server mixers (see `MediaConfig::mixer_decoder_complexity`).
    pub mixer_decoder_complexity: u8,
    /// Number of concurrent UDP receive workers (0 = derive from available CPUs).
    pub rx_workers: usize,
    /// Accept AURX media over the control WebSocket (see `MediaConfig::media_tunnel`).
    pub media_tunnel: bool,
    /// Downlink queue depth per tunneled session (see `MediaConfig::tunnel_queue_packets`).
    pub tunnel_queue_packets: usize,
    /// AURX over QUIC datagrams on the media socket (see `MediaConfig::quic*`).
    pub quic: QuicOptions,
    /// AURX over a dedicated TLS/TCP listener (see `MediaConfig::tls_tunnel_*`). Shares the
    /// certificate configured in `quic`.
    pub tls_tunnel: TlsTunnelOptions,
    /// Serve native sessions one server-mixed stream per channel on request
    /// (see `MediaConfig::downlink_mix`).
    pub downlink_mix: bool,
    /// Per-participant WebRTC downlink tracks a browser may negotiate
    /// (see `MediaConfig::webrtc_participant_streams`).
    pub webrtc_participant_streams: u32,
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
            quality_interval_ms: 2000,
            mos_alert: MosAlertPolicy::default(),
            max_channels_per_session: 10,
            max_positional_channels_per_session: 1,
            unfocused_channel_gain: DEFAULT_UNFOCUSED_GAIN,
            session_timeout_secs: 60,
            cascade_secret: None,
            cascade_peers: Vec::new(),
            cascade: CascadeOptions::default(),
            advertised_addrs: Vec::new(),
            downlink_bitrate: 32_000,
            mixer_decoder_complexity: 5,
            rx_workers: 0,
            media_tunnel: true,
            tunnel_queue_packets: 128,
            quic: QuicOptions::default(),
            tls_tunnel: TlsTunnelOptions::default(),
            downlink_mix: true,
            webrtc_participant_streams: 16,
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
    quic: Option<Arc<QuicServer>>,
    tls_tunnel: Option<Arc<TlsTunnelServer>>,
    cascade: Option<Arc<CascadeRelay>>,
    audio_pipeline: Option<Arc<AudioAnalysisPipeline>>,
    audio_sink: Option<Arc<dyn AudioSink>>,
    usage: Option<Arc<UsageMeter>>,
    events: broadcast::Sender<MediaEvent>,
    local_addr: Option<SocketAddr>,
    family: Option<aurix_common::net::BoundFamily>,
    advertised: Vec<SocketAddr>,
    started: bool,
}

impl SfuNode {
    fn mixer_config(&self) -> MixerConfig {
        MixerConfig {
            bitrate_bps: self.options.downlink_bitrate as i32,
            decoder_complexity: self.options.mixer_decoder_complexity,
        }
    }

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
            quic: None,
            tls_tunnel: None,
            cascade: None,
            audio_pipeline: None,
            audio_sink: None,
            usage: None,
            events,
            local_addr: None,
            family: None,
            advertised: Vec::new(),
            started: false,
        }
    }

    pub fn set_audio_pipeline(&mut self, pipeline: Arc<AudioAnalysisPipeline>) {
        self.audio_pipeline = Some(pipeline);
    }

    pub fn audio_pipeline(&self) -> Option<&Arc<AudioAnalysisPipeline>> {
        self.audio_pipeline.as_ref()
    }

    /// Attach a recording/analysis tap. Must be called before `start`.
    pub fn set_audio_sink(&mut self, sink: Arc<dyn AudioSink>) {
        self.audio_sink = Some(sink);
    }

    /// Meters media bytes per application into `meter` (see [`Self::meter_usage`]).
    pub fn set_usage_meter(&mut self, meter: Arc<UsageMeter>) {
        self.usage = Some(meter);
    }

    /// Hands the bytes every live session moved since the previous call to the usage meter.
    /// Called periodically; sessions being torn down are metered a last time in place.
    pub fn meter_usage(&self) {
        let Some(meter) = self.usage.as_ref() else {
            return;
        };
        for entry in self.sessions_by_id.iter() {
            Self::meter_session(meter, entry.value());
        }
    }

    /// Quality summaries of live sessions that gained evaluations since the previous call,
    /// for the periodic `sessions.quality_stats` checkpoint.
    pub fn dirty_quality_summaries(&self) -> Vec<(SessionId, QualitySummary)> {
        self.sessions_by_id
            .iter()
            .filter(|e| e.value().is_active())
            .filter_map(|e| {
                e.value()
                    .take_quality_summary_if_dirty()
                    .map(|s| (*e.key(), s))
            })
            .collect()
    }

    fn meter_session(meter: &UsageMeter, session: &MediaSession) {
        let (rx, tx) = session.take_unmetered_bytes();
        meter.record(session.app_id, None, UsageMetric::MediaBytesIn, rx);
        meter.record(session.app_id, None, UsageMetric::MediaBytesOut, tx);
        let q = session.take_unmetered_quality();
        if q.is_empty() {
            return;
        }
        let app = session.app_id;
        meter.record(app, None, UsageMetric::QualitySamples, q.samples);
        meter.record(app, None, UsageMetric::MosSumMilli, q.mos_milli);
        meter.record(app, None, UsageMetric::RttSumMs, q.rtt_ms);
        meter.record(app, None, UsageMetric::JitterSumMs, q.jitter_ms);
        meter.record(app, None, UsageMetric::LossSumPermille, q.loss_permille);
        meter.record(app, None, UsageMetric::PoorQualitySamples, q.poor_samples);
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

    /// The QUIC media endpoint; `None` until `start` or when disabled.
    pub fn quic(&self) -> Option<&Arc<QuicServer>> {
        self.quic.as_ref()
    }

    /// What native clients need to reach the media socket over QUIC (`None` when QUIC is
    /// disabled or the node is not started) — advertised in `SessionInitAck.quic`.
    pub fn quic_info(&self) -> Option<QuicInfo> {
        self.quic.as_ref().map(|q| q.info().clone())
    }

    /// The TLS tunnel listener; `None` until `start` or when disabled.
    pub fn tls_tunnel(&self) -> Option<&Arc<TlsTunnelServer>> {
        self.tls_tunnel.as_ref()
    }

    /// What native clients need to reach the TLS tunnel (`None` when disabled or the node is
    /// not started) — advertised in `SessionInitAck.tls_tunnel`.
    pub fn tls_tunnel_info(&self) -> Option<TlsTunnelInfo> {
        self.tls_tunnel.as_ref().map(|t| t.info().clone())
    }

    /// Subscribe to media-plane notifications (speaking/mute changes, session binds).
    pub fn subscribe_events(&self) -> broadcast::Receiver<MediaEvent> {
        self.events.subscribe()
    }

    /// Bound UDP address once started.
    pub fn local_addr(&self) -> Option<SocketAddr> {
        self.local_addr
    }

    /// Address families the media socket serves (`None` until started).
    pub fn family(&self) -> Option<aurix_common::net::BoundFamily> {
        self.family
    }

    /// Public media endpoints in advertisement order (IPv4 first, then IPv6); the bound
    /// address when nothing public is configured.
    pub fn advertised_addrs(&self) -> &[SocketAddr] {
        &self.advertised
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

    /// Bind the media socket and start every worker. `bind_addr` `[::]:port` is dual-stack
    /// (IPv4 peers show up with their real IPv4 address), `0.0.0.0:port` IPv4-only, a specific
    /// IPv6 literal IPv6-only.
    pub async fn start(&mut self, bind_addr: SocketAddr) -> Result<()> {
        if self.started {
            return Err(AurixError::Internal("SFU already started".into()));
        }
        let socket = bind_media_socket(bind_addr)
            .map_err(|e| AurixError::Transport(format!("Failed to bind UDP {bind_addr}: {e}")))?;
        let local_addr = socket.local_addr();

        let mut advertised: Vec<SocketAddr> = self
            .options
            .advertised_addrs
            .iter()
            .copied()
            .filter(|a| socket.can_reach(*a))
            .collect();
        if advertised.is_empty() {
            advertised.push(local_addr);
        }
        let (webrtc_event_tx, mut webrtc_event_rx) = mpsc::channel::<WebRtcMediaEvent>(4096);
        let webrtc_mgr = Arc::new(WebRtcManager::new(
            socket.clone(),
            advertised.clone(),
            webrtc_event_tx,
            self.mixer_config(),
            self.options.webrtc_participant_streams,
        ));
        self.webrtc_manager = Some(webrtc_mgr.clone());

        if let Some(secret) = self.options.cascade_secret.clone() {
            let cascade_addr = SocketAddr::new(local_addr.ip(), local_addr.port().wrapping_add(1));
            match CascadeRelay::new(
                cascade_addr,
                self.node_id,
                &secret,
                &self.options.cascade_peers,
                self.options.cascade,
            )
            .await
            {
                Ok(cascade) => {
                    cascade.set_advertised(
                        advertised
                            .iter()
                            .map(|a| SocketAddr::new(a.ip(), a.port().wrapping_add(1)))
                            .collect(),
                    );
                    self.cascade = Some(Arc::new(cascade));
                }
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
            self.options
                .downlink_mix
                .then(|| MixHub::new(socket.clone(), self.mixer_config())),
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
                            seq,
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
                            session.record_uplink(seq, Some(rtp_time), payload.len());
                            if let Err(e) = router
                                .route_webrtc_audio(
                                    &session,
                                    Some(seq as u32),
                                    rtp_time,
                                    payload,
                                    level,
                                )
                                .await
                            {
                                warn!("WebRTC audio route error: {}", e);
                            }
                        }
                        WebRtcMediaEvent::Connected { session_id, remote } => {
                            if let Some(session) =
                                sessions_by_id.get(&session_id).map(|s| s.value().clone())
                            {
                                if let Some(old) = session.set_remote_addr(remote) {
                                    if old != remote {
                                        sessions_by_addr.remove(&old);
                                    }
                                }
                                session.update_heartbeat();
                                sessions_by_addr.insert(remote, session.clone());
                                let _ = events.send(MediaEvent::SessionBound {
                                    session_id,
                                    transport: MediaTransportKind::WebRtc,
                                });
                            }
                            info!("WebRTC session {} connected from {}", session_id, remote);
                        }
                        WebRtcMediaEvent::Disconnected { session_id } => {
                            if let Some(session) =
                                sessions_by_id.get(&session_id).map(|s| s.value().clone())
                            {
                                if let Some(addr) = session.clear_endpoint() {
                                    sessions_by_addr.remove(&addr);
                                }
                            }
                            info!("WebRTC session {} disconnected", session_id);
                        }
                        WebRtcMediaEvent::ParticipantStreams {
                            session_id,
                            streams,
                        } => {
                            let _ = events.send(MediaEvent::ParticipantStreams {
                                session_id,
                                streams,
                            });
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
                        if let Err(e) = router.route_relayed_audio(&sender, packet).await {
                            warn!("Cascade relay route error: {}", e);
                        }
                    });
                }));
        }

        let tls_wanted = self.options.tls_tunnel.enabled;
        let cert = if self.options.quic.enabled || tls_wanted {
            let pair = self
                .options
                .quic
                .cert
                .as_ref()
                .map(|(c, k)| (c.as_path(), k.as_path()));
            Some(MediaCert::load_or_generate(
                pair,
                &self.options.quic.server_name,
            )?)
        } else {
            None
        };

        let quic = if let Some(cert) = cert.as_ref().filter(|_| self.options.quic.enabled) {
            let server = QuicServer::start(socket.clone(), self.options.quic.clone(), cert)?;
            let on_datagram = {
                let router = router.clone();
                move |link: Arc<QuicLink>, data: bytes::Bytes| {
                    let router = router.clone();
                    Box::pin(async move {
                        if let Err(e) = router.route_quic_packet(&data, &link).await {
                            tracing::debug!("Packet dropped from quic#{}: {}", link.id(), e);
                        }
                    })
                        as std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>>
                }
            };
            let on_closed = {
                let router = router.clone();
                move |link: Arc<QuicLink>| {
                    router.quic_link_closed(&link);
                }
            };
            server.clone().run_accept_loop(on_datagram, on_closed);
            Some(server)
        } else {
            None
        };

        let tls_tunnel = if let Some(cert) = cert.as_ref().filter(|_| tls_wanted) {
            let bind = SocketAddr::new(bind_addr.ip(), self.options.tls_tunnel.port);
            let server =
                TlsTunnelServer::bind(bind, &advertised, self.options.tls_tunnel.clone(), cert)
                    .await?;
            let on_packet = {
                let router = router.clone();
                move |link: Arc<TlsLink>, data: Vec<u8>| {
                    let router = router.clone();
                    Box::pin(async move {
                        if let Err(e) = router.route_tls_packet(&data, &link).await {
                            tracing::debug!("Packet dropped from tls#{}: {}", link.id(), e);
                        }
                    })
                        as std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>>
                }
            };
            let on_closed = {
                let router = router.clone();
                move |link: Arc<TlsLink>| {
                    router.tls_link_closed(&link);
                }
            };
            server.clone().run_accept_loop(on_packet, on_closed);
            Some(server)
        } else {
            None
        };

        // UDP receive workers. Several tasks drain the same socket so that per-packet work
        // (HMAC verification, fan-out signing, sends) runs in parallel across the runtime
        // instead of serialising behind a single recv loop and overflowing SO_RCVBUF.
        // Demultiplexing by first byte: AURX magic, WebRTC (STUN/DTLS/RTP ranges), else QUIC
        // (fixed bit set; the ranges are disjoint, see `quic` module docs).
        let workers = Self::rx_worker_count(self.options.rx_workers);
        for worker in 0..workers {
            let router = router.clone();
            let webrtc = webrtc_mgr.clone();
            let quic = quic.clone();
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
                            } else if let Some(quic) = quic
                                .as_ref()
                                .filter(|_| aurix_common::protocol::is_quic_packet(data))
                            {
                                quic.feed(src, data);
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
        self.start_quality_reports();
        if let Some(ref pipeline) = self.audio_pipeline {
            pipeline.spawn_idle_flusher(self.channels.clone());
        }
        self.local_addr = Some(local_addr);
        self.family = Some(socket.family());
        self.advertised = advertised.clone();
        self.router = Some(router);
        self.quic = quic;
        self.tls_tunnel = tls_tunnel;
        self.started = true;
        info!(
            "SFU node {} started in region {:?}, listening on {} ({:?}, advertised {:?})",
            self.node_id,
            self.region,
            local_addr,
            socket.family(),
            advertised
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
        let ssrc = self.crypto.generate_ssrc();
        self.create_session_with_ssrc(session_id, user_id, app_id, display_name, ssrc)
    }

    /// Recreates a session another node was hosting (cross-node failover): the identity the
    /// client already knows (`session_id`, `ssrc`) is kept, everything else — media key,
    /// endpoint, channels, preferences — starts fresh and is restored by the caller.
    /// `audio_seq` is where the participant's downlink audio sequence continues: receivers
    /// keep a per-SSRC anti-replay window, so it must lie above anything the old node sent.
    pub fn adopt_session(
        &self,
        session_id: SessionId,
        user_id: UserId,
        app_id: AppId,
        display_name: String,
        ssrc: u32,
        audio_seq: u32,
    ) -> Result<Arc<MediaSession>> {
        if self.sessions_by_id.contains_key(&session_id) {
            return Err(AurixError::Validation(
                "session already hosted on this node".into(),
            ));
        }
        let session =
            self.create_session_with_ssrc(session_id, user_id, app_id, display_name, ssrc)?;
        session.sequence.store(audio_seq, Ordering::Relaxed);
        Ok(session)
    }

    fn create_session_with_ssrc(
        &self,
        session_id: SessionId,
        user_id: UserId,
        app_id: AppId,
        display_name: String,
        ssrc: u32,
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
        if let Some(addr) = session.clear_endpoint() {
            self.sessions_by_addr.remove(&addr);
        }
        let answer = manager.create_session(offer_sdp, *session_id, session.user_id)?;
        session.set_transport(Transport::WebRtc);
        session.update_heartbeat();
        Ok(answer)
    }

    fn teardown_session(&self, session: &Arc<MediaSession>) {
        // Idempotent: `destroy_session` and a replacing `create_session` for the same user
        // can race on the same `Arc`; whoever removes the id index does the teardown once.
        if self
            .sessions_by_id
            .remove_if(&session.session_id, |_, s| Arc::ptr_eq(s, session))
            .is_none()
        {
            return;
        }
        session.deactivate();
        if let Some(meter) = self.usage.as_ref() {
            Self::meter_session(meter, session);
        }
        if let Some(addr) = session.clear_endpoint() {
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
        if let Some(hub) = self.router.as_ref().and_then(|r| r.mix_hub()) {
            hub.forget_session(&session.session_id, false);
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
        let mut left = ChannelLeft {
            transmission_reset,
            focus_reset,
            roster_observers: None,
            hidden: false,
        };
        let Some(channel) = self.channels.get(channel_id).map(|c| c.value().clone()) else {
            return left;
        };
        left.hidden = channel.is_hidden_listener(&session.user_id);
        left.roster_observers = channel.observers_of(&session.user_id);
        channel.remove_participant(&session.user_id);
        if let Some(ref router) = self.router {
            router.forget_pcmu_downlinks(Some(session.ssrc), Some(channel_id_hash(channel_id)));
            if let Some(hub) = router.mix_hub() {
                hub.forget_receiver(&session.session_id, channel_id);
            }
        }
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

    /// Opens a WebSocket media tunnel for `session_id`. The receiver yields sealed downlink
    /// packets to be written as binary frames on the connection that authenticated as that
    /// session. The tunnel becomes the session's media path only once the client sends an
    /// authenticated `SessionBind` through it (`PacketRouter::route_tunnel_packet`).
    pub fn open_tunnel(
        &self,
        session_id: &SessionId,
    ) -> Result<(Arc<MediaTunnel>, mpsc::Receiver<Vec<u8>>)> {
        if !self.options.media_tunnel {
            return Err(AurixError::AuthorizationDenied(
                "WebSocket media tunnel is disabled on this node".into(),
            ));
        }
        let session = self
            .get_session(session_id)
            .ok_or_else(|| AurixError::SessionNotFound(session_id.to_string()))?;
        if session.transport() == Transport::WebRtc {
            return Err(AurixError::AuthorizationDenied(
                "WebRTC sessions cannot use the AURX tunnel".into(),
            ));
        }
        Ok(MediaTunnel::new(
            *session_id,
            self.options.tunnel_queue_packets,
        ))
    }

    /// The packet router, for feeding tunneled AURX frames without holding the SFU lock across
    /// the (async) routing.
    pub fn packet_router(&self) -> Result<Arc<PacketRouter>> {
        self.router
            .as_ref()
            .cloned()
            .ok_or_else(|| AurixError::Internal("SFU not started".into()))
    }

    /// The connection owning `tunnel` closed: unbind the session if it was still on this
    /// tunnel (a later UDP bind or a newer tunnel is left alone). Returns whether it was.
    pub fn close_tunnel(&self, tunnel: &MediaTunnel) -> bool {
        self.get_session(&tunnel.session_id())
            .is_some_and(|s| s.clear_tunnel(tunnel))
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

    /// Stores a member's pose; returns roster transitions for local observers
    /// (`roster_radius` channels only).
    pub fn update_position(
        &self,
        user_id: &UserId,
        channel_id: &ChannelId,
        position: Position3D,
        orientation: Orientation3D,
    ) -> Vec<crate::channel::RosterChange> {
        match self.channels.get(channel_id) {
            Some(channel) => channel.update_position(user_id, position, orientation),
            None => Vec::new(),
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

    /// `SetDownlinkMode`: how a native session receives channel audio. `Mixed` needs
    /// `media.downlink_mix`; switching back to `Streams` drops the session's mixed streams
    /// (channels with `audience.mix_for_listeners` keep mixing for a listener regardless).
    pub fn set_downlink_mode(&self, session_id: &SessionId, mode: DownlinkMode) -> Result<()> {
        let session = self
            .get_session(session_id)
            .ok_or_else(|| AurixError::SessionNotFound(session_id.to_string()))?;
        if mode == DownlinkMode::Mixed && !self.options.downlink_mix {
            return Err(AurixError::Validation(
                "server-side downlink mixing is disabled on this node".into(),
            ));
        }
        session.set_downlink_mode(mode)?;
        if mode == DownlinkMode::Streams {
            if let Some(hub) = self.router.as_ref().and_then(|r| r.mix_hub()) {
                hub.forget_session(session_id, true);
            }
        }
        Ok(())
    }

    /// Whether this node serves server-mixed downlinks (`SessionInitAck.downlink_mix`).
    pub fn downlink_mix_enabled(&self) -> bool {
        self.options.downlink_mix
    }

    /// Per-participant WebRTC downlink tracks a browser may negotiate
    /// (`SessionInitAck.webrtc_participant_streams`).
    pub fn webrtc_participant_streams(&self) -> u32 {
        self.options.webrtc_participant_streams
    }

    /// Gain applied to voices of unfocused channels (`SessionInitAck.unfocused_channel_gain`).
    pub fn unfocused_channel_gain(&self) -> f32 {
        self.options.unfocused_channel_gain
    }

    /// `SetParticipantStreams`: participants a WebRTC session keeps on their own downlink
    /// track while heard. Rejected for sessions without a WebRTC media path.
    pub fn set_participant_streams(
        &self,
        session_id: &SessionId,
        pinned: Vec<UserId>,
    ) -> Result<()> {
        let session = self
            .get_session(session_id)
            .ok_or_else(|| AurixError::SessionNotFound(session_id.to_string()))?;
        if session.transport() != Transport::WebRtc {
            return Err(AurixError::Validation(
                "per-participant downlink tracks are a WebRTC feature".into(),
            ));
        }
        if pinned.len() > self.options.webrtc_participant_streams as usize {
            return Err(AurixError::Validation(format!(
                "at most {} participants can be pinned",
                self.options.webrtc_participant_streams
            )));
        }
        self.webrtc_manager
            .as_ref()
            .ok_or_else(|| AurixError::Validation("WebRTC is not available".into()))?
            .set_pinned(session_id, pinned)
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
    /// Applies an operator edit to a live channel. Returns the sessions on this node that
    /// are in the channel (they need the new `AudioPolicy`), or `None` if the channel is not
    /// live here.
    pub fn update_channel_config(
        &self,
        channel_id: &ChannelId,
        app_id: &AppId,
        config: ChannelConfig,
    ) -> Option<Vec<Arc<MediaSession>>> {
        let channel = self
            .get_channel(channel_id)
            .filter(|c| c.app_id == *app_id)?;
        channel.update_config(config);
        Some(channel.get_all_participants())
    }
    /// Encoder policy for one sender: the merge over every channel the session is in.
    pub fn session_audio_policy(&self, session: &MediaSession) -> AudioPolicy {
        AudioPolicy::merge_all(
            session
                .get_channels()
                .iter()
                .filter_map(|id| self.get_channel(id))
                .map(|c| c.audio_policy()),
        )
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
            .route_webrtc_audio(&session, None, rtp_time, payload, level)
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
            address_ipv6: None,
            media_port,
            api_port,
            cascade_port: self.cascade.as_ref().map(|c| c.local_addr().port()),
            ws_url: None,
            api_url: None,
            location: None,
            active_channels: self.active_channels(),
            active_participants: self.active_participants(),
            cpu_usage: 0.0,
            memory_usage: 0.0,
            bandwidth_in_mbps: 0.0,
            bandwidth_out_mbps: 0.0,
            healthy: true,
            last_heartbeat: Utc::now(),
            version: env!("CARGO_PKG_VERSION").to_string(),
            registered_at: None,
            capacity: self.options.max_participants,
            relay_only: false,
            drain: None,
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
                    if let Some(addr) = session.clear_endpoint() {
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
        if let Some(quic) = &self.quic {
            quic.shutdown();
        }
        if let Some(tls) = &self.tls_tunnel {
            tls.shutdown();
        }
        if let Some(cascade) = &self.cascade {
            cascade.shutdown();
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

    /// Every `quality_interval_ms`, close each bound session's uplink interval, merge it with
    /// the client's report and the worst downlink loss its local receivers reported, fold it
    /// into the session's summary and emit `NetworkQuality` when the bar count moved, the
    /// MOS alert state flipped, the loss a sender must protect against crossed a tier, or at
    /// least every fifth period so a client that missed one still converges. Sessions
    /// without a media path yet are not rated. Node-wide Prometheus gauges/histograms are
    /// refreshed from the same pass.
    fn start_quality_reports(&self) {
        let interval_ms = self.options.quality_interval_ms;
        if interval_ms == 0 {
            return;
        }
        let mos_policy = self.options.mos_alert;
        let period_secs = interval_ms as f64 / 1000.0;
        let sessions = self.sessions_by_id.clone();
        let channels = self.channels.clone();
        let events = self.events.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_millis(interval_ms));
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            let mut tick: u64 = 0;
            let mut protect_tiers: HashMap<SessionId, u8> = HashMap::new();
            let mut seen: HashSet<SessionId> = HashSet::new();
            loop {
                interval.tick().await;
                tick = tick.wrapping_add(1);
                let summary = tick.is_multiple_of(QUALITY_SUMMARY_EVERY);
                let mut by_bars = [0i64; 5];
                let mut degraded = 0i64;
                let worst_by_channel: HashMap<ChannelId, [Option<(UserId, f32)>; 2]> = channels
                    .iter()
                    .map(|c| (*c.key(), c.value().worst_receiver_loss()))
                    .collect();
                seen.clear();
                for entry in sessions.iter() {
                    let s = entry.value();
                    if !s.is_active() || !s.is_bound() {
                        continue;
                    }
                    let receivers_loss = s
                        .channels
                        .read()
                        .iter()
                        .filter_map(|id| worst_by_channel.get(id))
                        .filter_map(|worst| match worst {
                            [Some((u, _)), second] if *u == s.user_id => second.map(|(_, l)| l),
                            [first, _] => first.map(|(_, l)| l),
                        })
                        .fold(0.0f32, f32::max);
                    let QualityTick {
                        quality,
                        bars_changed,
                        transition,
                    } = s.refresh_network_quality(period_secs, mos_policy, receivers_loss);
                    let tier = protect_tier(quality.protect_loss_percent());
                    let tier_changed = protect_tiers.insert(s.session_id, tier) != Some(tier);
                    seen.insert(s.session_id);
                    by_bars[usize::from(quality.bars.clamp(1, 5)) - 1] += 1;
                    if s.is_mos_alerting() {
                        degraded += 1;
                    }
                    aurix_metrics::SESSION_MOS.observe(f64::from(quality.mos));
                    aurix_metrics::UPLINK_LOSS_PERCENT
                        .observe(f64::from(quality.uplink_loss_percent));
                    aurix_metrics::UPLINK_JITTER_MS.observe(f64::from(quality.uplink_jitter_ms));
                    if bars_changed || transition.is_some() || tier_changed || summary {
                        let _ = events.send(MediaEvent::NetworkQuality {
                            session_id: s.session_id,
                            app_id: s.app_id,
                            user_id: s.user_id,
                            quality,
                            transition,
                        });
                    }
                }
                protect_tiers.retain(|id, _| seen.contains(id));
                for (i, n) in by_bars.iter().enumerate() {
                    aurix_metrics::SESSIONS_BY_BARS
                        .with_label_values(&[BARS_LABELS[i]])
                        .set(*n);
                }
                aurix_metrics::SESSIONS_MOS_DEGRADED.set(degraded);
            }
        });
    }
}
