use aurix_common::error::{AurixError, Result};
use aurix_common::types::*;
use futures_util::StreamExt;
use redis::AsyncCommands;
use std::sync::Arc;

/// Redis-backed distributed state for cross-node coordination.
pub struct RedisStore {
    client: redis::Client,
}

impl RedisStore {
    pub fn new(client: redis::Client) -> Self {
        Self { client }
    }

    async fn conn(&self) -> Result<redis::aio::MultiplexedConnection> {
        self.client.get_multiplexed_async_connection().await
            .map_err(|e| AurixError::Redis(format!("Connection failed: {e}")))
    }

    pub fn start_event_subscriber(
        &self,
        local_bus: Arc<crate::event_bus::EventBus>,
    ) {
        let client = self.client.clone();
        tokio::spawn(async move {
            loop {
                match client.get_async_connection().await {
                    Ok(conn) => {
                        let mut pubsub = conn.into_pubsub();
                        if let Err(e) = pubsub.subscribe("aurix:events").await {
                            tracing::error!("Redis subscribe failed: {e}");
                            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                            continue;
                        }
                        tracing::info!("Redis Pub/Sub subscriber connected");
                        let mut stream = pubsub.on_message();
                        while let Some(msg) = stream.next().await {
                            if let Ok(payload) = msg.get_payload::<String>() {
                                if let Ok(event) = serde_json::from_str::<crate::event_bus::ServerEvent>(&payload) {
                                    local_bus.publish(event);
                                }
                            }
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

    /// Record which media node a session is on.
    pub async fn set_session_node(&self, session_id: SessionId, node_id: MediaNodeId) -> Result<()> {
        let mut conn = self.conn().await?;
        let key = format!("session:{}:node", session_id);
        conn.set_ex::<_, _, ()>(&key, node_id.0.to_string(), 120)
            .await
            .map_err(|e| AurixError::Redis(format!("set_session_node: {e}")))?;
        Ok(())
    }

    /// Look up which node a session is on.
    pub async fn get_session_node(&self, session_id: SessionId) -> Result<Option<MediaNodeId>> {
        let mut conn = self.conn().await?;
        let key = format!("session:{}:node", session_id);
        let val: Option<String> = conn.get(&key).await
            .map_err(|e| AurixError::Redis(format!("get_session_node: {e}")))?;
        Ok(val.and_then(|s| uuid::Uuid::parse_str(&s).ok()).map(MediaNodeId::from_uuid))
    }

    /// Record which channels a user is in (for cross-node queries).
    pub async fn add_user_channel(&self, user_id: UserId, channel_id: ChannelId) -> Result<()> {
        let mut conn = self.conn().await?;
        let key = format!("user:{}:channels", user_id);
        conn.sadd::<_, _, ()>(&key, channel_id.0.to_string()).await
            .map_err(|e| AurixError::Redis(format!("add_user_channel: {e}")))?;
        conn.expire::<_, ()>(&key, 3600).await
            .map_err(|e| AurixError::Redis(format!("expire: {e}")))?;
        Ok(())
    }

    pub async fn remove_user_channel(&self, user_id: UserId, channel_id: ChannelId) -> Result<()> {
        let mut conn = self.conn().await?;
        let key = format!("user:{}:channels", user_id);
        conn.srem::<_, _, ()>(&key, channel_id.0.to_string()).await
            .map_err(|e| AurixError::Redis(format!("remove_user_channel: {e}")))?;
        Ok(())
    }

    pub async fn get_user_channels(&self, user_id: UserId) -> Result<Vec<ChannelId>> {
        let mut conn = self.conn().await?;
        let key = format!("user:{}:channels", user_id);
        let members: Vec<String> = conn.smembers(&key).await
            .map_err(|e| AurixError::Redis(format!("get_user_channels: {e}")))?;
        Ok(members.iter()
            .filter_map(|s| uuid::Uuid::parse_str(s).ok())
            .map(ChannelId::from_uuid)
            .collect())
    }

    /// Track active participant count per channel across all nodes.
    pub async fn incr_channel_participants(&self, channel_id: ChannelId) -> Result<i64> {
        let mut conn = self.conn().await?;
        let key = format!("channel:{}:count", channel_id);
        let count: i64 = conn.incr(&key, 1i64).await
            .map_err(|e| AurixError::Redis(format!("incr: {e}")))?;
        conn.expire::<_, ()>(&key, 3600).await
            .map_err(|e| AurixError::Redis(format!("expire: {e}")))?;
        Ok(count)
    }

    pub async fn decr_channel_participants(&self, channel_id: ChannelId) -> Result<i64> {
        let mut conn = self.conn().await?;
        let key = format!("channel:{}:count", channel_id);
        let count: i64 = conn.decr(&key, 1i64).await
            .map_err(|e| AurixError::Redis(format!("decr: {e}")))?;
        Ok(count.max(0))
    }

    /// Publish an event to all nodes via Redis pub/sub.
    pub async fn publish_event(&self, event_json: &str) -> Result<()> {
        let mut conn = self.conn().await?;
        conn.publish::<_, _, ()>("aurix:events", event_json).await
            .map_err(|e| AurixError::Redis(format!("publish: {e}")))?;
        Ok(())
    }

    /// Distributed rate limiting: check if a key exceeds the limit using Redis INCR + EXPIRE.
    pub async fn check_rate_limit(&self, key: &str, limit: u32, window_secs: u64) -> Result<bool> {
        let mut conn = self.conn().await?;
        let redis_key = format!("ratelimit:{}", key);
        let count: i64 = conn.incr(&redis_key, 1i64).await
            .map_err(|e| AurixError::Redis(format!("rate_limit incr: {e}")))?;
        if count == 1 {
            conn.expire::<_, ()>(&redis_key, window_secs as i64).await
                .map_err(|e| AurixError::Redis(format!("rate_limit expire: {e}")))?;
        }
        Ok(count <= limit as i64)
    }

    /// Global server-mute: set flag in Redis so all nodes can enforce it.
    pub async fn set_global_mute(&self, user_id: UserId, muted: bool) -> Result<()> {
        let mut conn = self.conn().await?;
        let key = format!("user:{}:server_muted", user_id);
        if muted {
            conn.set_ex::<_, _, ()>(&key, "1", 86400).await
                .map_err(|e| AurixError::Redis(format!("set_global_mute: {e}")))?;
        } else {
            conn.del::<_, ()>(&key).await
                .map_err(|e| AurixError::Redis(format!("del_global_mute: {e}")))?;
        }
        Ok(())
    }

    pub async fn is_globally_muted(&self, user_id: UserId) -> Result<bool> {
        let mut conn = self.conn().await?;
        let key = format!("user:{}:server_muted", user_id);
        let val: Option<String> = conn.get(&key).await
            .map_err(|e| AurixError::Redis(format!("is_globally_muted: {e}")))?;
        Ok(val.is_some())
    }
}