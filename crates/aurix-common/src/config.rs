use serde::{Deserialize, Serialize};
use crate::types::Region;
use std::path::PathBuf;

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct AurixConfig {
    pub server: ServerConfig,
    pub database: DatabaseConfig,
    pub redis: RedisConfig,
    pub auth: AuthConfig,
    pub media: MediaConfig,
    pub turn: TurnConfig,
    pub moderation: ModerationConfig,
    pub recording: RecordingConfig,
    pub metrics: MetricsConfig,
    pub tracing: TracingConfig,
    pub rate_limiting: RateLimitConfig,
}

impl AurixConfig {
    pub fn load(path: Option<&str>) -> anyhow::Result<Self> {
        let mut builder = config::Config::builder();
        builder = builder.add_source(config::File::with_name("configs/default").required(false));
        if let Some(p) = path {
            builder = builder.add_source(config::File::with_name(p).required(true));
        }
        builder = builder.add_source(
            config::Environment::with_prefix("AURIX")
                .separator("__")
                .try_parsing(true),
        );
        let cfg = builder.build()?;
        let config: AurixConfig = cfg.try_deserialize()?;
        config.validate()?;
        Ok(config)
    }

    fn validate(&self) -> anyhow::Result<()> {
        if self.server.api_port == 0 {
            anyhow::bail!("API port must be non-zero");
        }
        if self.media.port == 0 {
            anyhow::bail!("Media port must be non-zero");
        }
        if self.auth.jwt_secret.is_empty() && self.auth.jwt_public_key_path.is_none() {
            anyhow::bail!("Either jwt_secret or jwt_public_key_path must be set");
        }
        if self.media.max_participants_per_node == 0 {
            anyhow::bail!("max_participants_per_node must be > 0");
        }
        Ok(())
    }
}

impl Default for AurixConfig {
    fn default() -> Self {
        Self {
            server: ServerConfig::default(),
            database: DatabaseConfig::default(),
            redis: RedisConfig::default(),
            auth: AuthConfig::default(),
            media: MediaConfig::default(),
            turn: TurnConfig::default(),
            moderation: ModerationConfig::default(),
            recording: RecordingConfig::default(),
            metrics: MetricsConfig::default(),
            tracing: TracingConfig::default(),
            rate_limiting: RateLimitConfig::default(),
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ServerConfig {
    pub host: String,
    pub api_port: u16,
    pub ws_port: u16,
    pub region: Region,
    pub node_id: Option<String>,
    pub external_url: String,
    pub cors_origins: Vec<String>,
    pub tls_cert_path: Option<String>,
    pub tls_key_path: Option<String>,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            host: "0.0.0.0".into(),
            api_port: 8080,
            ws_port: 8081,
            region: Region::UsEast,
            node_id: None,
            external_url: "http://localhost:8080".into(),
            cors_origins: vec!["*".into()],
            tls_cert_path: None,
            tls_key_path: None,
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct DatabaseConfig {
    pub url: String,
    pub max_connections: u32,
    pub min_connections: u32,
    pub connect_timeout_secs: u64,
    pub idle_timeout_secs: u64,
    pub run_migrations: bool,
}

impl Default for DatabaseConfig {
    fn default() -> Self {
        Self {
            url: "postgres://aurix:aurix@localhost:5432/aurix".into(),
            max_connections: 100,
            min_connections: 5,
            connect_timeout_secs: 30,
            idle_timeout_secs: 600,
            run_migrations: true,
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct RedisConfig {
    pub url: String,
    pub pool_size: u32,
}

impl Default for RedisConfig {
    fn default() -> Self {
        Self {
            url: "redis://localhost:6379".into(),
            pool_size: 20,
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct AuthConfig {
    pub jwt_secret: String,
    pub jwt_public_key_path: Option<PathBuf>,
    pub jwt_private_key_path: Option<PathBuf>,
    pub jwt_algorithm: String,
    pub token_ttl_secs: i64,
    pub oauth_enabled: bool,
    pub oauth_client_id: Option<String>,
    pub oauth_client_secret: Option<String>,
    pub oauth_issuer_url: Option<String>,
}

impl Default for AuthConfig {
    fn default() -> Self {
        Self {
            jwt_secret: "change-me-in-production-this-is-not-secure".into(),
            jwt_public_key_path: None,
            jwt_private_key_path: None,
            jwt_algorithm: "HS256".into(),
            token_ttl_secs: 3600,
            oauth_enabled: false,
            oauth_client_id: None,
            oauth_client_secret: None,
            oauth_issuer_url: None,
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct MediaConfig {
    pub host: String,
    pub port: u16,
    pub external_ip: Option<String>,
    pub max_participants_per_node: u32,
    pub max_channels_per_node: u32,
    pub default_bitrate: u32,
    pub max_bitrate: u32,
    pub default_sample_rate: u32,
    pub enable_dtx: bool,
    pub enable_fec: bool,
    pub srtp_enabled: bool,
    pub e2ee_enabled: bool,
    pub heartbeat_interval_ms: u64,
    pub session_timeout_secs: u64,
    pub packet_buffer_size: usize,
    pub worker_threads: usize,
}

impl Default for MediaConfig {
    fn default() -> Self {
        Self {
            host: "0.0.0.0".into(),
            port: 10000,
            external_ip: None,
            max_participants_per_node: 50000,
            max_channels_per_node: 10000,
            default_bitrate: 48000,
            max_bitrate: 510000,
            default_sample_rate: 48000,
            enable_dtx: true,
            enable_fec: true,
            srtp_enabled: true,
            e2ee_enabled: false,
            heartbeat_interval_ms: 5000,
            session_timeout_secs: 60,
            packet_buffer_size: 4096,
            worker_threads: 4,
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct TurnConfig {
    pub enabled: bool,
    pub host: String,
    pub udp_port: u16,
    pub tcp_port: u16,
    pub realm: String,
    pub auth_secret: String,
    pub min_port: u16,
    pub max_port: u16,
    pub allocation_lifetime_secs: u64,
    pub max_allocations: u32,
}

impl Default for TurnConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            host: "0.0.0.0".into(),
            udp_port: 3478,
            tcp_port: 3478,
            realm: "aurix.local".into(),
            auth_secret: "change-me-turn-secret".into(),
            min_port: 49152,
            max_port: 65535,
            allocation_lifetime_secs: 600,
            max_allocations: 10000,
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ModerationConfig {
    pub enabled: bool,
    pub max_reports_per_user_per_hour: u32,
    pub auto_mute_on_report_threshold: u32,
    pub profanity_filter_enabled: bool,
    pub content_analysis_webhook: Option<String>,
}

impl Default for ModerationConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            max_reports_per_user_per_hour: 10,
            auto_mute_on_report_threshold: 5,
            profanity_filter_enabled: false,
            content_analysis_webhook: None,
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct RecordingConfig {
    pub enabled: bool,
    pub storage_path: String,
    pub max_recording_duration_secs: u64,
    pub retention_days: u32,
    pub encryption_enabled: bool,
    pub encryption_key: Option<String>,
    pub s3_bucket: Option<String>,
    pub s3_region: Option<String>,
    pub s3_endpoint: Option<String>,
}

impl Default for RecordingConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            storage_path: "/var/lib/aurix/recordings".into(),
            max_recording_duration_secs: 7200,
            retention_days: 90,
            encryption_enabled: true,
            encryption_key: None,
            s3_bucket: None,
            s3_region: None,
            s3_endpoint: None,
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct MetricsConfig {
    pub enabled: bool,
    pub port: u16,
    pub path: String,
}

impl Default for MetricsConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            port: 9090,
            path: "/metrics".into(),
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct TracingConfig {
    pub enabled: bool,
    pub otlp_endpoint: Option<String>,
    pub service_name: String,
    pub log_level: String,
    pub log_format: String,
}

impl Default for TracingConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            otlp_endpoint: None,
            service_name: "aurix".into(),
            log_level: "info".into(),
            log_format: "json".into(),
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct RateLimitConfig {
    pub enabled: bool,
    pub requests_per_second: u32,
    pub burst_size: u32,
    pub channel_joins_per_minute: u32,
    pub messages_per_second: u32,
}

impl Default for RateLimitConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            requests_per_second: 100,
            burst_size: 200,
            channel_joins_per_minute: 30,
            messages_per_second: 10,
        }
    }
}