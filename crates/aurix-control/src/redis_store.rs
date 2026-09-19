use aurix_common::error::{AurixError, Result};
use aurix_common::types::*;
use futures_util::StreamExt;
use redis::AsyncCommands;
use std::sync::Arc;

use crate::event_bus::{EventBus, EventEnvelope};
use std::time::Duration;

const EVENT_CHANNEL: &str = "aurix:events";
const REDIS_OP_TIMEOUT: Duration = Duration::from_secs(2);

/// Redis-backed distributed state for cross-node coordination.
pub struct RedisStore {
    client: redis::Client,
    manager: redis::aio::ConnectionManager,
    node_id: MediaNodeId,
}

impl RedisStore {
    pub async fn connect(client: redis::Client, node_id: MediaNodeId) -> Result<Self> {
        let manager = tokio::time::timeout(
            Duration::from_secs(5),
            redis::aio::ConnectionManager::new(client.clone()),
        )
        .await
        .map_err(|_| AurixError::Redis("Connection timed out".into()))?
        .map_err(|e| AurixError::Redis(format!("Connection failed: {e}")))?;
        Ok(Self {
            client,
            manager,
            node_id,
        })
    }

    pub fn node_id(&self) -> MediaNodeId {
        self.node_id
    }

    /// A pooled, auto-reconnecting connection handle (cheap to clone).
    async fn conn(&self) -> Result<redis::aio::ConnectionManager> {
        Ok(self.manager.clone())
    }

    /// Bound a Redis operation so a stalled broker cannot hang request handlers.
    async fn with_timeout<T, F>(fut: F, what: &str) -> Result<T>
    where
        F: std::future::Future<Output = redis::RedisResult<T>>,
    {
        match tokio::time::timeout(REDIS_OP_TIMEOUT, fut).await {
            Ok(Ok(v)) => Ok(v),
            Ok(Err(e)) => Err(AurixError::Redis(format!("{what}: {e}"))),
            Err(_) => Err(AurixError::Redis(format!("{what}: timed out"))),
        }
    }

    /// Replicate locally-originated events to other nodes and deliver remote events locally.
    pub fn start_event_replication(self: &Arc<Self>, bus: Arc<EventBus>) {
        // Outbound: local → Redis
        let store = self.clone();
        let mut outbound = bus.subscribe_outbound();
        tokio::spawn(async move {
            loop {
                match outbound.recv().await {
                    Ok(event) => {
                        let env = EventEnvelope {
                            id: uuid::Uuid::now_v7(),
                            origin: store.node_id,
                            event,
                        };
                        if let Ok(json) = serde_json::to_string(&env) {
                            if let Err(e) = store.publish_event(&json).await {
                                tracing::warn!("event replication publish failed: {e}");
                            }
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                        tracing::warn!("event replication lagged, {n} events skipped");
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
        });
        // Inbound: Redis → local
        self.start_event_subscriber(bus);
    }

    pub fn start_event_subscriber(&self, local_bus: Arc<EventBus>) {
        let client = self.client.clone();
        let self_id = self.node_id;
        tokio::spawn(async move {
            let mut seen: std::collections::VecDeque<uuid::Uuid> =
                std::collections::VecDeque::with_capacity(4096);
            loop {
                match client.get_async_pubsub().await {
                    Ok(mut pubsub) => {
                        if let Err(e) = pubsub.subscribe(EVENT_CHANNEL).await {
                            tracing::error!("Redis subscribe failed: {e}");
                            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                            continue;
                        }
                        tracing::info!("Redis Pub/Sub subscriber connected");
                        let mut stream = pubsub.on_message();
                        while let Some(msg) = stream.next().await {
                            let Ok(payload) = msg.get_payload::<String>() else {
                                continue;
                            };
                            let Ok(env) = serde_json::from_str::<EventEnvelope>(&payload) else {
                                tracing::debug!("ignoring malformed cross-node event");
                                continue;
                            };
                            if env.origin == self_id || seen.contains(&env.id) {
                                continue;
                            }
                            if seen.len() == 4096 {
                                seen.pop_front();
                            }
                            seen.push_back(env.id);
                            local_bus.deliver_remote(env.event);
                        }
                    }
                    Err(e) => {
                        tracing::error!("Redis Pub/Sub connection failed: {e}");
                        tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                    }
                }
            }
        });
    }

    pub async fn ping(&self) -> Result<()> {
        let mut conn = self.conn().await?;
        let _: String =
            Self::with_timeout(redis::cmd("PING").query_async(&mut conn), "ping").await?;
        Ok(())
    }

    /// Record which media node a session is on.
    pub async fn set_session_node(
        &self,
        session_id: SessionId,
        node_id: MediaNodeId,
    ) -> Result<()> {
        let mut conn = self.conn().await?;
        let key = format!("session:{}:node", session_id);
        Self::with_timeout(
            conn.set_ex::<_, _, ()>(&key, node_id.0.to_string(), 120),
            "set_session_node",
        )
        .await?;
        Ok(())
    }

    /// Look up which node a session is on.
    pub async fn get_session_node(&self, session_id: SessionId) -> Result<Option<MediaNodeId>> {
        let mut conn = self.conn().await?;
        let key = format!("session:{}:node", session_id);
        let val: Option<String> = Self::with_timeout(conn.get(&key), "get_session_node").await?;
        Ok(val
            .and_then(|s| uuid::Uuid::parse_str(&s).ok())
            .map(MediaNodeId::from_uuid))
    }

    /// Record which channels a user is in (for cross-node queries).
    pub async fn add_user_channel(&self, user_id: UserId, channel_id: ChannelId) -> Result<()> {
        let mut conn = self.conn().await?;
        let key = format!("user:{}:channels", user_id);
        Self::with_timeout(
            conn.sadd::<_, _, ()>(&key, channel_id.0.to_string()),
            "add_user_channel",
        )
        .await?;
        Self::with_timeout(conn.expire::<_, ()>(&key, 3600), "expire").await?;
        Ok(())
    }

    pub async fn remove_user_channel(&self, user_id: UserId, channel_id: ChannelId) -> Result<()> {
        let mut conn = self.conn().await?;
        let key = format!("user:{}:channels", user_id);
        Self::with_timeout(
            conn.srem::<_, _, ()>(&key, channel_id.0.to_string()),
            "remove_user_channel",
        )
        .await?;
        Ok(())
    }

    pub async fn get_user_channels(&self, user_id: UserId) -> Result<Vec<ChannelId>> {
        let mut conn = self.conn().await?;
        let key = format!("user:{}:channels", user_id);
        let members: Vec<String> =
            Self::with_timeout(conn.smembers(&key), "get_user_channels").await?;
        Ok(members
            .iter()
            .filter_map(|s| uuid::Uuid::parse_str(s).ok())
            .map(ChannelId::from_uuid)
            .collect())
    }

    /// Track active participant count per channel across all nodes.
    pub async fn incr_channel_participants(&self, channel_id: ChannelId) -> Result<i64> {
        let mut conn = self.conn().await?;
        let key = format!("channel:{}:count", channel_id);
        let count: i64 = Self::with_timeout(conn.incr(&key, 1i64), "incr").await?;
        Self::with_timeout(conn.expire::<_, ()>(&key, 3600), "expire").await?;
        Ok(count)
    }

    pub async fn decr_channel_participants(&self, channel_id: ChannelId) -> Result<i64> {
        let mut conn = self.conn().await?;
        let key = format!("channel:{}:count", channel_id);
        let count: i64 = Self::with_timeout(conn.decr(&key, 1i64), "decr").await?;
        Ok(count.max(0))
    }

    /// Publish an event to all nodes via Redis pub/sub.
    pub async fn publish_event(&self, event_json: &str) -> Result<()> {
        let mut conn = self.conn().await?;
        Self::with_timeout(
            conn.publish::<_, _, ()>(EVENT_CHANNEL, event_json),
            "publish",
        )
        .await
    }

    /// Distributed rate limiting: check if a key exceeds the limit using Redis INCR + EXPIRE.
    pub async fn check_rate_limit(&self, key: &str, limit: u32, window_secs: u64) -> Result<bool> {
        let mut conn = self.conn().await?;
        let redis_key = format!("ratelimit:{}", key);
        let count: i64 = Self::with_timeout(conn.incr(&redis_key, 1i64), "rate_limit incr").await?;
        if count == 1 {
            Self::with_timeout(
                conn.expire::<_, ()>(&redis_key, window_secs as i64),
                "rate_limit expire",
            )
            .await?;
        }
        Ok(count <= limit as i64)
    }

    /// Atomically claims `key` for `ttl_secs` (`SET NX EX`). Returns `false` when it was
    /// already claimed. Used for one-time `jti` consumption across nodes.
    pub async fn claim_once(&self, key: &str, ttl_secs: u64) -> Result<bool> {
        let mut conn = self.conn().await?;
        let redis_key = format!("once:{}", key);
        let res: Option<String> = Self::with_timeout(
            redis::cmd("SET")
                .arg(&redis_key)
                .arg(1u8)
                .arg("NX")
                .arg("EX")
                .arg(ttl_secs.max(1))
                .query_async(&mut conn),
            "claim_once",
        )
        .await?;
        Ok(res.is_some())
    }

    /// Global server-mute: set flag in Redis so all nodes can enforce it.
    pub async fn set_global_mute(&self, user_id: UserId, muted: bool) -> Result<()> {
        let mut conn = self.conn().await?;
        let key = format!("user:{}:server_muted", user_id);
        if muted {
            Self::with_timeout(conn.set_ex::<_, _, ()>(&key, "1", 86400), "set_global_mute")
                .await?;
        } else {
            Self::with_timeout(conn.del::<_, ()>(&key), "del_global_mute").await?;
        }
        Ok(())
    }

    pub async fn is_globally_muted(&self, user_id: UserId) -> Result<bool> {
        let mut conn = self.conn().await?;
        let key = format!("user:{}:server_muted", user_id);
        let val: Option<String> = Self::with_timeout(conn.get(&key), "is_globally_muted").await?;
        Ok(val.is_some())
    }

    /// Drops every per-user key (user erasure).
    pub async fn forget_user(&self, user_id: UserId) -> Result<()> {
        let mut conn = self.conn().await?;
        let keys = [
            format!("user:{}:server_muted", user_id),
            format!("user:{}:channels", user_id),
        ];
        Self::with_timeout(conn.del::<_, ()>(&keys[..]), "forget_user").await?;
        Ok(())
    }
}
