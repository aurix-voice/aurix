use crate::action_tokens::{ActionTokenService, PendingClaim};
use crate::block_manager::BlockManager;
use crate::channel_manager::ChannelManager;
use crate::chat::ChatService;
use crate::event_bus::{EventBus, ServerEvent};
use crate::limits::{FleetLimiter, Scope};
use crate::node_manager::NodeManager;
use crate::redis_store::RedisStore;
use crate::safety::SafetyService;
use crate::session_manager::SessionManager;
use crate::speech::SpeechService;
use crate::translation::TranslationService;
use crate::usage::UsageService;
use crate::user_lifecycle::{RetentionService, UserLifecycle};
use crate::webhooks::WebhookService;
use aurix_auth::admin::AdminAuthService;
use aurix_auth::oidc::OidcProvider;
use aurix_auth::{AnyToken, ApiKeyService, JwtService, RbacService, ValidatedToken};
use aurix_common::audit::AuditLogger;
use aurix_common::config::AurixConfig;
use aurix_common::error::{AurixError, Result};
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
    /// Admin SSO; `None` unless `auth.oidc.enabled`.
    pub admin_oidc: Option<Arc<OidcProvider>>,
    pub nodes: Arc<NodeManager>,
    pub channels: Arc<ChannelManager>,
    pub sessions: Arc<SessionManager>,
    pub blocks: Arc<BlockManager>,
    pub action_tokens: Arc<ActionTokenService>,
    pub chat: Arc<ChatService>,
    pub safety: Arc<SafetyService>,
    pub speech: Arc<SpeechService>,
    pub translation: Arc<TranslationService>,
    pub webhooks: Arc<WebhookService>,
    pub users: Arc<UserLifecycle>,
    pub retention: Arc<RetentionService>,
    pub usage: Arc<UsageService>,
    pub events: Arc<EventBus>,
    pub audit: Arc<AuditLogger>,
    pub limits: Arc<FleetLimiter>,
    pub redis: Option<Arc<RedisStore>>,
    /// Nodes this node already declared lost, until they heartbeat again.
    reaped_nodes: dashmap::DashSet<MediaNodeId>,
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
                .with_bootstrap_token(config.auth.admin_bootstrap_token.clone())
                .with_password_login(config.auth.admin_password_login),
        );
        let admin_oidc = if config.auth.oidc.enabled {
            let provider = Arc::new(OidcProvider::new(
                config.auth.oidc.clone(),
                &config.auth.jwt_secret,
            )?);
            match provider.warm_up().await {
                Ok(()) => tracing::info!(issuer = %provider.issuer(), "Admin OIDC SSO ready"),
                Err(e) => tracing::warn!(
                    issuer = %provider.issuer(),
                    "Admin OIDC discovery failed at start-up (will retry on first login): {e}"
                ),
            }
            Some(provider)
        } else {
            None
        };
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

        // Redis
        let redis = match aurix_common::redis_pool::RedisSource::open(&config.redis).await {
            Ok(source) => match RedisStore::connect(source, node_id).await {
                Ok(store) => {
                    tracing::info!(
                        master = %store.master_addr(),
                        sentinel = store.is_sentinel(),
                        "Redis connected"
                    );
                    let store = Arc::new(store);
                    store.start_sentinel_supervisor();
                    Some(store)
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

        let limits = Arc::new(FleetLimiter::new(
            config.rate_limiting.clone(),
            redis.clone(),
        ));
        limits.start_cleanup_task();

        let usage = Arc::new(UsageService::new(config.usage.clone(), pool.clone()));

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
        let safety = Arc::new(SafetyService::new(
            config.safety.clone(),
            pool.clone(),
            events.clone(),
        )?);
        safety.start();
        let chat = Arc::new(
            ChatService::new(
                config.chat.clone(),
                pool.clone(),
                events.clone(),
                safety.text_filter(),
            )
            .with_usage(usage.meter()),
        );
        chat.start_retention_sweep();
        let speech = Arc::new(SpeechService::new(
            config.tts.clone(),
            config.media.default_bitrate as i32,
            events.clone(),
            chat.clone(),
        ));
        if let Some(engine) = speech.engine() {
            engine.set_usage_meter(usage.meter());
        }
        let translation = Arc::new(TranslationService::new(
            config.translation.clone(),
            speech.enabled(),
        ));
        let webhooks = Arc::new(WebhookService::new(
            config.webhooks.clone(),
            config.is_production(),
            pool.clone(),
            events.clone(),
        )?);
        let users = Arc::new(UserLifecycle::new(
            pool.clone(),
            events.clone(),
            audit.clone(),
            redis.clone(),
        ));
        let retention = Arc::new(RetentionService::new(
            config.retention.clone(),
            pool.clone(),
            users.clone(),
            audit.clone(),
        ));

        // Node health checker
        let nodes_clone = nodes.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(15));
            loop {
                interval.tick().await;
                nodes_clone.check_node_health().await;
            }
        });

        Ok(Self {
            node_id,
            config: Arc::new(config),
            pool,
            jwt,
            rbac,
            api_keys,
            admin_auth,
            admin_oidc,
            nodes,
            channels,
            sessions,
            action_tokens,
            chat,
            safety,
            speech,
            translation,
            webhooks,
            users,
            retention,
            usage,
            blocks,
            events,
            audit,
            limits,
            redis,
            reaped_nodes: dashmap::DashSet::new(),
        })
    }

    /// Periodically declares nodes lost and cleans up after them (see [`Self::reap_lost_nodes`]).
    pub fn start_lost_node_reaper(self: &Arc<Self>) {
        let weak = Arc::downgrade(self);
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(5));
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                interval.tick().await;
                let Some(plane) = weak.upgrade() else { return };
                plane.reap_lost_nodes().await;
            }
        });
    }

    /// A node silent for `cluster.node_lost_after_secs` is lost: its sessions and memberships
    /// are closed in the database with reason `node_lost` and every roster is told, so the
    /// fleet does not show ghosts until the 24 h registry prune. Exactly one node does this per
    /// lost node (Redis `SET NX`); the mirrors stay, so a client of the lost node that shows up
    /// here within the mirror TTL still gets its session back (`migrate_session` reopens it).
    /// Sessions whose node has no registry row at all (pruned before anyone reaped it) are
    /// closed the same way, as of now, so they stop accruing usage.
    pub async fn reap_lost_nodes(&self) {
        let after = self.config.cluster.node_lost_after_secs;
        let mut lost = self.nodes.lost_nodes(self.node_id, after);
        match aurix_db::queries::orphaned_session_nodes(&self.pool).await {
            Ok(orphans) => {
                for node in orphans {
                    let node = MediaNodeId::from_uuid(node);
                    if node != self.node_id && !lost.contains(&node) {
                        lost.push(node);
                    }
                }
            }
            Err(e) => tracing::warn!("orphaned-session scan skipped: {e}"),
        }
        self.reaped_nodes.retain(|id| lost.contains(id));
        for node in lost {
            if self.reaped_nodes.contains(&node) {
                continue;
            }
            if let Some(redis) = &self.redis {
                match redis.is_node_alive(node).await {
                    Ok(true) => continue, // database lag, not a dead node
                    Ok(false) => {}
                    Err(e) => {
                        tracing::warn!("lost-node check for {node} skipped, Redis: {e}");
                        continue;
                    }
                }
                match redis.claim_reaper(node, after.max(10)).await {
                    Ok(true) => {}
                    Ok(false) => {
                        self.reaped_nodes.insert(node);
                        continue;
                    }
                    Err(e) => {
                        tracing::warn!("reaper claim for {node} failed: {e}");
                        continue;
                    }
                }
            }
            match self.sessions.reap_lost_node(node).await {
                Ok(reaped) => {
                    self.reaped_nodes.insert(node);
                    aurix_metrics::NODES_REAPED.inc();
                    if reaped.sessions > 0 || !reaped.memberships.is_empty() {
                        tracing::warn!(
                            "node {node} lost: closed {} sessions and {} memberships",
                            reaped.sessions,
                            reaped.memberships.len()
                        );
                    }
                    let now = chrono::Utc::now();
                    for m in reaped.memberships {
                        self.events.publish(ServerEvent::ParticipantLeft {
                            app_id: AppId::from_uuid(m.app_id),
                            channel_id: ChannelId::from_uuid(m.channel_id),
                            user_id: UserId::from_uuid(m.user_id),
                            session_id: SessionId::from_uuid(m.session_id),
                            reason: "node_lost".into(),
                            hidden: false,
                            timestamp: now,
                        });
                    }
                    for (app_id, channel_id) in reaped.deactivated_channels {
                        self.channel_emptied(app_id, channel_id).await;
                    }
                }
                Err(e) => tracing::warn!("reaping lost node {node} failed: {e}"),
            }
        }
    }

    /// End-user REST authentication. Accepts the session JWT and — because a client that
    /// opened its session with a `login` action token holds nothing else — `login` action
    /// tokens as well (not consumed here: REST reads are idempotent and the token stays
    /// short-lived). Any other action token is refused, as is a token minted before its user
    /// was erased.
    pub async fn validate_token(&self, token: &str) -> Result<ValidatedToken> {
        let validated = match self.jwt.validate_any(token)? {
            AnyToken::Session(v) => v,
            AnyToken::Action(a) if a.action == ActionKind::Login => a.as_session(),
            AnyToken::Action(a) => Err(AurixError::AuthorizationDenied(format!(
                "Action token authorises '{}', not API access",
                a.action
            )))?,
        };
        self.reject_if_erased(&validated).await?;
        Ok(validated)
    }

    /// Refuses tokens issued at or before the user's erasure; a user re-created afterwards
    /// (new `POST /v1/tokens`) gets a fresh id, so only stale credentials are affected.
    async fn reject_if_erased(&self, validated: &ValidatedToken) -> Result<()> {
        let deleted_at = aurix_db::queries::get_user_tombstone(
            &self.pool,
            validated.app_id.0,
            validated.user_id.0,
        )
        .await
        .map_err(|e| AurixError::Database(format!("Tombstone check failed: {e}")))?;
        match deleted_at {
            Some(t) if validated.issued_at <= t.timestamp() => Err(AurixError::TokenInvalid(
                "Token was issued before the user was deleted".into(),
            )),
            _ => Ok(()),
        }
    }

    /// Validate an admin JWT against the account's current state (active, not revoked, role
    /// as stored now rather than as it was when the token was issued).
    pub async fn authenticate_admin(&self, token: &str) -> Result<AdminContext> {
        self.admin_auth.authenticate_token(token).await
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

        if self
            .limits
            .check(Scope::Connect, &validated.user_id.to_string())
            .await
            .is_err()
        {
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

        self.reject_if_erased(&validated).await?;

        let user = aurix_db::queries::get_user(&self.pool, validated.app_id.0, validated.user_id.0)
            .await
            .map_err(|e| AurixError::Database(format!("User lookup failed: {e}")))?
            .ok_or_else(|| AurixError::UserNotFound(validated.user_id.to_string()))?;
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

    /// The last participant left `channel_id`: announce `channel.deactivated` and, for ad-hoc
    /// channels, soft-delete the row and announce `channel.destroyed`. Safe to call from every
    /// node; only the node whose delete wins announces destruction.
    pub async fn channel_emptied(&self, app_id: AppId, channel_id: ChannelId) {
        let now = chrono::Utc::now();
        self.events.publish(ServerEvent::ChannelDeactivated {
            app_id,
            channel_id,
            timestamp: now,
        });
        match self.channels.release_if_ad_hoc(app_id, channel_id).await {
            Ok(true) => self.events.publish(ServerEvent::ChannelDestroyed {
                app_id,
                channel_id,
                timestamp: now,
            }),
            Ok(false) => {}
            Err(e) => tracing::warn!("ad-hoc channel release failed for {channel_id}: {e}"),
        }
    }

    /// Undoes an ad-hoc creation whose join did not complete (nobody ever entered).
    pub async fn release_ad_hoc(&self, app_id: AppId, channel_id: ChannelId) {
        match self.channels.release_if_ad_hoc(app_id, channel_id).await {
            Ok(true) => self.events.publish(ServerEvent::ChannelDestroyed {
                app_id,
                channel_id,
                timestamp: chrono::Utc::now(),
            }),
            Ok(false) => {}
            Err(e) => tracing::warn!("ad-hoc channel release failed for {channel_id}: {e}"),
        }
    }
}
