use aurix_api::state::AppState;
use aurix_common::config::AurixConfig;
use aurix_common::types::*;
use aurix_control::ControlPlane;
use aurix_media::SfuNode;
use aurix_moderation::ModerationService;
use aurix_recording::RecordingService;
use aurix_turn::TurnServer;
use aurix_ws::WsState;
use clap::Parser;
use parking_lot::RwLock;
use std::sync::Arc;
use tracing::{error, info};

#[derive(Parser)]
#[command(name = "aurix-server", version, about = "Aurix Voice Communication Server")]
struct Cli {
    #[arg(short, long, default_value = "configs/default")]
    config: String,

    #[arg(long)]
    migrate_only: bool,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    // Load configuration
    let config = AurixConfig::load(Some(&cli.config))?;

    // Initialize tracing
    init_tracing(&config);

    info!("Starting Aurix server v{}", env!("CARGO_PKG_VERSION"));
    info!("Region: {:?}", config.server.region);

    // Connect to database
    let pool = aurix_db::create_pool(&config.database).await?;
    info!("Database connected");

    if cli.migrate_only {
        info!("Migrations complete, exiting");
        return Ok(());
    }

    // Initialize control plane
    let control = Arc::new(ControlPlane::new(config.clone(), pool.clone()).await?);
    info!("Control plane initialized");

    // Initialize SFU node
    let node_id = config
        .server
        .node_id
        .as_ref()
        .and_then(|s| uuid::Uuid::parse_str(s).ok())
        .map(MediaNodeId::from_uuid)
        .unwrap_or_else(|| MediaNodeId(uuid::Uuid::now_v7()));

    let mut sfu = SfuNode::new(
        node_id,
        config.server.region,
        config.media.max_participants_per_node,
    );

    // ── Audio analysis pipeline (STT/content analysis) ──
    let audio_pipeline = {
        let stt_provider: Option<Arc<dyn aurix_common::tts_stt::SttProvider>> =
            config.moderation.content_analysis_webhook.as_ref().map(|endpoint| {
                Arc::new(aurix_common::tts_stt::WhisperSttProvider::new(endpoint))
                    as Arc<dyn aurix_common::tts_stt::SttProvider>
            });

        let pipeline = aurix_media::audio_pipeline::AudioAnalysisPipeline::new(
            config.media.default_sample_rate,
            2.0, // 2-second analysis window
            stt_provider,
            Vec::new(), // Content analyzers added via plugin interface
        );
        Arc::new(pipeline)
    };
    sfu.set_audio_pipeline(audio_pipeline);

    let media_bind = format!("{}:{}", config.media.host, config.media.port);
    sfu.start(&media_bind).await?;
    info!("SFU node started on {}", media_bind);

    let sfu = Arc::new(RwLock::new(sfu));

    // Register this node with the control plane
    {
        let sfu_read = sfu.read();
        let node_info = sfu_read.node_info(
            config.media.external_ip.as_deref().unwrap_or(&config.server.host),
            config.media.port,
            config.server.api_port,
        );
        control.nodes.register_node(node_info).await?;
    }

    // Initialize moderation service
    let moderation = Arc::new(ModerationService::new(
        pool.clone(),
        config.moderation.content_analysis_webhook.clone(),
    ));

    // Initialize recording service
    let recording = if config.recording.enabled {
        Some(Arc::new(RecordingService::new(
            pool.clone(),
            config.recording.clone(),
        )?))
    } else {
        None
    };

    // Create application state
    let app_state = AppState {
        control: control.clone(),
        sfu: sfu.clone(),
        moderation: moderation.clone(),
        recording: recording.clone(),
    };

    let ws_state = WsState {
        control: control.clone(),
        sfu: sfu.clone(),
        connections: Arc::new(dashmap::DashMap::new()),
    };

    // Build REST API router
    let api_router = aurix_api::routes::create_router(app_state);

    // Build WebSocket router
    let ws_router = axum::Router::new()
        .route("/ws", axum::routing::get(aurix_ws::ws_handler))
        .route("/events", axum::routing::get(aurix_ws::event_stream_handler))
        .with_state(ws_state);

    // Start TURN server
    if config.turn.enabled {
        let turn_server = TurnServer::new(&config.turn);
        tokio::spawn(async move {
            if let Err(e) = turn_server.run().await {
                error!("TURN server error: {}", e);
            }
        });
        info!("TURN server started on {}:{}", config.turn.host, config.turn.udp_port);
    }

    // Start metrics server
    if config.metrics.enabled {
        let metrics_addr = format!("{}:{}", config.server.host, config.metrics.port);
        let metrics_router = axum::Router::new()
            .route(&config.metrics.path, axum::routing::get(aurix_metrics::metrics_handler));

        tokio::spawn(async move {
            let listener = tokio::net::TcpListener::bind(&metrics_addr).await.unwrap();
            info!("Metrics server listening on {}", metrics_addr);
            axum::serve(listener, metrics_router).await.unwrap();
        });
    }

    // Start node heartbeat
    let heartbeat_control = control.clone();
    let heartbeat_sfu = sfu.clone();
    let heartbeat_config = config.clone();
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_millis(
            heartbeat_config.media.heartbeat_interval_ms,
        ));
        loop {
            interval.tick().await;
            let info = {
                let sfu_read = heartbeat_sfu.read();
                let info = sfu_read.node_info(
                    heartbeat_config.media.external_ip.as_deref().unwrap_or(&heartbeat_config.server.host),
                    heartbeat_config.media.port,
                    heartbeat_config.server.api_port,
                );
                // Update metrics while we hold the lock
                aurix_metrics::ACTIVE_SESSIONS.set(sfu_read.active_participants() as i64);
                aurix_metrics::ACTIVE_CHANNELS.set(sfu_read.active_channels() as i64);
                info
            };
            if let Err(e) = heartbeat_control.nodes.heartbeat(info.id, info).await {
                error!("Heartbeat failed: {}", e);
            }
        }
    });

    // Start recording cleanup task
    if let Some(ref rec) = recording {
        let rec_clone = rec.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(3600));
            loop {
                interval.tick().await;
                if let Err(e) = rec_clone.cleanup_expired().await {
                    error!("Recording cleanup error: {}", e);
                }
            }
        });
    }

    // Start API server
    let api_addr = format!("{}:{}", config.server.host, config.server.api_port);
    let api_listener = tokio::net::TcpListener::bind(&api_addr).await?;
    info!("API server listening on {}", api_addr);

    // Start WebSocket server
    let ws_addr = format!("{}:{}", config.server.host, config.server.ws_port);
    let ws_listener = tokio::net::TcpListener::bind(&ws_addr).await?;
    info!("WebSocket server listening on {}", ws_addr);

    // Run both servers
    tokio::select! {
        result = axum::serve(api_listener, api_router) => {
            if let Err(e) = result {
                error!("API server error: {}", e);
            }
        }
        result = axum::serve(ws_listener, ws_router) => {
            if let Err(e) = result {
                error!("WebSocket server error: {}", e);
            }
        }
    }

    Ok(())
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
            use opentelemetry_otlp::WithExportConfig;
            let tracer = opentelemetry_otlp::new_pipeline()
                .tracing()
                .with_exporter(
                    opentelemetry_otlp::new_exporter()
                        .tonic()
                        .with_endpoint(endpoint),
                )
                .with_trace_config(
                    opentelemetry_sdk::trace::config().with_resource(
                        opentelemetry_sdk::Resource::new(vec![
                            opentelemetry::KeyValue::new(
                                "service.name",
                                config.tracing.service_name.clone(),
                            ),
                        ]),
                    ),
                )
                .install_batch(opentelemetry_sdk::runtime::Tokio)
                .expect("Failed to initialize OTLP tracer");

            let telemetry = tracing_opentelemetry::layer().with_tracer(tracer);
            subscriber.with(telemetry).init();
        } else {
            subscriber.init();
        }
    } else {
        subscriber.init();
    }
}