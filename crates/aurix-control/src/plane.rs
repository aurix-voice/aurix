use crate::action_tokens::{ActionTokenService, PendingClaim};
use crate::analytics::AnalyticsCollector;
use crate::block_manager::BlockManager;
use crate::channel_manager::ChannelManager;
use crate::event_bus::EventBus;
use crate::node_manager::NodeManager;
use crate::redis_store::RedisStore;
use crate::session_manager::SessionManager;
use aurix_auth::admin::AdminAuthService;
use aurix_auth::{AnyToken, ApiKeyService, JwtService, RbacService, ValidatedToken};
use aurix_common::audit::AuditLogger;
use aurix_common::config::AurixConfig;
use aurix_common::error::{AurixError, Result};
use aurix_common::rate_limit::RateLimiter;
use aurix_common::types::*;
use aurix_db::DbPool;
use std::sync::Arc;

pub struct ControlPlane {
    pub node_id: MediaNodeId,
    pub config: Arc<AurixConfig>,
    pub pool: DbPool,
    pub jwt: Arc<JwtService>,
    pub rbac: Arc<RbacService>,
    pub api_keys: Arc<ApiKeyService>,
    pub admin_auth: Arc<AdminAuthService>,
    pub nodes: Arc<NodeManager>,
    pub channels: Arc<ChannelManager>,
    pub sessions: Arc<SessionManager>,
    pub blocks: Arc<BlockManager>,
    pub action_tokens: Arc<ActionTokenService>,
    pub events: Arc<EventBus>,
    pub audit: Arc<AuditLogger>,
    pub rate_limiter: Arc<RateLimiter>,
    pub redis: Option<Arc<RedisStore>>,
}

impl ControlPlane {
    pub async fn new(config: AurixConfig, pool: DbPool, node_id: MediaNodeId) -> Result<Self> {
        config
            .validate()
            .map_err(|e| AurixError::InvalidConfiguration(e.to_string()))?;
        let jwt = Arc::new(JwtService::new(&config.auth)?);
        let rbac = Arc::new(RbacService::new());
        let api_keys = Arc::new(ApiKeyService::new(pool.clone()));
        let admin_auth = Arc::new(
            AdminAuthService::new(pool.clone(), config.auth.jwt_secret.clone())
                .with_token_ttl(config.auth.admin_token_ttl_secs)
                .with_bootstrap_token(config.auth.admin_bootstrap_token.clone()),
        );
        let nodes = Arc::new(NodeManager::new(pool.clone()));
        if let Err(e) = nodes.load_from_db().await {
            tracing::warn!("Could not load media nodes from database: {e}");
        }
        let channels = Arc::new(ChannelManager::new(pool.clone()));
        let sessions = Arc::new(SessionManager::new(pool.clone()));
        let blocks = Arc::new(BlockManager::new(pool.clone()));
        let events = Arc::new(EventBus::new(10000));

        let (audit_tx, audit_rx) = flume::bounded(10000);
        let audit = Arc::new(AuditLogger::new(audit_tx));

        // Audit log writer
        let audit_pool = pool.clone();
        tokio::spawn(async move {
            while let Ok(entry) = audit_rx.recv_async().await {
                let row = aurix_db::models::AuditLogRow {
                    id: entry.id,
                    app_id: entry.app_id.map(|a| a.0),
                    actor_id: entry.actor_id.0,
                    action: serde_json::to_value(&entry.action)
                        .ok()
                        .and_then(|v| v.as_str().map(str::to_owned))
                        .unwrap_or_else(|| format!("{:?}", entry.action).to_lowercase()),
                    target_type: entry.target_type,
                    target_id: entry.target_id,
                    details: entry.details,
                    ip_address: entry.ip_address,
                    previous_hash: entry.previous_hash,
                    hash: entry.hash,
                    created_at: entry.timestamp,
                };
                if let Err(e) = aurix_db::queries::insert_audit_log(&audit_pool, &row).await {
                    tracing::error!("Failed to write audit log: {}", e);
                }
            }
        });

        let rate_limiter = Arc::new(RateLimiter::new(
            config.rate_limiting.requests_per_second,
            config.rate_limiting.burst_size,
        ));
        rate_limiter.start_cleanup_task();

        // Redis
        let redis = match aurix_common::redis_pool::create_redis_client(&config.redis).await {
            Ok(client) => match RedisStore::connect(client, node_id).await {
                Ok(store) => {
                    tracing::info!("Redis connected");
                    Some(Arc::new(store))
                }
                Err(e) => {
                    if config.is_production() {
                        return Err(AurixError::Redis(format!(
                            "Redis is required in production: {e}"
                        )));
                    }
                    tracing::warn!("Redis not available (non-fatal in development): {e}");
                    None
                }
            },
            Err(e) => {
                if config.is_production() {
                    return Err(AurixError::Redis(format!(
                        "Redis is required in production: {e}"
                    )));
                }
                tracing::warn!("Redis not available (non-fatal in development): {}", e);
                None
            }
        };

        // Node health checker
        let nodes_clone = nodes.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(15));
            loop {
                interval.tick().await;
                nodes_clone.check_node_health().await;
            }
        });

        // Analytics collector
        let analytics = AnalyticsCollector::new(pool.clone());
        analytics.start();

        // Cross-node event replication (origin-tagged, loop-free)
        if let Some(ref redis_store) = redis {
            redis_store.start_event_replication(events.clone());
        }

        // Expire time-limited user bans
        let ban_pool = pool.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(60));
            loop {
                interval.tick().await;
                match aurix_db::queries::expire_user_bans(&ban_pool).await {
                    Ok(n) if n > 0 => tracing::info!("Expired {n} user bans"),
                    Ok(_) => {}
                    Err(e) => tracing::warn!("Ban expiry sweep failed: {e}"),
                }
            }
        });

        let action_tokens = Arc::new(ActionTokenService::new(
            jwt.clone(),
            redis.clone(),
            &config.auth,
        ));

        Ok(Self {
            node_id,
            config: Arc::new(config),
            pool,
            jwt,
            rbac,
            api_keys,
            admin_auth,
            nodes,
            channels,
            sessions,
            action_tokens,
            blocks,
            events,
            audit,
            rate_limiter,
            redis,
        })
    }

    /// End-user REST authentication. Accepts the session JWT and — because a client that
    /// opened its session with a `login` action token holds nothing else — `login` action
    /// tokens as well (not consumed here: REST reads are idempotent and the token stays
    /// short-lived). Any other action token is refused.
    pub fn validate_token(&self, token: &str) -> Result<ValidatedToken> {
        match self.jwt.validate_any(token)? {
            AnyToken::Session(v) => Ok(v),
            AnyToken::Action(a) if a.action == ActionKind::Login => Ok(a.as_session()),
            AnyToken::Action(a) => Err(AurixError::AuthorizationDenied(format!(
                "Action token authorises '{}', not API access",
                a.action
            ))),
        }
    }

    /// Validate an admin JWT and confirm the account is still active.
    pub async fn authenticate_admin(&self, token: &str) -> Result<AdminContext> {
        let ctx = self.admin_auth.validate_admin_token(token)?;
        self.admin_auth.ensure_active(ctx.admin_id).await?;
        Ok(ctx)
    }

    /// Validates the credential a client opens a WebSocket session with, runs the ban and
    /// rate-limit checks and picks a media node. `one_time` is set when the credential was a
    /// `login` action token; the caller must [`consume_login`](Self::consume_login) it once
    /// it decides to open a *fresh* session (a resume of the session that token already
    /// opened does not consume it again).
    pub async fn authenticate_session(
        &self,
        token: &str,
        ip_address: &str,
    ) -> Result<(ValidatedToken, MediaNodeInfo, Option<PendingClaim>)> {
        let (validated, one_time) = match self.jwt.validate_any(token)? {
            AnyToken::Session(v) => {
                if self.action_tokens.required() {
                    return Err(AurixError::ActionTokenRequired(
                        "This server only accepts 'login' action tokens for sessions".into(),
                    ));
                }
                (v, None)
            }
            AnyToken::Action(a) if a.action == ActionKind::Login => {
                let claim = ActionTokenService::pending(&a);
                (a.as_session(), Some(claim))
            }
            AnyToken::Action(a) => {
                return Err(AurixError::AuthorizationDenied(format!(
                    "Action token authorises '{}', not login",
                    a.action
                )));
            }
        };

        let key = format!("session:{}", validated.user_id);
        if !self.rate_limiter.check(&key) {
            return Err(AurixError::RateLimitExceeded(
                "Too many connection attempts".into(),
            ));
        }

        let bans = aurix_db::queries::get_active_bans_for_user(
            &self.pool,
            validated.app_id.0,
            validated.user_id.0,
        )
        .await
        .map_err(|e| AurixError::Database(format!("Ban check failed: {e}")))?;

        if !bans.is_empty() {
            return Err(AurixError::UserBanned("User is banned".into()));
        }

        if let Some(user) =
            aurix_db::queries::get_user(&self.pool, validated.app_id.0, validated.user_id.0)
                .await
                .map_err(|e| AurixError::Database(format!("User lookup failed: {e}")))?
        {
            let ban_active = user.is_banned
                && user
                    .ban_expires_at
                    .map(|t| t > chrono::Utc::now())
                    .unwrap_or(true);
            if ban_active {
                return Err(AurixError::UserBanned(
                    user.ban_reason.unwrap_or_else(|| "User is banned".into()),
                ));
            }
        }

        // Check global mute via Redis
        if let Some(ref redis) = self.redis {
            if redis
                .is_globally_muted(validated.user_id)
                .await
                .unwrap_or(false)
            {
                // User is globally muted — still allowed to connect, just flagged
                tracing::info!("User {} is globally server-muted", validated.user_id);
            }
        }

        let node = self.nodes.select_node(self.config.server.region)?;
        let _ = ip_address;
        let _ = aurix_db::queries::update_user_last_seen(&self.pool, validated.user_id.0).await;

        Ok((validated, node, one_time))
    }

    /// Consumes the `jti` of a `login` action token; `Err(TokenReused)` on replay.
    pub async fn consume_login(&self, claim: &PendingClaim) -> Result<()> {
        self.action_tokens.consume(claim).await
    }
}
