use crate::types::Region;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
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
    #[serde(default)]
    pub chat: ChatConfig,
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
                .try_parsing(true)
                .list_separator(",")
                .with_list_parse_key("server.cors_origins")
                .with_list_parse_key("server.trusted_proxies")
                .with_list_parse_key("media.cascade_peers"),
        );
        let cfg = builder.build()?;
        let mut config: AurixConfig = cfg.try_deserialize()?;
        config.normalize();
        config.validate()?;
        Ok(config)
    }

    /// Trim list entries and drop empty ones so `AURIX__SERVER__TRUSTED_PROXIES=""` or
    /// `"a, b"` from the environment behave as expected.
    pub fn normalize(&mut self) {
        fn clean(list: &mut Vec<String>) {
            list.iter_mut().for_each(|s| *s = s.trim().to_string());
            list.retain(|s| !s.is_empty());
        }
        clean(&mut self.server.cors_origins);
        clean(&mut self.server.trusted_proxies);
        clean(&mut self.media.cascade_peers);
        for opt in [
            &mut self.media.external_ip,
            &mut self.turn.external_ip,
            &mut self.media.cascade_secret,
            &mut self.auth.admin_bootstrap_token,
            &mut self.recording.encryption_key,
            &mut self.chat.filter_webhook,
        ] {
            if opt.as_deref().map(|s| s.trim().is_empty()).unwrap_or(false) {
                *opt = None;
            }
        }
    }

    pub fn is_production(&self) -> bool {
        self.server.environment.eq_ignore_ascii_case("production")
            || self.server.environment.eq_ignore_ascii_case("prod")
    }

    pub fn validate(&self) -> anyhow::Result<()> {
        if self.server.api_port == 0 {
            anyhow::bail!("API port must be non-zero");
        }
        if self.media.port == 0 {
            anyhow::bail!("Media port must be non-zero");
        }
        if self.auth.jwt_secret.is_empty() && self.auth.jwt_public_key_path.is_none() {
            anyhow::bail!("Either jwt_secret or jwt_public_key_path must be set");
        }
        if self.auth.action_token_ttl_secs < 1
            || self.auth.action_token_ttl_secs > self.auth.action_token_max_ttl_secs
        {
            anyhow::bail!(
                "auth.action_token_ttl_secs must be within 1..=auth.action_token_max_ttl_secs"
            );
        }
        if self.media.max_participants_per_node == 0 {
            anyhow::bail!("max_participants_per_node must be > 0");
        }
        if self.chat.enabled {
            if self.chat.max_message_bytes == 0 || self.chat.max_message_bytes > 16 * 1024 {
                anyhow::bail!("chat.max_message_bytes must be within 1..=16384");
            }
            if self.chat.messages_per_second == 0 || self.chat.message_burst == 0 {
                anyhow::bail!("chat.messages_per_second and chat.message_burst must be > 0");
            }
            if let Some(url) = &self.chat.filter_webhook {
                if !url.starts_with("http://") && !url.starts_with("https://") {
                    anyhow::bail!("chat.filter_webhook must be an http(s) URL");
                }
            }
        }
        if self.turn.min_port > self.turn.max_port {
            anyhow::bail!("turn.min_port must be <= turn.max_port");
        }
        if self.server.session_resume_grace_secs > self.media.session_timeout_secs {
            anyhow::bail!(
                "server.session_resume_grace_secs must be <= media.session_timeout_secs, otherwise \
                 the media session is reaped before the client can resume"
            );
        }
        for cidr in &self.server.trusted_proxies {
            cidr.parse::<ipnetwork::IpNetwork>().map_err(|e| {
                anyhow::anyhow!("server.trusted_proxies entry '{cidr}' is not a valid CIDR: {e}")
            })?;
        }
        if self.recording.encryption_enabled && self.recording.enabled {
            match &self.recording.encryption_key {
                None => anyhow::bail!(
                    "recording.encryption_key must be set when recording encryption is enabled"
                ),
                Some(k) if k.len() < 32 => {
                    anyhow::bail!("recording.encryption_key must be at least 32 characters")
                }
                _ => {}
            }
        }

        if self.is_production() {
            if self.auth.jwt_public_key_path.is_none() {
                if self.auth.jwt_secret.len() < 32 {
                    anyhow::bail!("auth.jwt_secret must be at least 32 bytes in production");
                }
                if is_placeholder_secret(&self.auth.jwt_secret) {
                    anyhow::bail!(
                        "auth.jwt_secret is a placeholder value; set a real secret in production"
                    );
                }
            }
            if self.turn.enabled {
                if self.turn.auth_secret.len() < 32 {
                    anyhow::bail!("turn.auth_secret must be at least 32 bytes in production");
                }
                if is_placeholder_secret(&self.turn.auth_secret) {
                    anyhow::bail!(
                        "turn.auth_secret is a placeholder value; set a real secret in production"
                    );
                }
                if self.turn.external_ip.is_none() && self.media.external_ip.is_none() {
                    anyhow::bail!(
                        "turn.external_ip (or media.external_ip) must be set in production so TURN URIs are routable"
                    );
                }
            }
            if self.server.cors_origins.iter().any(|o| o == "*") {
                anyhow::bail!("server.cors_origins must not contain '*' in production");
            }
            if self.media.external_ip.is_none() {
                anyhow::bail!("media.external_ip must be set in production so clients receive a routable media address");
            }
            if self.database.url.contains("aurix:aurix@") {
                anyhow::bail!("database.url uses the development default credentials; set real credentials in production");
            }
        }
        Ok(())
    }
}

pub fn is_placeholder_secret(secret: &str) -> bool {
    let s = secret.to_ascii_lowercase();
    s.contains("change-me")
        || s.contains("changeme")
        || s.contains("not-secure")
        || s.contains("example")
        || s.contains("secret-here")
        || s == "secret"
        || s == "password"
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
    /// `development` (default) or `production`. Production enables strict validation.
    #[serde(default = "default_environment")]
    pub environment: String,
    /// CIDR blocks of reverse proxies whose `X-Forwarded-For` headers are trusted.
    #[serde(default)]
    pub trusted_proxies: Vec<String>,
    /// Maximum request body size accepted by the REST API (bytes).
    #[serde(default = "default_max_body_bytes")]
    pub max_body_bytes: usize,
    /// Per-request timeout for REST handlers (seconds).
    #[serde(default = "default_request_timeout_secs")]
    pub request_timeout_secs: u64,
    /// How long a player session survives after its WebSocket drops unexpectedly, waiting for
    /// the client to reconnect with its resume token (seconds). `0` disables session resume.
    #[serde(default = "default_session_resume_grace_secs")]
    pub session_resume_grace_secs: u64,
}

fn default_environment() -> String {
    "development".into()
}
fn default_max_body_bytes() -> usize {
    1024 * 1024
}
fn default_request_timeout_secs() -> u64 {
    30
}
fn default_session_resume_grace_secs() -> u64 {
    30
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
            cors_origins: vec!["http://localhost:3000".into()],
            tls_cert_path: None,
            tls_key_path: None,
            environment: default_environment(),
            trusted_proxies: Vec::new(),
            max_body_bytes: default_max_body_bytes(),
            request_timeout_secs: default_request_timeout_secs(),
            session_resume_grace_secs: default_session_resume_grace_secs(),
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
    /// Lifetime of admin dashboard JWTs.
    #[serde(default = "default_admin_token_ttl_secs")]
    pub admin_token_ttl_secs: i64,
    /// Optional shared secret required by `POST /admin/setup`. When unset, setup is
    /// only permitted while no admin user exists yet (first-run bootstrap).
    #[serde(default)]
    pub admin_bootstrap_token: Option<String>,
    /// Default lifetime of one-time action tokens (`POST /v1/tokens/action`).
    #[serde(default = "default_action_token_ttl_secs")]
    pub action_token_ttl_secs: i64,
    /// Upper bound a caller may request for an action token lifetime.
    #[serde(default = "default_action_token_max_ttl_secs")]
    pub action_token_max_ttl_secs: i64,
    /// When true, WebSocket sessions may only be opened with `login` action tokens and channels
    /// may only be joined with `join` action tokens; the multi-use session JWT from
    /// `POST /v1/tokens` is refused for both (it stays valid for end-user REST calls).
    #[serde(default)]
    pub require_action_tokens: bool,
    pub oauth_enabled: bool,
    pub oauth_client_id: Option<String>,
    pub oauth_client_secret: Option<String>,
    pub oauth_issuer_url: Option<String>,
}

fn default_admin_token_ttl_secs() -> i64 {
    8 * 3600
}

fn default_action_token_ttl_secs() -> i64 {
    90
}

fn default_action_token_max_ttl_secs() -> i64 {
    600
}

impl Default for AuthConfig {
    fn default() -> Self {
        Self {
            jwt_secret: "change-me-in-production-this-is-not-secure".into(),
            jwt_public_key_path: None,
            jwt_private_key_path: None,
            jwt_algorithm: "HS256".into(),
            token_ttl_secs: 3600,
            admin_token_ttl_secs: default_admin_token_ttl_secs(),
            admin_bootstrap_token: None,
            action_token_ttl_secs: default_action_token_ttl_secs(),
            action_token_max_ttl_secs: default_action_token_max_ttl_secs(),
            require_action_tokens: false,
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
    /// Require an HMAC authentication tag on every AURX audio/control packet.
    #[serde(default = "default_true")]
    pub require_packet_auth: bool,
    /// Milliseconds without audio after which a participant stops being "speaking".
    #[serde(default = "default_speaking_timeout_ms")]
    pub speaking_timeout_ms: u64,
    /// Concurrent UDP receive workers for the SFU socket (0 = auto, based on CPU count).
    #[serde(default)]
    pub rx_workers: usize,
    /// Shared secret authenticating cascade (node-to-node relay) traffic.
    #[serde(default)]
    pub cascade_secret: Option<String>,
    /// Statically configured cascade peers (`host:port`). Optional: peers are normally
    /// discovered from the `media_nodes` registry (see `cascade_discovery`).
    #[serde(default)]
    pub cascade_peers: Vec<String>,
    /// Discover cascade peers automatically from healthy nodes in the `media_nodes` table and
    /// forward each channel only to nodes hosting its participants.
    #[serde(default = "default_true")]
    pub cascade_discovery: bool,
    /// How often (ms) the cascade topology is reconciled against the registry.
    #[serde(default = "default_cascade_discovery_interval_ms")]
    pub cascade_discovery_interval_ms: u64,
}

fn default_true() -> bool {
    true
}
fn default_cascade_discovery_interval_ms() -> u64 {
    3000
}
fn default_speaking_timeout_ms() -> u64 {
    400
}

impl Default for MediaConfig {
    fn default() -> Self {
        Self {
            host: "0.0.0.0".into(),
            port: 10000,
            external_ip: None,
            max_participants_per_node: 5000,
            max_channels_per_node: 2000,
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
            require_packet_auth: true,
            speaking_timeout_ms: default_speaking_timeout_ms(),
            rx_workers: 0,
            cascade_secret: None,
            cascade_peers: Vec::new(),
            cascade_discovery: true,
            cascade_discovery_interval_ms: 3000,
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
    /// Public IP advertised to clients as the relay address (defaults to `host`).
    #[serde(default)]
    pub external_ip: Option<String>,
    /// Lifetime of TURN credentials handed out by `POST /v1/turn/credentials`.
    #[serde(default = "default_turn_credential_ttl_secs")]
    pub credential_ttl_secs: i64,
}

fn default_turn_credential_ttl_secs() -> i64 {
    24 * 3600
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
            max_port: 49999,
            allocation_lifetime_secs: 600,
            max_allocations: 10000,
            external_ip: None,
            credential_ttl_secs: default_turn_credential_ttl_secs(),
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
    #[serde(default)]
    pub s3_access_key: Option<String>,
    #[serde(default)]
    pub s3_secret_key: Option<String>,
    /// Require explicit consent from every participant before their audio is written.
    #[serde(default = "default_true")]
    pub require_consent: bool,
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
            s3_access_key: None,
            s3_secret_key: None,
            require_consent: true,
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

/// Lightweight in-game text chat: real-time channel and directed messages plus typing
/// indicators over the control WebSocket. Not a messaging product — no offline delivery, no
/// conversations, no read markers; history is optional and off by default.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ChatConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Upper bound on UTF-8 bytes of `text` plus serialized `metadata` per message.
    #[serde(default = "default_chat_max_message_bytes")]
    pub max_message_bytes: usize,
    /// Anti-flood token bucket per session: sustained rate and burst.
    #[serde(default = "default_chat_messages_per_second")]
    pub messages_per_second: u32,
    #[serde(default = "default_chat_message_burst")]
    pub message_burst: u32,
    /// Minimum interval between typing indicators per session and channel.
    #[serde(default = "default_chat_typing_interval_ms")]
    pub typing_interval_ms: u64,
    /// A server-muted participant cannot send text either (moderation mute silences fully).
    #[serde(default = "default_true")]
    pub server_mute_blocks_text: bool,
    /// Optional filter hook: `POST` with `{app_id, channel_id, from_user_id, to_user_id, text}`;
    /// the reply `{"action":"allow"|"replace"|"block","text":…,"reason":…}` decides.
    #[serde(default)]
    pub filter_webhook: Option<String>,
    #[serde(default = "default_chat_filter_timeout_ms")]
    pub filter_timeout_ms: u64,
    /// Deliver messages when the filter is unreachable (`false` = block them).
    #[serde(default)]
    pub filter_fail_open: bool,
    /// Store messages in `chat_messages` and expose `GET …/messages` history endpoints.
    #[serde(default)]
    pub persist: bool,
    /// Days to keep stored messages (`0` = forever).
    #[serde(default = "default_chat_retention_days")]
    pub retention_days: u32,
}

fn default_chat_max_message_bytes() -> usize {
    1024
}
fn default_chat_messages_per_second() -> u32 {
    2
}
fn default_chat_message_burst() -> u32 {
    10
}
fn default_chat_typing_interval_ms() -> u64 {
    1500
}
fn default_chat_filter_timeout_ms() -> u64 {
    1500
}
fn default_chat_retention_days() -> u32 {
    30
}

impl Default for ChatConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            max_message_bytes: default_chat_max_message_bytes(),
            messages_per_second: default_chat_messages_per_second(),
            message_burst: default_chat_message_burst(),
            typing_interval_ms: default_chat_typing_interval_ms(),
            server_mute_blocks_text: true,
            filter_webhook: None,
            filter_timeout_ms: default_chat_filter_timeout_ms(),
            filter_fail_open: false,
            persist: false,
            retention_days: default_chat_retention_days(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dev_config() -> AurixConfig {
        let mut cfg = AurixConfig::default();
        cfg.server.environment = "development".into();
        cfg
    }

    #[test]
    fn production_rejects_wildcard_cors_and_placeholder_secrets() {
        let mut cfg = dev_config();
        assert!(cfg.validate().is_ok(), "development defaults must validate");

        cfg.server.environment = "production".into();
        cfg.turn.enabled = false;
        cfg.media.external_ip = Some("203.0.113.10".into());
        cfg.database.url = "postgres://prod:s3cret@db/aurix".into();
        cfg.auth.jwt_secret = "change-me-in-production-this-is-not-secure-32bytes!".into();
        cfg.server.cors_origins = vec!["https://game.example.com".into()];
        assert!(cfg
            .validate()
            .unwrap_err()
            .to_string()
            .contains("placeholder"));

        cfg.auth.jwt_secret = "a".repeat(48);
        cfg.server.cors_origins = vec!["*".into()];
        let err = cfg.validate().unwrap_err().to_string();
        assert!(err.contains("cors_origins"), "{err}");

        cfg.server.cors_origins = vec!["https://game.example.com".into()];
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn env_lists_are_comma_separated() {
        std::env::set_var(
            "AURIX__SERVER__CORS_ORIGINS",
            "https://a.example,https://b.example",
        );
        std::env::set_var(
            "AURIX__SERVER__TRUSTED_PROXIES",
            "10.0.0.0/8, 192.168.0.0/16",
        );
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../configs/default");
        let cfg = AurixConfig::load(Some(path)).expect("load with env overrides");
        std::env::remove_var("AURIX__SERVER__CORS_ORIGINS");
        std::env::remove_var("AURIX__SERVER__TRUSTED_PROXIES");
        assert_eq!(
            cfg.server.cors_origins,
            vec!["https://a.example", "https://b.example"]
        );
        assert_eq!(cfg.server.trusted_proxies.len(), 2);
    }
}
