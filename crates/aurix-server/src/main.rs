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
use tracing::{error, info};

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

    let advertised_addr = config
        .media
        .external_ip
        .as_deref()
        .and_then(|ip| ip.parse::<std::net::IpAddr>().ok())
        .map(|ip| std::net::SocketAddr::new(ip, config.media.port));
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
            max_channels_per_session: config.media.max_channels_per_session,
            max_positional_channels_per_session: config.media.max_positional_channels_per_session,
            unfocused_channel_gain: config.media.unfocused_channel_gain,
            session_timeout_secs: config.media.session_timeout_secs,
            cascade_secret: config.media.cascade_secret.clone(),
            cascade_peers: config.media.cascade_peers.clone(),
            advertised_addr,
            downlink_bitrate: config.media.default_bitrate,
            rx_workers: config.media.rx_workers,
        },
    );

    // STT / content analysis pipeline (only when a provider endpoint is configured).
    if let Some(endpoint) = config.moderation.content_analysis_webhook.as_ref() {
        let stt: Arc<dyn aurix_common::tts_stt::SttProvider> =
            Arc::new(aurix_common::tts_stt::WhisperSttProvider::new(endpoint));
        let pipeline = aurix_media::audio_pipeline::AudioAnalysisPipeline::new(
            config.media.default_sample_rate,
            2.0,
            Some(stt),
            Vec::new(),
        );
        sfu.set_audio_pipeline(Arc::new(pipeline));
    }

    let recording = if config.recording.enabled {
        let svc = Arc::new(RecordingService::new(
            pool.clone(),
            config.recording.clone(),
        )?);
        sfu.set_audio_sink(svc.clone());
        Some(svc)
    } else {
        None
    };

    let media_bind = format!("{}:{}", config.media.host, config.media.port);
    sfu.start(&media_bind).await?;
    info!("SFU node started on {}", media_bind);
    let sfu = Arc::new(RwLock::new(sfu));

    let node_address = config
        .media
        .external_ip
        .clone()
        .unwrap_or_else(|| config.server.host.clone());
    {
        let node_info =
            sfu.read()
                .node_info(&node_address, config.media.port, config.server.api_port);
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
        let turn_server = Arc::new(TurnServer::new(&config.turn));
        let cancel = shutdown.clone();
        tasks.spawn(async move {
            tokio::select! {
                r = turn_server.run() => { if let Err(e) = r { error!("TURN server error: {}", e); } }
                _ = cancel.cancelled() => {}
            }
        });
        info!(
            "TURN server started on {}:{} (udp) / {} (tcp)",
            config.turn.host, config.turn.udp_port, config.turn.tcp_port
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
                    sfu.node_info(&node_address, config.media.port, config.server.api_port)
                };
                if let Err(e) = control.nodes.heartbeat(info.id, info).await {
                    error!("Heartbeat failed: {}", e);
                }
            }
        });
    }

    if let Some(rec) = recording.clone() {
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

    // Automatic cascade topology (peers + per-channel routing from the media_nodes registry).
    let cascade_relay = sfu.read().cascade().cloned();
    if let Some(cascade) = cascade_relay.filter(|_| config.media.cascade_discovery) {
        let topology = Arc::new(aurix_control::cascade_topology::CascadeTopology::new(
            pool.clone(),
            control.nodes.clone(),
            node_id,
            cascade,
            sfu.clone(),
        ));
        let events = control.events.clone();
        let interval = std::time::Duration::from_millis(config.media.cascade_discovery_interval_ms);
        let cancel = shutdown.clone();
        tasks.spawn(topology.run(events, interval, cancel));
        info!(
            "Cascade auto-discovery enabled (interval {} ms)",
            config.media.cascade_discovery_interval_ms
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
