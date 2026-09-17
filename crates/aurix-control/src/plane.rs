use crate::analytics::AnalyticsCollector;
use crate::channel_manager::ChannelManager;
use crate::event_bus::EventBus;
use crate::node_manager::NodeManager;
use crate::redis_store::RedisStore;
use crate::session_manager::SessionManager;
use aurix_auth::{ApiKeyService, JwtService, RbacService};
use aurix_auth::admin::AdminAuthService;
use aurix_common::audit::AuditLogger;
use aurix_common::config::AurixConfig;
use aurix_common::error::{AurixError, Result};
use aurix_common::rate_limit::RateLimiter;
use aurix_common::types::*;
use aurix_db::DbPool;
use std::sync::Arc;

pub struct ControlPlane {
    pub config: Arc<AurixConfig>,
    pub pool: DbPool,
    pub jwt: Arc<JwtService>,
    pub rbac: Arc<RbacService>,
    pub api_keys: Arc<ApiKeyService>,
    pub admin_auth: Arc<AdminAuthService>,
    pub nodes: Arc<NodeManager>,
    pub channels: Arc<ChannelManager>,
    pub sessions: Arc<SessionManager>,
    pub events: Arc<EventBus>,
    pub audit: Arc<AuditLogger>,
    pub rate_limiter: Arc<RateLimiter>,
    pub redis: Option<Arc<RedisStore>>,
}

impl ControlPlane {
    pub async fn new(config: AurixConfig, pool: DbPool) -> Result<Self> {
        let jwt = Arc::new(JwtService::new(&config.auth)?);
        let rbac = Arc::new(RbacService::new());
        let api_keys = Arc::new(ApiKeyService::new(pool.clone()));
        let admin_auth = Arc::new(AdminAuthService::new(pool.clone(), config.auth.jwt_secret.clone()));
        let nodes = Arc::new(NodeManager::new(pool.clone()));
        let channels = Arc::new(ChannelManager::new(pool.clone()));
        let sessions = Arc::new(SessionManager::new(pool.clone()));
        let events = Arc::new(EventBus::new(10000));

        let (audit_tx, audit_rx) = flume::bounded(10000);
        let audit = Arc::new(AuditLogger::new(audit_tx));

        // Audit log writer
        let audit_pool = pool.clone();
        tokio::spawn(async move {
            while let Ok(entry) = audit_rx.recv_async().await {
                let row = aurix_db::models::AuditLogRow {
                    id: entry.id,
                    app_id: None,
                    actor_id: entry.actor_id.0,
                    action: format!("{:?}", entry.action).to_lowercase(),
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
            Ok(client) => {
                tracing::info!("Redis connected");
                Some(Arc::new(RedisStore::new(client)))
            }
            Err(e) => {
                tracing::warn!("Redis not available (non-fatal): {}", e);
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

        // Start Redis Pub/Sub subscriber for cross-node events ──
        if let Some(ref redis_store) = redis {
            redis_store.start_event_subscriber(events.clone());
        }

        // Republish local events to Redis for cross-node delivery ──
        if let Some(ref redis_store) = redis {
            let mut local_rx = events.subscribe();
            let redis_pub = redis_store.clone();
            tokio::spawn(async move {
                while let Ok(event) = local_rx.recv().await {
                    if let Ok(json) = serde_json::to_string(&event) {
                        let _ = redis_pub.publish_event(&json).await;
                    }
                }
            });
        }

        Ok(Self {
            config: Arc::new(config),
            pool,
            jwt,
            rbac,
            api_keys,
            admin_auth,
            nodes,
            channels,
            sessions,
            events,
            audit,
            rate_limiter,
            redis,
        })
    }

    pub fn validate_token(&self, token: &str) -> Result<aurix_auth::ValidatedToken> {
        self.jwt.validate_token(token)
    }

    pub async fn authenticate_session(
        &self,
        token: &str,
        ip_address: &str,
    ) -> Result<(aurix_auth::ValidatedToken, MediaNodeInfo)> {
        let validated = self.jwt.validate_token(token)?;

        let bans = aurix_db::queries::get_active_bans_for_user(
            &self.pool, validated.app_id.0, validated.user_id.0,
        ).await.map_err(|e| AurixError::Database(format!("Ban check failed: {e}")))?;

        if !bans.is_empty() {
            return Err(AurixError::UserBanned("User is banned".into()));
        }

        // Check global mute via Redis
        if let Some(ref redis) = self.redis {
            if redis.is_globally_muted(validated.user_id).await.unwrap_or(false) {
                // User is globally muted — still allowed to connect, just flagged
                tracing::info!("User {} is globally server-muted", validated.user_id);
            }
        }

        let key = format!("session:{}", validated.user_id);
        if !self.rate_limiter.check(&key) {
            return Err(AurixError::RateLimitExceeded("Too many connection attempts".into()));
        }

        let node = self.nodes.select_node(self.config.server.region)?;
        let _ = aurix_db::queries::update_user_last_seen(&self.pool, validated.user_id.0).await;

        Ok((validated, node))
    }
}