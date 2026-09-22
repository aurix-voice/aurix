use crate::config::RedisConfig;
use crate::error::{AurixError, Result};
use redis::cluster::{ClusterClient, ClusterClientBuilder};
use redis::sentinel::{SentinelClient, SentinelClientBuilder, SentinelServerType};
use redis::{ConnectionAddr, ConnectionInfo, IntoConnectionInfo, ProtocolVersion, TlsMode};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;

/// Where the node's Redis lives: a fixed URL, a master set resolved through Sentinel, or a
/// Redis Cluster addressed through its seed nodes.
#[derive(Clone)]
pub enum RedisSource {
    Direct(redis::Client),
    Sentinel {
        client: Arc<Mutex<SentinelClient>>,
        master: String,
    },
    Cluster {
        client: Arc<ClusterClient>,
        /// Seed addresses without credentials, for logs and health output.
        seeds: Vec<String>,
        /// Cluster nodes in `redis.cluster`, kept to build the RESP3 Pub/Sub connection.
        urls: Vec<String>,
        sharded_pubsub: bool,
    },
}

/// How long connection establishment and single round trips may take against the cluster.
const CLUSTER_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const CLUSTER_RESPONSE_TIMEOUT: Duration = Duration::from_secs(2);

impl RedisSource {
    /// Builds the source from the configuration and verifies it with a `PING`.
    pub async fn open(config: &RedisConfig) -> Result<Self> {
        if !config.cluster.is_empty() {
            let client = cluster_builder(&config.cluster)?
                .build()
                .map_err(|e| AurixError::Redis(format!("Redis Cluster client: {e}")))?;
            let seeds = config
                .cluster
                .iter()
                .map(|u| crate::config::redact_url_credentials(u))
                .collect();
            let source = Self::Cluster {
                client: Arc::new(client),
                seeds,
                urls: config.cluster.clone(),
                sharded_pubsub: config.sharded_pubsub,
            };
            let mut conn = source.cluster_connection().await?;
            let _: String = redis::cmd("PING")
                .query_async(&mut conn)
                .await
                .map_err(|e| AurixError::Redis(format!("Redis Cluster PING failed: {e}")))?;
            return Ok(source);
        }
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

    pub fn is_cluster(&self) -> bool {
        matches!(self, Self::Cluster { .. })
    }

    /// Cluster only: whether cross-node events go through sharded Pub/Sub.
    pub fn sharded_pubsub(&self) -> bool {
        matches!(
            self,
            Self::Cluster {
                sharded_pubsub: true,
                ..
            }
        )
    }

    /// Human-readable description of the configured endpoint(s), credentials stripped.
    pub fn describe(&self) -> String {
        match self {
            Self::Direct(client) => client.get_connection_info().addr.to_string(),
            Self::Sentinel { master, .. } => format!("sentinel master {master}"),
            Self::Cluster { seeds, .. } => format!("cluster [{}]", seeds.join(", ")),
        }
    }

    /// A client for the current master. With Sentinel this asks the sentinels every time,
    /// so callers should cache the result and re-resolve on failure or periodically. A
    /// cluster has no single master: use [`RedisSource::cluster_connection`] instead.
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
            Self::Cluster { .. } => Err(AurixError::Redis(
                "Redis Cluster has no single master to resolve".into(),
            )),
        }
    }

    /// A slot-aware connection to the cluster (routes by key, follows `MOVED`/`ASK`,
    /// refreshes the topology and reconnects by itself).
    pub async fn cluster_connection(&self) -> Result<redis::cluster_async::ClusterConnection> {
        let Self::Cluster { client, .. } = self else {
            return Err(AurixError::Redis("not a Redis Cluster source".into()));
        };
        tokio::time::timeout(CLUSTER_CONNECT_TIMEOUT, client.get_async_connection())
            .await
            .map_err(|_| AurixError::Redis("Redis Cluster connection timed out".into()))?
            .map_err(|e| AurixError::Redis(format!("Redis Cluster connection failed: {e}")))
    }

    /// A second cluster connection speaking RESP3 whose server pushes (Pub/Sub messages,
    /// subscription confirmations, disconnect notices) arrive on `pushes`. Subscriptions made
    /// on it are re-established by the client after a node failover or slot migration.
    pub async fn cluster_pubsub_connection(
        &self,
        pushes: tokio::sync::mpsc::UnboundedSender<redis::PushInfo>,
    ) -> Result<redis::cluster_async::ClusterConnection> {
        let Self::Cluster { urls, .. } = self else {
            return Err(AurixError::Redis("not a Redis Cluster source".into()));
        };
        let client = cluster_builder(urls)?
            .use_protocol(ProtocolVersion::RESP3)
            .push_sender(pushes)
            .build()
            .map_err(|e| AurixError::Redis(format!("Redis Cluster Pub/Sub client: {e}")))?;
        tokio::time::timeout(CLUSTER_CONNECT_TIMEOUT, client.get_async_connection())
            .await
            .map_err(|_| AurixError::Redis("Redis Cluster Pub/Sub connection timed out".into()))?
            .map_err(|e| AurixError::Redis(format!("Redis Cluster Pub/Sub connection: {e}")))
    }
}

fn cluster_builder(urls: &[String]) -> Result<ClusterClientBuilder> {
    let mut infos = Vec::with_capacity(urls.len());
    for url in urls {
        let info: ConnectionInfo = url.as_str().into_connection_info().map_err(|e| {
            AurixError::Redis(format!(
                "Invalid redis.cluster URL {}: {e}",
                crate::config::redact_url_credentials(url)
            ))
        })?;
        infos.push(info);
    }
    // Credentials, TLS mode and database come from the seed URLs (they must agree).
    Ok(ClusterClientBuilder::new(infos)
        .connection_timeout(CLUSTER_CONNECT_TIMEOUT)
        .response_timeout(CLUSTER_RESPONSE_TIMEOUT)
        .retries(3))
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
