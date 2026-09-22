use aurix_api::state::AppState;
use aurix_common::config::AurixConfig;
use aurix_common::types::*;
use aurix_control::ControlPlane;
use aurix_media::{SfuNode, SfuOptions};
use aurix_moderation::ModerationService;
use aurix_recording::RecordingService;
use aurix_turn::TurnServer;
use aurix_ws::WsState;
use clap::Parser;
use parking_lot::RwLock;
use std::sync::Arc;
use tracing::{error, info, warn};

#[derive(Parser)]
#[command(
    name = "aurix-server",
    version,
    about = "Aurix Voice Communication Server"
)]
struct Cli {
    #[arg(short, long, default_value = "configs/default")]
    config: String,

    #[arg(long)]
    migrate_only: bool,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    let config = AurixConfig::load(Some(&cli.config))?;
    init_tracing(&config);

    info!(
        "Starting Aurix server v{} ({})",
        env!("CARGO_PKG_VERSION"),
        config.server.environment
    );
    info!("Region: {:?}", config.server.region);

    let pool = aurix_db::create_pool(&config.database).await?;
    info!("Database connected");

    if !aurix_db::migrations::check_migration_status(&pool).await? {
        anyhow::bail!("Database schema is behind the compiled migrations; run with database.run_migrations=true or `aurix-server --migrate-only`");
    }
    if cli.migrate_only {
        info!("Migrations complete, exiting");
        return Ok(());
    }

    let node_id = config
        .server
        .node_id
        .as_ref()
        .and_then(|s| uuid::Uuid::parse_str(s).ok())
        .map(MediaNodeId::from_uuid)
        .unwrap_or_else(|| MediaNodeId(uuid::Uuid::now_v7()));
    info!("Media node id: {}", node_id);

    let control = Arc::new(ControlPlane::new(config.clone(), pool.clone(), node_id).await?);
    info!("Control plane initialized");
    control.start_lost_node_reaper();

    let mut deactivated_on_recovery = Vec::new();
    match control.sessions.recover_node_state(node_id).await {
        Ok(r) => {
            if r.sessions + r.memberships > 0 {
                info!(
                    "Recovered {} stale sessions and {} memberships from a previous run",
                    r.sessions, r.memberships
                );
            }
            deactivated_on_recovery = r.deactivated_channels;
        }
        Err(e) => error!("Stale session recovery failed: {e}"),
    }

    let (media_v4, media_v6) = config.media.advertised_ips();
    let advertised_addrs: Vec<std::net::SocketAddr> = media_v4
        .map(|ip| std::net::SocketAddr::new(ip.into(), config.media.port))
        .into_iter()
        .chain(media_v6.map(|ip| std::net::SocketAddr::new(ip.into(), config.media.port)))
        .collect();
    let mut sfu = SfuNode::new(
        node_id,
        config.server.region,
        SfuOptions {
            max_participants: config.media.max_participants_per_node,
            max_channels: config.media.max_channels_per_node,
            require_packet_auth: config.media.require_packet_auth,
            speaking_timeout_ms: config.media.speaking_timeout_ms,
            speaking_energy_threshold: config.media.speaking_energy_threshold,
            energy_interval_ms: config.media.energy_interval_ms,
            quality_interval_ms: config.media.quality_interval_ms,
            max_channels_per_session: config.media.max_channels_per_session,
            max_positional_channels_per_session: config.media.max_positional_channels_per_session,
            unfocused_channel_gain: config.media.unfocused_channel_gain,
            session_timeout_secs: config.media.session_timeout_secs,
            cascade_secret: config.media.cascade_secret.clone(),
            cascade_peers: config.media.cascade_peers.clone(),
            cascade: aurix_media::cascade::CascadeOptions {
                tcp_fallback: config.media.cascade_tcp_fallback,
                probe_interval: std::time::Duration::from_millis(
                    config.media.cascade_probe_interval_ms,
                ),
            },
            advertised_addrs,
            downlink_bitrate: config.media.default_bitrate,
            mixer_decoder_complexity: config.media.mixer_decoder_complexity,
            rx_workers: config.media.rx_workers,
            media_tunnel: config.media.media_tunnel,
            tunnel_queue_packets: config.media.tunnel_queue_packets,
            quic: aurix_media::quic::QuicOptions {
                enabled: config.media.quic,
                idle_timeout: std::time::Duration::from_millis(config.media.quic_idle_timeout_ms),
                zero_rtt: config.media.quic_zero_rtt,
                migration: config.media.quic_migration,
                queue_packets: config.media.quic_queue_packets,
                max_connections: if config.media.quic_max_connections == 0 {
                    config.media.max_participants_per_node as usize * 2
                } else {
                    config.media.quic_max_connections
                },
                cert: config
                    .media
                    .quic_cert_path
                    .clone()
                    .zip(config.media.quic_key_path.clone()),
                server_name: config.media.quic_server_name.clone(),
            },
            tls_tunnel: aurix_media::tls::TlsTunnelOptions {
                enabled: config.media.tls_tunnel_port != 0,
                port: config.media.tls_tunnel_port,
                advertise: config.media.tls_tunnel_endpoints(),
                queue_packets: config.media.tls_tunnel_queue_packets,
                max_connections: if config.media.tls_tunnel_max_connections == 0 {
                    config.media.max_participants_per_node as usize * 2
                } else {
                    config.media.tls_tunnel_max_connections
                },
                bind_timeout: std::time::Duration::from_millis(
                    config.media.tls_tunnel_bind_timeout_ms,
                ),
                idle_timeout: std::time::Duration::from_millis(config.media.quic_idle_timeout_ms),
            },
            webtransport: aurix_media::webtransport::WebTransportOptions {
                enabled: config.media.webtransport_port != 0,
                port: config.media.webtransport_port,
                advertise: config.media.webtransport_endpoints(),
                cert: config
                    .media
                    .webtransport_cert_path
                    .clone()
                    .zip(config.media.webtransport_key_path.clone()),
                cert_validity: std::time::Duration::from_secs(
                    u64::from(config.media.webtransport_cert_days) * 86_400,
                ),
                server_name: config.media.quic_server_name.clone(),
                queue_packets: config.media.webtransport_queue_packets,
                max_connections: if config.media.webtransport_max_connections == 0 {
                    config.media.max_participants_per_node as usize * 2
                } else {
                    config.media.webtransport_max_connections
                },
                bind_timeout: std::time::Duration::from_millis(
                    config.media.webtransport_bind_timeout_ms,
                ),
                idle_timeout: std::time::Duration::from_millis(config.media.quic_idle_timeout_ms),
            },
            downlink_mix: config.media.downlink_mix,
            webrtc_participant_streams: config.media.webrtc_participant_streams,
            noise_suppression: config.media.noise_suppression.clone(),
            mos_alert: aurix_media::quality::MosAlertPolicy {
                threshold: config.quality.mos_alert_threshold,
                periods: config.quality.mos_alert_periods,
            },
        },
    );

    // Speech-to-text: channels with `transcription` enabled are segmented per speaker and sent
    // to the provider; results fan out as `channel.transcript` events (never stored here).
    let stt_provider: Option<Arc<dyn aurix_common::tts_stt::SttProvider>> =
        match (config.stt.enabled, config.stt.endpoint.as_ref()) {
            (true, Some(endpoint)) => Some(Arc::new(
                aurix_common::tts_stt::WhisperSttProvider::with_options(
                    endpoint,
                    aurix_common::tts_stt::HttpProviderOptions {
                        api_key: config.stt.api_key.clone(),
                        model: config.stt.model.clone(),
                        timeout: Some(std::time::Duration::from_millis(config.stt.timeout_ms)),
                    },
                    config.stt.language.clone(),
                ),
            )),
            _ => None,
        };
    if let Some(stt) = stt_provider.clone() {
        let mut options = aurix_media::audio_pipeline::PipelineOptions::new(
            config.media.default_sample_rate,
            config.stt.segment_secs,
        );
        options.silence_flush = (config.stt.silence_flush_ms > 0)
            .then(|| std::time::Duration::from_millis(config.stt.silence_flush_ms));
        options.min_segment = std::time::Duration::from_millis(config.stt.min_segment_ms);
        options.max_concurrent_stt = config.stt.max_concurrent_requests as usize;
        let mut pipeline =
            aurix_media::audio_pipeline::AudioAnalysisPipeline::new(options, Some(stt), Vec::new());
        pipeline.set_usage_meter(control.usage.meter());
        let safety = control.safety.clone();
        if safety.voice_enabled() {
            pipeline.set_safety_enabled(true);
            info!("Voice content safety enabled for channels with `safety_voice`");
        }
        let events = control.events.clone();
        let include_words = config.stt.include_words;
        pipeline.set_stt_callback(move |seg| {
            if seg.safety {
                safety.handle_voice(aurix_control::VoiceSegment {
                    app_id: seg.app_id,
                    channel_id: seg.channel_id,
                    user_id: seg.user_id,
                    text: seg.result.text.clone(),
                    language: Some(seg.result.language.clone()).filter(|l| !l.is_empty()),
                    started_at: seg.started_at,
                    audio_ms: seg.audio_ms,
                    pcm: seg.pcm.clone(),
                    sample_rate: seg.sample_rate,
                });
            }
            if !seg.deliver {
                return;
            }
            let words = if include_words {
                seg.result
                    .words
                    .into_iter()
                    .map(|w| aurix_common::protocol::TranscriptWordTiming {
                        word: w.word,
                        start_ms: w.start_ms,
                        end_ms: w.end_ms,
                    })
                    .collect()
            } else {
                Vec::new()
            };
            events.publish(aurix_control::ServerEvent::Transcript {
                app_id: seg.app_id,
                transcript: aurix_common::protocol::Transcript {
                    id: uuid::Uuid::new_v4(),
                    channel_id: seg.channel_id,
                    user_id: seg.user_id,
                    text: seg.result.text,
                    language: Some(seg.result.language).filter(|l| !l.is_empty()),
                    started_at: seg.started_at,
                    duration_ms: seg.audio_ms,
                    words,
                    original: None,
                },
            });
        });
        sfu.set_audio_pipeline(Arc::new(pipeline));
        info!("Speech-to-text enabled");
    }

    let recording = if config.recording.enabled || config.recording.live.enabled {
        let svc = Arc::new(RecordingService::new(
            pool.clone(),
            config.recording.clone(),
            config.is_production(),
            node_id.0,
            stt_provider.clone(),
        )?);
        sfu.set_audio_sink(svc.clone());
        control.users.set_media_purger(svc.clone());
        if svc.storage_enabled() {
            control.safety.set_evidence_store(svc.clone());
            if svc.processing_enabled() {
                if let Err(e) = svc.recover_processing().await {
                    warn!("Recording job recovery failed: {e}");
                }
                info!(
                    "Recording processing enabled (mixdowns; transcripts {})",
                    if svc.transcripts_available() {
                        "on"
                    } else {
                        "off: no [stt] provider"
                    }
                );
            }
        }
        if config.recording.live.enabled {
            info!(
                "Live audio streams enabled (max {}/channel, {}/app, push {})",
                config.recording.live.max_per_channel,
                config.recording.live.max_per_app,
                if config.recording.live.push_enabled {
                    "on"
                } else {
                    "off"
                }
            );
        }
        Some(svc)
    } else {
        None
    };

    let media_bind = aurix_common::net::parse_bind_addr(&config.media.host, config.media.port)
        .map_err(|e| anyhow::anyhow!("media.host: {e}"))?;
    sfu.set_usage_meter(control.usage.meter());
    sfu.start(media_bind).await?;
    info!(
        "SFU node started on {} ({:?})",
        media_bind,
        sfu.family().expect("started")
    );
    control.speech.start(&sfu);
    let sfu = Arc::new(RwLock::new(sfu));
    {
        let sfu = sfu.clone();
        control
            .usage
            .set_pre_flush(move || sfu.read().meter_usage());
    }
    if control.safety.enabled() {
        control
            .safety
            .set_enforcer(Arc::new(aurix_control::ControlPlaneEnforcer::new(
                &control,
                sfu.clone(),
            )));
    }

    let node_address = config
        .media
        .external_ip
        .clone()
        .or_else(|| config.media.external_ipv6.clone())
        .unwrap_or_else(|| config.server.host.clone());
    let node_address_ipv6 = config.media.external_ipv6.clone();
    let advertised_ws_url = config.server.advertised_ws_url(config.is_production());
    let advertised_api_url = config.server.advertised_api_url();
    match &advertised_ws_url {
        _ if config.media.cascade_relay_only => {
            info!("Relay-only cascade hub: not advertised to clients, /ws refuses connections")
        }
        Some(ws) => info!("Advertising WebSocket endpoint {ws} for region discovery"),
        None => warn!(
            "no public wss:// endpoint to advertise (set server.external_ws_url, or an https \
             server.external_url): this node will not be offered by region discovery"
        ),
    }
    let advertise = {
        let ws_url = advertised_ws_url.clone();
        let api_url = advertised_api_url.clone();
        let location = config.server.location;
        let address_ipv6 = node_address_ipv6.clone();
        let relay_only = config.media.cascade_relay_only;
        move |mut info: aurix_common::types::MediaNodeInfo| {
            info.ws_url = ws_url.clone();
            info.api_url = api_url.clone();
            info.location = location;
            info.address_ipv6 = address_ipv6.clone();
            info.relay_only = relay_only;
            if relay_only {
                // Pure inter-regional hub: never offered to clients or picked for failover.
                info.ws_url = None;
                info.capacity = 0;
            }
            info
        }
    };
    {
        let node_info = advertise(sfu.read().node_info(
            &node_address,
            config.media.port,
            config.server.api_port,
        ));
        control.nodes.register_node(node_info).await?;
    }

    let moderation = Arc::new(ModerationService::new(
        pool.clone(),
        config.moderation.content_analysis_webhook.clone(),
    ));

    let app_state = AppState::new(
        control.clone(),
        sfu.clone(),
        moderation.clone(),
        recording.clone(),
    );
    let ws_state = WsState::new(control.clone(), sfu.clone(), recording.clone());
    ws_state.start_fanout();

    let api_router = aurix_api::routes::create_router(app_state);
    let ws_router = axum::Router::new()
        .route("/ws", axum::routing::get(aurix_ws::ws_handler))
        .route(
            "/events",
            axum::routing::get(aurix_ws::event_stream_handler),
        )
        .with_state(ws_state.clone());

    let shutdown = tokio_util::sync::CancellationToken::new();
    let mut tasks = tokio::task::JoinSet::new();

    for handle in control.webhooks.start(shutdown.clone()) {
        tasks.spawn(async move {
            let _ = handle.await;
        });
    }
    if control.webhooks.enabled() {
        info!(
            "Webhooks enabled (retry schedule {:?}s, {} workers)",
            config.webhooks.retry_delays_secs, config.webhooks.concurrency
        );
    }
    // Channels emptied by the crash cleanup above: tell subscribers now that the queue is live.
    for (app_id, channel_id) in deactivated_on_recovery {
        control.channel_emptied(app_id, channel_id).await;
    }

    if config.turn.enabled {
        let turn_server = Arc::new(TurnServer::new(&config.turn, &config.media));
        let cancel = shutdown.clone();
        tasks.spawn(async move {
            tokio::select! {
                r = turn_server.run() => { if let Err(e) = r { error!("TURN server error: {}", e); } }
                _ = cancel.cancelled() => {}
            }
        });
        info!(
            "TURN server started on {} (udp) / {} (tcp)",
            aurix_common::addr::host_port(&config.turn.host, config.turn.udp_port),
            aurix_common::addr::host_port(&config.turn.host, config.turn.tcp_port)
        );
    }

    if config.metrics.enabled {
        let metrics_addr = format!("{}:{}", config.server.host, config.metrics.port);
        let metrics_router = axum::Router::new().route(
            &config.metrics.path,
            axum::routing::get(aurix_metrics::metrics_handler),
        );
        let listener = tokio::net::TcpListener::bind(&metrics_addr).await?;
        info!("Metrics server listening on {}", metrics_addr);
        let cancel = shutdown.clone();
        tasks.spawn(async move {
            let _ = axum::serve(listener, metrics_router)
                .with_graceful_shutdown(async move { cancel.cancelled().await })
                .await;
        });
    }

    // Node heartbeat + gauges.
    {
        let control = control.clone();
        let sfu = sfu.clone();
        let config = config.clone();
        let node_address = node_address.clone();
        let advertise = advertise.clone();
        let cancel = shutdown.clone();
        tasks.spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_millis(
                config.media.heartbeat_interval_ms.max(1000),
            ));
            loop {
                tokio::select! {
                    _ = interval.tick() => {}
                    _ = cancel.cancelled() => return,
                }
                let info = {
                    let sfu = sfu.read();
                    aurix_metrics::ACTIVE_SESSIONS.set(sfu.active_participants() as i64);
                    aurix_metrics::ACTIVE_CHANNELS.set(sfu.active_channels() as i64);
                    advertise(sfu.node_info(
                        &node_address,
                        config.media.port,
                        config.server.api_port,
                    ))
                };
                if let Err(e) = control.nodes.heartbeat(info.id, info).await {
                    error!("Heartbeat failed: {}", e);
                }
                if let Some(redis) = &control.redis {
                    if let Err(e) = redis
                        .beacon_alive(config.cluster.node_lost_after_secs)
                        .await
                    {
                        warn!("Redis liveness beacon failed: {e}");
                    }
                }
            }
        });
    }

    if let Some(rec) = recording.clone().filter(|r| r.live().enabled()) {
        // Lifecycle notices → tenant events (webhooks/SSE, WS disclosure to channel members),
        // fleet directory upkeep and release of the channel pin taken when the stream opened.
        if let Some(mut notices) = rec.live().take_notices() {
            let events = control.events.clone();
            let cancel = shutdown.clone();
            let pool = control.pool.clone();
            let sfu = sfu.clone();
            tasks.spawn(async move {
                loop {
                    let notice = tokio::select! {
                        n = notices.recv() => match n { Some(n) => n, None => return },
                        _ = cancel.cancelled() => return,
                    };
                    match &notice {
                        aurix_recording::live::LiveNotice::Opened(info) => {
                            aurix_recording::live_directory::publish(&pool, info).await;
                        }
                        aurix_recording::live::LiveNotice::Closed { info, .. } => {
                            sfu.read().unpin_channel(&info.channel_id);
                            aurix_recording::live_directory::remove(&pool, info.id).await;
                        }
                    }
                    events.publish(live_stream_event(notice));
                }
            });
        }
        tasks.spawn(rec.live().clone().run_mixer(shutdown.clone()));
        let cancel = shutdown.clone();
        let pool = control.pool.clone();
        tasks.spawn(async move {
            aurix_recording::live_directory::sync(&pool, rec.live()).await;
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(5));
            loop {
                tokio::select! {
                    _ = interval.tick() => {}
                    _ = cancel.cancelled() => return,
                }
                rec.live().enforce_duration_limit();
                aurix_recording::live_directory::sync(&pool, rec.live()).await;
            }
        });
    }

    if let Some(rec) = recording.clone().filter(|r| r.storage_enabled()) {
        if let Some(mut notices) = rec.take_processing_notices() {
            let events = control.events.clone();
            let cancel = shutdown.clone();
            tasks.spawn(async move {
                loop {
                    let notice = tokio::select! {
                        n = notices.recv() => match n { Some(n) => n, None => return },
                        _ = cancel.cancelled() => return,
                    };
                    events.publish(aurix_control::ServerEvent::RecordingProcessed {
                        app_id: notice.app_id,
                        channel_id: notice.channel_id,
                        recording_id: notice.recording_id,
                        job: notice.kind.as_str().to_string(),
                        status: notice.status.to_string(),
                        error: notice.error,
                        timestamp: notice.timestamp,
                    });
                }
            });
        }
        let cancel = shutdown.clone();
        tasks.spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(3600));
            loop {
                tokio::select! {
                    _ = interval.tick() => {}
                    _ = cancel.cancelled() => return,
                }
                if let Err(e) = rec.cleanup_expired().await {
                    error!("Recording cleanup error: {}", e);
                }
            }
        });
    }

    control.usage.start(shutdown.clone());
    if control.usage.config().enabled {
        info!(
            "Usage accounting enabled (flush {}s, aggregate {}s, retention {}d / channels {}d)",
            config.usage.flush_interval_secs,
            config.usage.aggregate_interval_secs,
            config.usage.retention_days,
            config.usage.channel_retention_days
        );
    }

    if control.retention.enabled() {
        tasks.spawn(control.retention.clone().run(shutdown.clone()));
        info!(
            "Retention sweep enabled (every {}s; sessions {}d, moderation {}d, audit {}d, \
             analytics {}d, tombstones {}d, inactive users {}d)",
            config.retention.interval_secs,
            config.retention.sessions_days,
            config.retention.moderation_events_days,
            config.retention.audit_log_days,
            config.retention.analytics_days,
            config.retention.tombstones_days,
            config.retention.inactive_users_days
        );
    }

    // Automatic cascade topology (peers + per-channel routing from the media_nodes registry).
    let cascade_relay = sfu.read().cascade().cloned();
    if let Some(cascade) = cascade_relay.filter(|_| config.media.cascade_discovery) {
        let topology = Arc::new(aurix_control::cascade_topology::CascadeTopology::new(
            pool.clone(),
            control.nodes.clone(),
            aurix_control::cascade_topology::LocalNode {
                id: node_id,
                region: config.server.region,
                relay_only: config.media.cascade_relay_only,
            },
            config.media.cascade_topology,
            cascade,
            sfu.clone(),
            std::time::Duration::from_millis(config.media.cascade_discovery_interval_ms),
        ));
        let events = control.events.clone();
        let interval = std::time::Duration::from_millis(config.media.cascade_discovery_interval_ms);
        let cancel = shutdown.clone();
        tasks.spawn(topology.run(events, interval, cancel));
        info!(
            "Cascade auto-discovery enabled (interval {} ms, topology {:?})",
            config.media.cascade_discovery_interval_ms, config.media.cascade_topology
        );
    }

    let api_addr: std::net::SocketAddr =
        format!("{}:{}", config.server.host, config.server.api_port).parse()?;
    let ws_addr: std::net::SocketAddr =
        format!("{}:{}", config.server.host, config.server.ws_port).parse()?;
    let tls = match (&config.server.tls_cert_path, &config.server.tls_key_path) {
        (Some(cert), Some(key)) => {
            let _ = rustls::crypto::ring::default_provider().install_default();
            let cfg = axum_server::tls_rustls::RustlsConfig::from_pem_file(cert, key)
                .await
                .map_err(|e| anyhow::anyhow!("failed to load TLS cert/key: {e}"))?;
            info!("TLS enabled for API and WebSocket listeners (cert {cert})");
            Some(cfg)
        }
        (None, None) => None,
        _ => anyhow::bail!("server.tls_cert_path and server.tls_key_path must be set together"),
    };

    let api_handle = axum_server::Handle::new();
    let ws_handle = axum_server::Handle::new();
    spawn_http_server(
        &mut tasks,
        "API",
        api_addr,
        api_router.into_make_service_with_connect_info::<std::net::SocketAddr>(),
        tls.clone(),
        api_handle.clone(),
    );
    spawn_http_server(
        &mut tasks,
        "WebSocket",
        ws_addr,
        ws_router.into_make_service_with_connect_info::<std::net::SocketAddr>(),
        tls,
        ws_handle.clone(),
    );
    for (name, handle, addr) in [
        ("API", &api_handle, api_addr),
        ("WebSocket", &ws_handle, ws_addr),
    ] {
        match tokio::time::timeout(std::time::Duration::from_secs(5), handle.listening()).await {
            Ok(Some(bound)) => info!("{name} server listening on {bound}"),
            _ => anyhow::bail!("{name} server failed to bind {addr}"),
        }
    }
    {
        let cancel = shutdown.clone();
        tokio::spawn(async move {
            cancel.cancelled().await;
            let grace = Some(std::time::Duration::from_secs(15));
            api_handle.graceful_shutdown(grace);
            ws_handle.graceful_shutdown(grace);
        });
    }

    shutdown_signal().await;
    info!("Shutdown signal received; draining");
    control.nodes.mark_offline(node_id).await;
    shutdown.cancel();

    // Tell connected players to reconnect elsewhere, then wait (bounded) for tasks to drain.
    ws_state.close_all("server_shutdown");
    if let Some(rec) = recording.as_ref() {
        rec.live().close_all("server_shutdown");
    }
    let drain = tokio::time::timeout(std::time::Duration::from_secs(20), async {
        while tasks.join_next().await.is_some() {}
    });
    if drain.await.is_err() {
        error!("Shutdown drain timed out; aborting remaining tasks");
        tasks.shutdown().await;
    }
    if let Err(e) = control.sessions.recover_node_state(node_id).await {
        error!("Final session cleanup failed: {e}");
    }
    sfu.read().shutdown();
    pool.close().await;
    info!("Aurix server stopped");
    Ok(())
}

fn spawn_http_server(
    tasks: &mut tokio::task::JoinSet<()>,
    name: &'static str,
    addr: std::net::SocketAddr,
    make_service: axum::extract::connect_info::IntoMakeServiceWithConnectInfo<
        axum::Router,
        std::net::SocketAddr,
    >,
    tls: Option<axum_server::tls_rustls::RustlsConfig>,
    handle: axum_server::Handle,
) {
    tasks.spawn(async move {
        let result = match tls {
            Some(tls) => {
                axum_server::bind_rustls(addr, tls)
                    .handle(handle)
                    .serve(make_service)
                    .await
            }
            None => {
                axum_server::bind(addr)
                    .handle(handle)
                    .serve(make_service)
                    .await
            }
        };
        if let Err(e) = result {
            error!("{name} server error: {e}");
        }
    });
}

fn live_stream_event(notice: aurix_recording::live::LiveNotice) -> aurix_control::ServerEvent {
    use aurix_recording::live::LiveNotice;
    match notice {
        LiveNotice::Opened(info) => aurix_control::ServerEvent::LiveStreamStarted {
            app_id: aurix_common::types::AppId::from_uuid(info.app_id),
            channel_id: info.channel_id,
            stream_id: info.id,
            mode: info.mode.as_str().to_string(),
            format: info.format.as_str().to_string(),
            users: info.users,
            timestamp: info.started_at,
        },
        LiveNotice::Closed { info, reason } => aurix_control::ServerEvent::LiveStreamStopped {
            app_id: aurix_common::types::AppId::from_uuid(info.app_id),
            channel_id: info.channel_id,
            stream_id: info.id,
            reason,
            duration_secs: (chrono::Utc::now() - info.started_at)
                .num_milliseconds()
                .max(0) as f64
                / 1000.0,
            frames_sent: info.frames_sent,
            frames_dropped: info.frames_dropped,
            timestamp: chrono::Utc::now(),
        },
    }
}

async fn shutdown_signal() {
    let ctrl_c = tokio::signal::ctrl_c();
    #[cfg(unix)]
    {
        let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("SIGTERM handler");
        tokio::select! {
            _ = ctrl_c => {}
            _ = term.recv() => {}
        }
    }
    #[cfg(not(unix))]
    {
        let _ = ctrl_c.await;
    }
}

fn init_tracing(config: &AurixConfig) {
    use tracing_subscriber::prelude::*;
    use tracing_subscriber::{fmt, EnvFilter};

    let env_filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new(&config.tracing.log_level));

    let fmt_layer = if config.tracing.log_format == "json" {
        fmt::layer().json().flatten_event(true).boxed()
    } else {
        fmt::layer().pretty().boxed()
    };

    let subscriber = tracing_subscriber::registry()
        .with(env_filter)
        .with(fmt_layer);

    if config.tracing.enabled {
        if let Some(ref endpoint) = config.tracing.otlp_endpoint {
            use opentelemetry::trace::TracerProvider as _;
            use opentelemetry_otlp::WithExportConfig;
            let exporter = opentelemetry_otlp::SpanExporter::builder()
                .with_tonic()
                .with_endpoint(endpoint)
                .build()
                .expect("Failed to build OTLP span exporter");
            let resource = opentelemetry_sdk::Resource::builder()
                .with_service_name(config.tracing.service_name.clone())
                .build();
            let provider = opentelemetry_sdk::trace::SdkTracerProvider::builder()
                .with_batch_exporter(exporter)
                .with_resource(resource)
                .build();
            let tracer = provider.tracer("aurix-server");
            opentelemetry::global::set_tracer_provider(provider);

            let telemetry = tracing_opentelemetry::layer().with_tracer(tracer);
            subscriber.with(telemetry).init();
        } else {
            subscriber.init();
        }
    } else {
        subscriber.init();
    }
}
