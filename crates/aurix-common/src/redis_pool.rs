use crate::config::RedisConfig;
use crate::error::{AurixError, Result};

pub async fn create_redis_client(config: &RedisConfig) -> Result<redis::Client> {
    let client = redis::Client::open(config.url.as_str())
        .map_err(|e| AurixError::Redis(format!("Failed to create Redis client: {e}")))?;

    // Verify connectivity
    let mut conn = client.get_multiplexed_async_connection().await
        .map_err(|e| AurixError::Redis(format!("Failed to connect to Redis: {e}")))?;

    let _: String = redis::cmd("PING")
        .query_async(&mut conn)
        .await
        .map_err(|e| AurixError::Redis(format!("Redis PING failed: {e}")))?;

    Ok(client)
}

pub type RedisConn = redis::aio::MultiplexedConnection;

pub async fn get_conn(client: &redis::Client) -> Result<RedisConn> {
    client
        .get_multiplexed_async_connection()
        .await
        .map_err(|e| AurixError::Redis(format!("Redis connection failed: {e}")))
}