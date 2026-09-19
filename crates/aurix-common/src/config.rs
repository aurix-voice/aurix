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
    #[serde(default)]
    pub webhooks: WebhooksConfig,
    #[serde(default)]
    pub retention: RetentionConfig,
    #[serde(default)]
    pub stt: SttConfig,
    #[serde(default)]
    pub tts: TtsConfig,
    #[serde(default)]
    pub safety: SafetyConfig,
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
                .with_list_parse_key("media.cascade_peers")
                .with_list_parse_key("webhooks.retry_delays_secs")
                .with_list_parse_key("tts.voices")
                .with_list_parse_key("safety.categories"),
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
            &mut self.server.external_ws_url,
            &mut self.media.external_ip,
            &mut self.turn.external_ip,
            &mut self.media.cascade_secret,
            &mut self.auth.admin_bootstrap_token,
            &mut self.recording.encryption_key,
            &mut self.chat.filter_webhook,
            &mut self.stt.endpoint,
            &mut self.stt.api_key,
            &mut self.stt.model,
            &mut self.stt.language,
            &mut self.tts.endpoint,
            &mut self.tts.api_key,
            &mut self.tts.model,
            &mut self.safety.classifier.endpoint,
            &mut self.safety.classifier.api_key,
            &mut self.safety.classifier.model,
            &mut self.safety.text.lexicon_path,
        ] {
            if opt.as_deref().map(|s| s.trim().is_empty()).unwrap_or(false) {
                *opt = None;
            }
        }
        clean(&mut self.tts.voices);
        clean(&mut self.safety.categories);
        for entry in &mut self.safety.text.lexicon {
            entry.pattern = entry.pattern.trim().to_string();
        }
        self.safety
            .text
            .lexicon
            .retain(|entry| !entry.pattern.is_empty());
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
        if !(0.0..=1.0).contains(&self.media.speaking_energy_threshold) {
            anyhow::bail!("media.speaking_energy_threshold must be within 0.0..=1.0");
        }
        if self.media.energy_interval_ms != 0 && self.media.energy_interval_ms < 50 {
            anyhow::bail!("media.energy_interval_ms must be 0 (off) or >= 50");
        }
        if self.media.quality_interval_ms != 0 && self.media.quality_interval_ms < 500 {
            anyhow::bail!("media.quality_interval_ms must be 0 (off) or >= 500");
        }
        if !(8..=4096).contains(&self.media.tunnel_queue_packets) {
            anyhow::bail!("media.tunnel_queue_packets must be within 8..=4096");
        }
        if !(0.0..=1.0).contains(&self.media.unfocused_channel_gain) {
            anyhow::bail!("media.unfocused_channel_gain must be within 0.0..=1.0");
        }
        if self.media.max_channels_per_session == 0 {
            anyhow::bail!("media.max_channels_per_session must be > 0");
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
        if self.webhooks.enabled {
            if self.webhooks.timeout_ms < 100 || self.webhooks.timeout_ms > 60_000 {
                anyhow::bail!("webhooks.timeout_ms must be within 100..=60000");
            }
            if self.webhooks.retry_delays_secs.len() > 32 {
                anyhow::bail!("webhooks.retry_delays_secs may list at most 32 delays");
            }
            if self.webhooks.poll_interval_ms < 100 {
                anyhow::bail!("webhooks.poll_interval_ms must be >= 100");
            }
            if self.webhooks.batch_size == 0 || self.webhooks.concurrency == 0 {
                anyhow::bail!("webhooks.batch_size and webhooks.concurrency must be > 0");
            }
            if self.webhooks.max_subscriptions_per_app == 0 {
                anyhow::bail!("webhooks.max_subscriptions_per_app must be > 0");
            }
        }
        if self.turn.min_port > self.turn.max_port {
            anyhow::bail!("turn.min_port must be <= turn.max_port");
        }
        if self.retention.enabled {
            if self.retention.batch_size == 0 || self.retention.batch_size > 100_000 {
                anyhow::bail!("retention.batch_size must be within 1..=100000");
            }
            if self.retention.interval_secs < 60 {
                anyhow::bail!("retention.interval_secs must be >= 60");
            }
            let tombstone_secs = i64::from(self.retention.tombstones_days) * 86_400;
            if tombstone_secs
                < self
                    .auth
                    .token_ttl_secs
                    .max(self.auth.action_token_max_ttl_secs)
            {
                anyhow::bail!(
                    "retention.tombstones_days must cover the longest token lifetime \
                     (auth.token_ttl_secs / auth.action_token_max_ttl_secs)"
                );
            }
        }
        if self.server.session_resume_grace_secs > self.media.session_timeout_secs {
            anyhow::bail!(
                "server.session_resume_grace_secs must be <= media.session_timeout_secs, otherwise \
                 the media session is reaped before the client can resume"
            );
        }
        if self.stt.enabled {
            match &self.stt.endpoint {
                Some(url) if url.starts_with("http://") || url.starts_with("https://") => {}
                Some(_) => anyhow::bail!("stt.endpoint must be an http(s) URL"),
                None => anyhow::bail!("stt.enabled requires stt.endpoint"),
            }
            if self.stt.segment_secs < 0.5 || self.stt.segment_secs > 60.0 {
                anyhow::bail!("stt.segment_secs must be within 0.5..=60");
            }
            if self.stt.silence_flush_ms != 0 && self.stt.silence_flush_ms < 100 {
                anyhow::bail!("stt.silence_flush_ms must be 0 (off) or >= 100");
            }
            if self.stt.timeout_ms < 500 || self.stt.timeout_ms > 120_000 {
                anyhow::bail!("stt.timeout_ms must be within 500..=120000");
            }
            if self.stt.max_concurrent_requests == 0 {
                anyhow::bail!("stt.max_concurrent_requests must be > 0");
            }
        }
        if self.tts.enabled {
            match &self.tts.endpoint {
                Some(url) if url.starts_with("http://") || url.starts_with("https://") => {}
                Some(_) => anyhow::bail!("tts.endpoint must be an http(s) URL"),
                None => anyhow::bail!("tts.enabled requires tts.endpoint"),
            }
            if self.tts.max_text_chars == 0 || self.tts.max_text_chars > 10_000 {
                anyhow::bail!("tts.max_text_chars must be within 1..=10000");
            }
            if self.tts.max_audio_secs == 0 || self.tts.max_audio_secs > 600 {
                anyhow::bail!("tts.max_audio_secs must be within 1..=600");
            }
            if self.tts.timeout_ms < 500 || self.tts.timeout_ms > 120_000 {
                anyhow::bail!("tts.timeout_ms must be within 500..=120000");
            }
            if self.tts.max_concurrent_requests == 0 {
                anyhow::bail!("tts.max_concurrent_requests must be > 0");
            }
            if self.tts.max_queued_per_session == 0 || self.tts.max_queued_per_channel == 0 {
                anyhow::bail!(
                    "tts.max_queued_per_session and tts.max_queued_per_channel must be > 0"
                );
            }
            if self.tts.requests_per_minute_per_session == 0 {
                anyhow::bail!("tts.requests_per_minute_per_session must be > 0");
            }
            if self.tts.voices.is_empty() {
                anyhow::bail!("tts.voices must list at least one voice");
            }
            if !self.tts.voices.contains(&self.tts.default_voice) {
                anyhow::bail!("tts.default_voice must be one of tts.voices");
            }
        }
        if self.safety.enabled {
            let s = &self.safety;
            match &s.classifier.endpoint {
                Some(url) if url.starts_with("http://") || url.starts_with("https://") => {}
                Some(_) => anyhow::bail!("safety.classifier.endpoint must be an http(s) URL"),
                None => {}
            }
            if s.classifier.timeout_ms < 200 || s.classifier.timeout_ms > 60_000 {
                anyhow::bail!("safety.classifier.timeout_ms must be within 200..=60000");
            }
            if s.classifier.max_concurrent_requests == 0 {
                anyhow::bail!("safety.classifier.max_concurrent_requests must be > 0");
            }
            if !(0.0..=1.0).contains(&s.incident_threshold) {
                anyhow::bail!("safety.incident_threshold must be within 0..=1");
            }
            if s.risk_half_life_secs < 10 {
                anyhow::bail!("safety.risk_half_life_secs must be >= 10");
            }
            if !(s.risk_elevated > 0.0 && s.risk_high >= s.risk_elevated) {
                anyhow::bail!("safety.risk_elevated must be > 0 and <= safety.risk_high");
            }
            let has_lexicon = s.text.lexicon_path.is_some() || !s.text.lexicon.is_empty();
            if s.classifier.endpoint.is_none() && !has_lexicon {
                anyhow::bail!(
                    "safety.enabled requires safety.classifier.endpoint and/or a lexicon (safety.text.lexicon_path / safety.text.lexicon)"
                );
            }
            if s.voice.enabled && !self.stt.enabled {
                anyhow::bail!(
                    "safety.voice.enabled requires stt.enabled (transcripts are classified)"
                );
            }
            if s.voice.evidence_retention_days == 0 || s.voice.evidence_retention_days > 3650 {
                anyhow::bail!("safety.voice.evidence_retention_days must be within 1..=3650");
            }
            if s.voice.evidence_pre_segments > 20 {
                anyhow::bail!("safety.voice.evidence_pre_segments must be <= 20");
            }
            if s.text.mask_char.chars().count() != 1 {
                anyhow::bail!("safety.text.mask_char must be exactly one character");
            }
            if !(0.0..=1.0).contains(&s.text.block_threshold)
                || s.text.block_threshold < s.incident_threshold
            {
                anyhow::bail!(
                    "safety.text.block_threshold must be within safety.incident_threshold..=1"
                );
            }
            if s.text.context_messages > 50 {
                anyhow::bail!("safety.text.context_messages must be <= 50");
            }
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
        if self.recording.live.enabled {
            if self.recording.live.max_per_channel == 0 || self.recording.live.max_per_app == 0 {
                anyhow::bail!("recording.live.max_per_channel/max_per_app must be >= 1");
            }
            if self.recording.live.queue_frames < 16 {
                anyhow::bail!("recording.live.queue_frames must be >= 16");
            }
            if self.recording.live.connect_timeout_ms < 100 {
                anyhow::bail!("recording.live.connect_timeout_ms must be >= 100");
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
            if let Some(ws) = &self.server.external_ws_url {
                if !ws.trim().starts_with("wss://") {
                    anyhow::bail!("server.external_ws_url must use wss:// in production");
                }
            }
        }
        if let Some(ws) = &self.server.external_ws_url {
            let parsed = url::Url::parse(ws)
                .map_err(|e| anyhow::anyhow!("server.external_ws_url is not a valid URL: {e}"))?;
            if !matches!(parsed.scheme(), "ws" | "wss") || parsed.host_str().is_none() {
                anyhow::bail!("server.external_ws_url must be a ws:// or wss:// URL with a host");
            }
        }
        if let Some(loc) = &self.server.location {
            if !loc.is_valid() {
                anyhow::bail!(
                    "server.location must be latitude within -90..=90 and longitude within -180..=180"
                );
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
    /// Public WebSocket URL of *this* node (`wss://eu1.voice.example.com/ws`), advertised to
    /// clients by region discovery. Derived from `external_url` (`http→ws`, `https→wss`,
    /// explicit port → `ws_port`, path `/ws`) when unset. Point it at the node itself, not at
    /// a shared load balancer: session resume must reach the node that holds the session.
    #[serde(default)]
    pub external_ws_url: Option<String>,
    /// Approximate coordinates of the node for distance-based region selection.
    #[serde(default)]
    pub location: Option<crate::types::GeoLocation>,
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

impl ServerConfig {
    /// Public WebSocket URL advertised to clients: `external_ws_url`, or one derived from
    /// `external_url` (`http→ws`, `https→wss`, an explicit port becomes `ws_port`, path `/ws`).
    /// `None` when `external_url` is not a parseable http(s) URL, or when the derived URL
    /// would be plain `ws://` in production (browsers on https pages cannot use it, and it
    /// would leak tokens): such a node still serves, it is just not offered by discovery.
    pub fn advertised_ws_url(&self, production: bool) -> Option<String> {
        if let Some(explicit) = self.external_ws_url.as_deref() {
            let trimmed = explicit.trim();
            return (!trimmed.is_empty()).then(|| trimmed.to_string());
        }
        let mut url = url::Url::parse(self.external_url.trim()).ok()?;
        let scheme = match url.scheme() {
            "http" if !production => "ws",
            "https" => "wss",
            _ => return None,
        };
        url.host_str()?;
        let had_port = url.port().is_some();
        url.set_scheme(scheme).ok()?;
        if had_port {
            url.set_port(Some(self.ws_port)).ok()?;
        }
        url.set_path("/ws");
        url.set_query(None);
        url.set_fragment(None);
        Some(url.to_string())
    }

    /// Public REST base URL of this node without a trailing slash, if `external_url` parses.
    pub fn advertised_api_url(&self) -> Option<String> {
        let url = url::Url::parse(self.external_url.trim()).ok()?;
        if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
            return None;
        }
        Some(url.to_string().trim_end_matches('/').to_string())
    }
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
            external_ws_url: None,
            location: None,
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
    /// Linear audio level (`0.0..=1.0`, see `protocol::decode_audio_level`) a frame must
    /// reach to count as voice for the speaking indicator. Only applies to frames that carry a
    /// level (AURX `Energy` flag / RTP audio-level extension); unlabeled frames always count.
    #[serde(default = "default_speaking_energy_threshold")]
    pub speaking_energy_threshold: f32,
    /// How often (ms) `ChannelEnergy` level reports are sent to channel members (0 = never).
    #[serde(default = "default_energy_interval_ms")]
    pub energy_interval_ms: u64,
    /// How often (ms) the server evaluates each session's link and sends `NetworkQuality`
    /// (0 = never). Reports go out when the bar count changes and at least every fifth period.
    #[serde(default = "default_quality_interval_ms")]
    pub quality_interval_ms: u64,
    /// Channels one session may be joined to at the same time.
    #[serde(default = "default_max_channels_per_session")]
    pub max_channels_per_session: u32,
    /// Positional channels one session may be joined to at the same time (0 = unlimited).
    #[serde(default = "default_max_positional_channels_per_session")]
    pub max_positional_channels_per_session: u32,
    /// Gain applied to audio from channels other than the one a session focused
    /// (`SetChannelFocus`); `1.0` makes focus a no-op.
    #[serde(default = "default_unfocused_channel_gain")]
    pub unfocused_channel_gain: f32,
    /// Let native AURX sessions negotiate G.711 μ-law (`SetAudioCodec { codec: "pcmu" }`).
    /// Each PCMU session costs one Opus encoder plus one decoder per sender it hears on the
    /// node; disable on CPU-bound nodes.
    #[serde(default = "default_true")]
    pub pcmu_fallback: bool,
    /// Let native AURX sessions carry media over their control WebSocket when UDP is
    /// blocked (one sealed AURX packet per binary frame). Costs TCP head-of-line blocking
    /// for those sessions only; disable to force UDP.
    #[serde(default = "default_true")]
    pub media_tunnel: bool,
    /// Downlink packets queued per tunneled session before the node starts dropping that
    /// session's audio (a stalled TCP connection never blocks the SFU). ~20 ms of audio per
    /// packet per speaker heard.
    #[serde(default = "default_tunnel_queue_packets")]
    pub tunnel_queue_packets: usize,
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
fn default_speaking_energy_threshold() -> f32 {
    0.01
}
fn default_energy_interval_ms() -> u64 {
    200
}
fn default_quality_interval_ms() -> u64 {
    2000
}
fn default_max_channels_per_session() -> u32 {
    10
}
fn default_max_positional_channels_per_session() -> u32 {
    1
}
fn default_unfocused_channel_gain() -> f32 {
    0.5
}

fn default_tunnel_queue_packets() -> usize {
    128
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
            speaking_energy_threshold: default_speaking_energy_threshold(),
            energy_interval_ms: default_energy_interval_ms(),
            quality_interval_ms: default_quality_interval_ms(),
            max_channels_per_session: default_max_channels_per_session(),
            max_positional_channels_per_session: default_max_positional_channels_per_session(),
            unfocused_channel_gain: default_unfocused_channel_gain(),
            pcmu_fallback: true,
            media_tunnel: true,
            tunnel_queue_packets: default_tunnel_queue_packets(),
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
    /// Live audio taps: stream a channel's Opus/PCM frames to an operator service in real time
    /// (WebSocket pull from the node, or push to a `wss://` URL) instead of, or in addition to,
    /// writing files.
    #[serde(default)]
    pub live: LiveStreamConfig,
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
            live: LiveStreamConfig::default(),
        }
    }
}

/// `[recording.live]` — real-time audio streams out of a node. Consent rules are the same as for
/// file recordings (`recording.require_consent`).
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct LiveStreamConfig {
    /// Master switch for `GET /v1/channels/:id/audio/streams/pull` and `POST …/audio/streams`.
    #[serde(default)]
    pub enabled: bool,
    /// Concurrent live streams per channel on one node.
    #[serde(default = "default_live_max_per_channel")]
    pub max_per_channel: u32,
    /// Concurrent live streams per application on one node.
    #[serde(default = "default_live_max_per_app")]
    pub max_per_app: u32,
    /// Frames buffered per stream while the consumer is slow; older frames are dropped (and the
    /// consumer told how many) rather than stalling the media path. 20 ms per frame per talker.
    #[serde(default = "default_live_queue_frames")]
    pub queue_frames: usize,
    /// Hard stop for a live stream (0 = only `recording.max_recording_duration_secs` applies).
    #[serde(default)]
    pub max_duration_secs: u64,
    /// Allow `format=pcm_s16le` (decoded on the node; ~1 Opus decoder per active talker).
    #[serde(default = "default_true")]
    pub allow_pcm: bool,
    /// Allow push streams (the node dials out to an operator WebSocket URL).
    #[serde(default = "default_true")]
    pub push_enabled: bool,
    /// Push URLs may point at private/loopback addresses (default: only outside production).
    #[serde(default)]
    pub allow_private_urls: Option<bool>,
    /// Push URLs must be `wss://` (default: required in production).
    #[serde(default)]
    pub require_tls: Option<bool>,
    #[serde(default = "default_live_connect_timeout_ms")]
    pub connect_timeout_ms: u64,
    /// Reconnect attempts (exponential backoff from 1 s) before a push stream is given up.
    #[serde(default = "default_live_max_reconnects")]
    pub max_reconnects: u32,
}

fn default_live_max_per_channel() -> u32 {
    4
}
fn default_live_max_per_app() -> u32 {
    64
}
fn default_live_queue_frames() -> usize {
    512
}
fn default_live_connect_timeout_ms() -> u64 {
    5000
}
fn default_live_max_reconnects() -> u32 {
    5
}

impl Default for LiveStreamConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            max_per_channel: default_live_max_per_channel(),
            max_per_app: default_live_max_per_app(),
            queue_frames: default_live_queue_frames(),
            max_duration_secs: 0,
            allow_pcm: true,
            push_enabled: true,
            allow_private_urls: None,
            require_tls: None,
            connect_timeout_ms: default_live_connect_timeout_ms(),
            max_reconnects: default_live_max_reconnects(),
        }
    }
}

impl LiveStreamConfig {
    pub fn private_urls_allowed(&self, production: bool) -> bool {
        self.allow_private_urls.unwrap_or(!production)
    }

    pub fn tls_required(&self, production: bool) -> bool {
        self.require_tls.unwrap_or(production)
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

/// Tenant webhooks (`/v1/webhooks`) and the game-server event stream (`GET /v1/events`).
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct WebhooksConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Per-request timeout for one delivery attempt (connect + response headers).
    #[serde(default = "default_webhook_timeout_ms")]
    pub timeout_ms: u64,
    /// Backoff between attempts; an empty list means a single attempt. The first delivery is
    /// immediate, then `retry_delays_secs[n-1]` seconds before attempt `n+1`.
    #[serde(default = "default_webhook_retry_delays")]
    pub retry_delays_secs: Vec<u64>,
    /// How often each node polls the queue for due deliveries.
    #[serde(default = "default_webhook_poll_interval_ms")]
    pub poll_interval_ms: u64,
    /// Deliveries leased per poll and delivered concurrently per node.
    #[serde(default = "default_webhook_batch_size")]
    pub batch_size: u32,
    #[serde(default = "default_webhook_concurrency")]
    pub concurrency: usize,
    /// Queue depth per subscription; when exceeded new events are dropped (the endpoint is
    /// considered dead) and a `webhook.resync` is the way to catch up.
    #[serde(default = "default_webhook_max_pending")]
    pub max_pending_per_subscription: u32,
    /// Hours to keep delivered/failed rows for `GET /v1/webhooks/:id/deliveries` (`0` = forever).
    #[serde(default = "default_webhook_retention_hours")]
    pub retention_hours: u32,
    #[serde(default = "default_webhook_max_subscriptions")]
    pub max_subscriptions_per_app: u32,
    /// Permit endpoints on loopback/private/link-local addresses. Defaults to `true` outside
    /// production and `false` in production (SSRF guard).
    #[serde(default)]
    pub allow_private_urls: Option<bool>,
    /// Require `https://` endpoints. Defaults to the production flag as well.
    #[serde(default)]
    pub require_https: Option<bool>,
    /// Interval of SSE keep-alive comments on `GET /v1/events`.
    #[serde(default = "default_webhook_sse_keepalive_secs")]
    pub sse_keepalive_secs: u64,
}

fn default_webhook_timeout_ms() -> u64 {
    5000
}
fn default_webhook_retry_delays() -> Vec<u64> {
    vec![5, 30, 120, 600, 1800, 3600, 7200]
}
fn default_webhook_poll_interval_ms() -> u64 {
    1000
}
fn default_webhook_batch_size() -> u32 {
    100
}
fn default_webhook_concurrency() -> usize {
    16
}
fn default_webhook_max_pending() -> u32 {
    10_000
}
fn default_webhook_retention_hours() -> u32 {
    72
}
fn default_webhook_max_subscriptions() -> u32 {
    20
}
fn default_webhook_sse_keepalive_secs() -> u64 {
    15
}

impl Default for WebhooksConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            timeout_ms: default_webhook_timeout_ms(),
            retry_delays_secs: default_webhook_retry_delays(),
            poll_interval_ms: default_webhook_poll_interval_ms(),
            batch_size: default_webhook_batch_size(),
            concurrency: default_webhook_concurrency(),
            max_pending_per_subscription: default_webhook_max_pending(),
            retention_hours: default_webhook_retention_hours(),
            max_subscriptions_per_app: default_webhook_max_subscriptions(),
            allow_private_urls: None,
            require_https: None,
            sse_keepalive_secs: default_webhook_sse_keepalive_secs(),
        }
    }
}

impl WebhooksConfig {
    /// Total attempts per delivery: the immediate one plus one per configured delay.
    pub fn max_attempts(&self) -> u32 {
        self.retry_delays_secs.len() as u32 + 1
    }

    pub fn private_urls_allowed(&self, production: bool) -> bool {
        self.allow_private_urls.unwrap_or(!production)
    }

    pub fn https_required(&self, production: bool) -> bool {
        self.require_https.unwrap_or(production)
    }
}

/// Data retention: how long operational rows about players are kept before the hourly sweep
/// removes them. `0` disables a rule (keep forever). Recording and chat retention live in
/// their own sections (`recording.retention_days`, `chat.retention_days`).
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct RetentionConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Closed sessions and their channel memberships.
    #[serde(default = "default_retention_sessions_days")]
    pub sessions_days: u32,
    /// Resolved moderation events (open ones are kept).
    #[serde(default = "default_retention_moderation_days")]
    pub moderation_events_days: u32,
    /// Hash-chained audit log. Trimming cuts the chain at the oldest kept row.
    #[serde(default = "default_retention_audit_days")]
    pub audit_log_days: u32,
    #[serde(default = "default_retention_analytics_days")]
    pub analytics_days: u32,
    /// Erase users (with everything `DELETE /v1/users/:id` removes) that have not connected for
    /// this long. Banned users and users with an open session are never auto-erased.
    #[serde(default)]
    pub inactive_users_days: u32,
    /// How long a deleted user's tombstone blocks tokens minted before the deletion. Must cover
    /// the longest session-token lifetime.
    #[serde(default = "default_retention_tombstones_days")]
    pub tombstones_days: u32,
    /// Rows removed per table per sweep iteration; the sweep repeats until a table is clean.
    #[serde(default = "default_retention_batch_size")]
    pub batch_size: u32,
    /// Seconds between sweeps.
    #[serde(default = "default_retention_interval_secs")]
    pub interval_secs: u64,
}

fn default_retention_sessions_days() -> u32 {
    90
}
fn default_retention_moderation_days() -> u32 {
    365
}
fn default_retention_audit_days() -> u32 {
    0
}
fn default_retention_analytics_days() -> u32 {
    400
}
fn default_retention_tombstones_days() -> u32 {
    30
}
fn default_retention_batch_size() -> u32 {
    5000
}
fn default_retention_interval_secs() -> u64 {
    3600
}

impl Default for RetentionConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            sessions_days: default_retention_sessions_days(),
            moderation_events_days: default_retention_moderation_days(),
            audit_log_days: default_retention_audit_days(),
            analytics_days: default_retention_analytics_days(),
            inactive_users_days: 0,
            tombstones_days: default_retention_tombstones_days(),
            batch_size: default_retention_batch_size(),
            interval_secs: default_retention_interval_secs(),
        }
    }
}

/// Speech-to-text: server-side transcription of channel audio through an OpenAI-compatible
/// `/v1/audio/transcriptions` endpoint (whisper.cpp server, faster-whisper, vLLM, …).
/// Transcripts are delivered live to the participants of channels created with
/// `transcription: true`; nothing is stored.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct SttConfig {
    pub enabled: bool,
    /// Base URL of the transcription server, e.g. `http://whisper:8000`.
    pub endpoint: Option<String>,
    /// Optional bearer token sent as `Authorization: Bearer …`.
    pub api_key: Option<String>,
    /// `model` form field (provider specific; omitted when unset).
    pub model: Option<String>,
    /// ISO-639-1 hint (`language` form field); auto-detect when unset.
    pub language: Option<String>,
    /// Audio per participant accumulated before a transcription request is issued.
    pub segment_secs: f32,
    /// Flush a shorter segment once the participant has been silent for this long
    /// (`0` = only flush on `segment_secs`).
    pub silence_flush_ms: u64,
    /// Segments shorter than this are dropped instead of transcribed.
    pub min_segment_ms: u64,
    pub timeout_ms: u64,
    /// Requests in flight to the STT server per node; further segments are dropped.
    pub max_concurrent_requests: u32,
    /// Include word timings (when the provider returns them) in the client event.
    pub include_words: bool,
}

impl Default for SttConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            endpoint: None,
            api_key: None,
            model: None,
            language: None,
            segment_secs: 3.0,
            silence_flush_ms: 700,
            min_segment_ms: 400,
            timeout_ms: 15_000,
            max_concurrent_requests: 8,
            include_words: false,
        }
    }
}

/// Text-to-speech through an OpenAI-compatible `/v1/audio/speech` endpoint returning WAV
/// (Piper/OpenedAI-Speech, Kokoro-FastAPI, …). The server resamples and Opus-encodes the
/// result and plays it into the channel as ordinary media.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct TtsConfig {
    pub enabled: bool,
    /// Base URL of the speech server, e.g. `http://piper:8000`.
    pub endpoint: Option<String>,
    pub api_key: Option<String>,
    /// `model` JSON field (provider specific; omitted when unset).
    pub model: Option<String>,
    /// Voices clients/operators may request. The first entry is used when none is given
    /// unless `default_voice` says otherwise.
    pub voices: Vec<String>,
    pub default_voice: String,
    /// Allow players to trigger TTS from the client (`TtsSpeak`). REST announcements by the
    /// game server are always allowed when TTS is enabled.
    pub allow_client_requests: bool,
    pub max_text_chars: usize,
    /// Synthesized audio longer than this is truncated.
    pub max_audio_secs: u32,
    pub timeout_ms: u64,
    /// Synthesis requests in flight per node; further requests are rejected.
    pub max_concurrent_requests: u32,
    /// Pending utterances per player session / per channel announcement queue.
    pub max_queued_per_session: u32,
    pub max_queued_per_channel: u32,
    pub requests_per_minute_per_session: u32,
}

impl Default for TtsConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            endpoint: None,
            api_key: None,
            model: None,
            voices: vec!["alloy".to_string()],
            default_voice: "alloy".to_string(),
            allow_client_requests: true,
            max_text_chars: 500,
            max_audio_secs: 30,
            timeout_ms: 15_000,
            max_concurrent_requests: 4,
            max_queued_per_session: 3,
            max_queued_per_channel: 8,
            requests_per_minute_per_session: 10,
        }
    }
}

/// Content safety: toxicity classification of transcripts and chat, lexicon filtering,
/// per-user risk scoring, incident records with evidence, and optional automatic moderation.
///
/// The classifier is any HTTP service speaking the OpenAI `/v1/moderations` request/response
/// shape (`format = "openai_moderation"`) or Aurix's minimal `{text} → {score, categories}`
/// contract (`format = "aurix"`), so self-hosted open models (Detoxify, Perspective-like
/// wrappers, llama-guard behind a small shim) and hosted APIs plug in the same way.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct SafetyConfig {
    pub enabled: bool,
    pub classifier: SafetyClassifierConfig,
    /// Classifier score (`0..=1`) at which a transcript or message becomes an incident.
    pub incident_threshold: f32,
    /// Only these classifier categories count (empty = every category the classifier returns).
    pub categories: Vec<String>,
    /// Half-life of a user's risk score. Each incident adds its severity; the sum decays by
    /// half every interval, so a burst of incidents raises the level, a quiet user recovers.
    pub risk_half_life_secs: u64,
    /// Risk score at which a user is `elevated`.
    pub risk_elevated: f32,
    /// Risk score at which a user is `high`.
    pub risk_high: f32,
    pub voice: VoiceSafetyConfig,
    pub text: TextSafetyConfig,
}

impl Default for SafetyConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            classifier: SafetyClassifierConfig::default(),
            incident_threshold: 0.7,
            categories: Vec::new(),
            risk_half_life_secs: 900,
            risk_elevated: 1.0,
            risk_high: 2.5,
            voice: VoiceSafetyConfig::default(),
            text: TextSafetyConfig::default(),
        }
    }
}

/// Request/response contract of the toxicity classifier endpoint.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SafetyClassifierFormat {
    /// `POST {endpoint}` with `{"input": text, "model"?}`; reply per OpenAI moderations
    /// (`results[0].category_scores`, `results[0].flagged`).
    OpenaiModeration,
    /// `POST {endpoint}` with `{"text", "language"?, "context"?}`; reply
    /// `{"score": 0..1, "categories": {"name": 0..1}, "labels"?: [..]}`.
    Aurix,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct SafetyClassifierConfig {
    /// `http(s)` URL of the classifier. Without it only the lexicon runs.
    pub endpoint: Option<String>,
    pub format: SafetyClassifierFormat,
    /// Sent as `Authorization: Bearer …`; never leaves the node otherwise.
    pub api_key: Option<String>,
    pub model: Option<String>,
    pub timeout_ms: u64,
    pub max_concurrent_requests: u32,
}

impl Default for SafetyClassifierConfig {
    fn default() -> Self {
        Self {
            endpoint: None,
            format: SafetyClassifierFormat::OpenaiModeration,
            api_key: None,
            model: None,
            timeout_ms: 5_000,
            max_concurrent_requests: 8,
        }
    }
}

/// Risk level at which an automatic action fires.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SafetyTrigger {
    /// Never act automatically; incidents are recorded and published only.
    Never,
    /// On every incident.
    Incident,
    /// When the user's risk score reaches `risk_elevated`.
    Elevated,
    /// When the user's risk score reaches `risk_high`.
    High,
}

/// Speech: transcripts of channels with `safety_voice: true` are classified (requires `[stt]`).
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct VoiceSafetyConfig {
    pub enabled: bool,
    /// Store the offending audio segment plus `evidence_pre_segments` preceding segments as an
    /// Ogg/Opus evidence clip (needs `recording.enabled`; stored like recordings — encrypted at
    /// rest and mirrored to object storage when configured).
    pub evidence: bool,
    pub evidence_pre_segments: u32,
    pub evidence_retention_days: u32,
    pub auto_mute: SafetyTrigger,
    pub auto_kick: SafetyTrigger,
}

impl Default for VoiceSafetyConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            evidence: true,
            evidence_pre_segments: 2,
            evidence_retention_days: 30,
            auto_mute: SafetyTrigger::Never,
            auto_kick: SafetyTrigger::Never,
        }
    }
}

/// Text chat: lexicon (dictionary) filter with obfuscation-resistant normalization, then the
/// classifier, then the legacy `chat.filter_webhook` — the first stage that replaces or blocks
/// decides.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct TextSafetyConfig {
    pub enabled: bool,
    /// TOML file with `[[entries]]` (`pattern`, `action = "mask"|"block"|"flag"`, `severity`,
    /// `category`); see `configs/lexicon.example.toml`.
    pub lexicon_path: Option<String>,
    /// Inline entries, merged with the file.
    pub lexicon: Vec<LexiconEntryConfig>,
    /// Character masked words are replaced with.
    pub mask_char: String,
    /// Run the classifier on chat (in addition to the lexicon).
    pub classify: bool,
    /// Classifier score at which a message is blocked (above `incident_threshold` it is
    /// delivered but recorded as an incident).
    pub block_threshold: f32,
    /// Preceding messages from the same channel (or between the same pair) attached to a text
    /// incident as context.
    pub context_messages: u32,
    /// Deliver messages when the classifier is unreachable (`false` = block).
    pub fail_open: bool,
    pub auto_mute: SafetyTrigger,
    pub auto_kick: SafetyTrigger,
}

impl Default for TextSafetyConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            lexicon_path: None,
            lexicon: Vec::new(),
            mask_char: "*".to_string(),
            classify: true,
            block_threshold: 0.9,
            context_messages: 5,
            fail_open: true,
            auto_mute: SafetyTrigger::Never,
            auto_kick: SafetyTrigger::Never,
        }
    }
}

/// One lexicon rule. `pattern` is matched against the normalized text (case-folded, accents
/// and zero-width characters stripped, leet-speak and repeated letters collapsed), on word
/// boundaries unless `substring = true`.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
pub struct LexiconEntryConfig {
    pub pattern: String,
    #[serde(default)]
    pub action: LexiconAction,
    /// Incident severity contributed by a match (`0` = filter only, no incident).
    #[serde(default = "default_lexicon_severity")]
    pub severity: f32,
    #[serde(default = "default_lexicon_category")]
    pub category: String,
    #[serde(default)]
    pub substring: bool,
}

fn default_lexicon_severity() -> f32 {
    0.5
}
fn default_lexicon_category() -> String {
    "profanity".to_string()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LexiconAction {
    /// Replace the matched word with `mask_char`s and deliver.
    #[default]
    Mask,
    /// Reject the message.
    Block,
    /// Deliver unchanged; only record/score.
    Flag,
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
    fn advertised_urls_derive_from_external_url() {
        let mut cfg = dev_config();
        assert_eq!(
            cfg.server.advertised_ws_url(false).as_deref(),
            Some("ws://localhost:8081/ws")
        );
        assert_eq!(
            cfg.server.advertised_api_url().as_deref(),
            Some("http://localhost:8080")
        );
        // Plain http is never advertised as ws:// in production.
        assert_eq!(cfg.server.advertised_ws_url(true), None);

        cfg.server.external_url = "https://eu1.voice.example.com/".into();
        assert_eq!(
            cfg.server.advertised_ws_url(true).as_deref(),
            Some("wss://eu1.voice.example.com/ws")
        );
        assert_eq!(
            cfg.server.advertised_api_url().as_deref(),
            Some("https://eu1.voice.example.com")
        );

        cfg.server.external_ws_url = Some("wss://ws.example.com:4443/voice".into());
        assert_eq!(
            cfg.server.advertised_ws_url(true).as_deref(),
            Some("wss://ws.example.com:4443/voice")
        );
        assert!(cfg.validate().is_ok());

        cfg.server.external_ws_url = Some("https://not-a-ws-url".into());
        assert!(cfg
            .validate()
            .unwrap_err()
            .to_string()
            .contains("external_ws_url"));

        cfg.server.external_ws_url = None;
        cfg.server.location = Some(crate::types::GeoLocation {
            latitude: 91.0,
            longitude: 0.0,
        });
        assert!(cfg
            .validate()
            .unwrap_err()
            .to_string()
            .contains("server.location"));
    }

    #[test]
    fn production_requires_wss_for_explicit_ws_url() {
        let mut cfg = dev_config();
        cfg.server.environment = "production".into();
        cfg.turn.enabled = false;
        cfg.media.external_ip = Some("203.0.113.10".into());
        cfg.database.url = "postgres://prod:s3cret@db/aurix".into();
        cfg.auth.jwt_secret = "a".repeat(48);
        cfg.server.cors_origins = vec!["https://game.example.com".into()];
        cfg.server.external_ws_url = Some("ws://eu1.voice.example.com/ws".into());
        let err = cfg.validate().unwrap_err().to_string();
        assert!(err.contains("wss://"), "{err}");
        cfg.server.external_ws_url = Some("wss://eu1.voice.example.com/ws".into());
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
