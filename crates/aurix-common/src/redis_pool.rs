use crate::config::RedisConfig;
use crate::error::{AurixError, Result};
use redis::sentinel::{SentinelClient, SentinelClientBuilder, SentinelServerType};
use redis::{ConnectionAddr, ConnectionInfo, IntoConnectionInfo, TlsMode};
use std::sync::Arc;
use tokio::sync::Mutex;

/// Where the node's Redis lives: a fixed URL, or a master set resolved through Sentinel.
#[derive(Clone)]
pub enum RedisSource {
    Direct(redis::Client),
    Sentinel {
        client: Arc<Mutex<SentinelClient>>,
        master: String,
    },
}

impl RedisSource {
    /// Builds the source from the configuration and verifies it with a `PING`.
    pub async fn open(config: &RedisConfig) -> Result<Self> {
        let source = if config.sentinels.is_empty() {
            let client = redis::Client::open(config.url.as_str())
                .map_err(|e| AurixError::Redis(format!("Failed to create Redis client: {e}")))?;
            Self::Direct(client)
        } else {
            let master = config
                .sentinel_master
                .clone()
                .ok_or_else(|| AurixError::Redis("redis.sentinel_master is not set".into()))?;
            let client = build_sentinel_client(config, &master)?;
            Self::Sentinel {
                client: Arc::new(Mutex::new(client)),
                master,
            }
        };
        let client = source.resolve().await?;
        let mut conn = client
            .get_multiplexed_async_connection()
            .await
            .map_err(|e| AurixError::Redis(format!("Failed to connect to Redis: {e}")))?;
        let _: String = redis::cmd("PING")
            .query_async(&mut conn)
            .await
            .map_err(|e| AurixError::Redis(format!("Redis PING failed: {e}")))?;
        Ok(source)
    }

    pub fn is_sentinel(&self) -> bool {
        matches!(self, Self::Sentinel { .. })
    }

    /// A client for the current master. With Sentinel this asks the sentinels every time,
    /// so callers should cache the result and re-resolve on failure or periodically.
    pub async fn resolve(&self) -> Result<redis::Client> {
        match self {
            Self::Direct(client) => Ok(client.clone()),
            Self::Sentinel { client, master } => {
                let mut guard = client.lock().await;
                tokio::time::timeout(std::time::Duration::from_secs(5), guard.async_get_client())
                    .await
                    .map_err(|_| {
                        AurixError::Redis(format!("Sentinel lookup of master {master} timed out"))
                    })?
                    .map_err(|e| {
                        AurixError::Redis(format!("Sentinel lookup of master {master}: {e}"))
                    })
            }
        }
    }
}

fn build_sentinel_client(config: &RedisConfig, master: &str) -> Result<SentinelClient> {
    // `url` only contributes credentials, db and TLS mode for the data nodes.
    let data: ConnectionInfo = config
        .url
        .as_str()
        .into_connection_info()
        .map_err(|e| AurixError::Redis(format!("Invalid redis.url: {e}")))?;
    let mut sentinel_addrs = Vec::with_capacity(config.sentinels.len());
    let mut sentinel_auth: Option<ConnectionInfo> = None;
    for url in &config.sentinels {
        let info: ConnectionInfo = url
            .as_str()
            .into_connection_info()
            .map_err(|e| AurixError::Redis(format!("Invalid sentinel URL {url:?}: {e}")))?;
        if sentinel_auth.is_none() {
            sentinel_auth = Some(info.clone());
        }
        sentinel_addrs.push(info.addr);
    }
    let mut builder = SentinelClientBuilder::new(
        sentinel_addrs,
        master.to_string(),
        SentinelServerType::Master,
    )
    .map_err(|e| AurixError::Redis(format!("Sentinel client: {e}")))?
    .set_client_to_redis_db(data.redis.db)
    .set_client_to_redis_protocol(data.redis.protocol);
    if let Some(tls) = tls_mode(&data.addr) {
        builder = builder.set_client_to_redis_tls_mode(tls);
    }
    if let Some(user) = data.redis.username.clone() {
        builder = builder.set_client_to_redis_username(user);
    }
    if let Some(pass) = data.redis.password.clone() {
        builder = builder.set_client_to_redis_password(pass);
    }
    if let Some(first) = sentinel_auth {
        if let Some(tls) = tls_mode(&first.addr) {
            builder = builder.set_client_to_sentinel_tls_mode(tls);
        }
        if let Some(user) = first.redis.username.clone() {
            builder = builder.set_client_to_sentinel_username(user);
        }
        if let Some(pass) = first.redis.password.clone() {
            builder = builder.set_client_to_sentinel_password(pass);
        }
    }
    builder
        .build()
        .map_err(|e| AurixError::Redis(format!("Sentinel client: {e}")))
}

fn tls_mode(addr: &ConnectionAddr) -> Option<TlsMode> {
    match addr {
        ConnectionAddr::TcpTls { insecure: true, .. } => Some(TlsMode::Insecure),
        ConnectionAddr::TcpTls { .. } => Some(TlsMode::Secure),
        _ => None,
    }
}

/// Opens the configured Redis and returns a client for the current master.
pub async fn create_redis_client(config: &RedisConfig) -> Result<redis::Client> {
    RedisSource::open(config).await?.resolve().await
}

pub type RedisConn = redis::aio::MultiplexedConnection;

pub async fn get_conn(client: &redis::Client) -> Result<RedisConn> {
    client
        .get_multiplexed_async_connection()
        .await
        .map_err(|e| AurixError::Redis(format!("Redis connection failed: {e}")))
}
