use aurix_common::error::{AurixError, Result};
use aurix_common::redis_pool::RedisSource;
use aurix_common::types::*;
use futures_util::StreamExt;
use redis::aio::ConnectionLike;
use redis::{AsyncCommands, PushKind};
use std::sync::Arc;

use crate::event_bus::{EventBus, EventEnvelope, ServerEvent};
use crate::session_mirror::{SessionMirror, TakeoverRefused};
use std::time::Duration;

/// Cross-node event channel. The hash tag pins it to one slot so that, on a cluster with
/// sharded Pub/Sub, publishers and subscribers meet on the same shard.
const EVENT_CHANNEL: &str = "{aurix}:events";
const REDIS_OP_TIMEOUT: Duration = Duration::from_secs(2);
/// How often a Sentinel-backed store re-asks the sentinels who the master is.
const SENTINEL_POLL: Duration = Duration::from_secs(5);

/// The Redis the store currently talks to. Swapped as a whole on a Sentinel failover; a
/// cluster connection routes and re-routes by itself.
#[derive(Clone)]
enum Handle {
    Single {
        client: redis::Client,
        manager: redis::aio::ConnectionManager,
    },
    Cluster(redis::cluster_async::ClusterConnection),
}

/// One command sink for both backends so every operation below is written once.
#[derive(Clone)]
pub enum Conn {
    Single(redis::aio::ConnectionManager),
    Cluster(redis::cluster_async::ClusterConnection),
}

impl ConnectionLike for Conn {
    fn req_packed_command<'a>(
        &'a mut self,
        cmd: &'a redis::Cmd,
    ) -> redis::RedisFuture<'a, redis::Value> {
        match self {
            Self::Single(c) => c.req_packed_command(cmd),
            Self::Cluster(c) => c.req_packed_command(cmd),
        }
    }

    fn req_packed_commands<'a>(
        &'a mut self,
        cmd: &'a redis::Pipeline,
        offset: usize,
        count: usize,
    ) -> redis::RedisFuture<'a, Vec<redis::Value>> {
        match self {
            Self::Single(c) => c.req_packed_commands(cmd, offset, count),
            Self::Cluster(c) => c.req_packed_commands(cmd, offset, count),
        }
    }

    fn get_db(&self) -> i64 {
        match self {
            Self::Single(c) => c.get_db(),
            Self::Cluster(c) => c.get_db(),
        }
    }
}

// ── Key layout ──
//
// Keys that one Lua script or one multi-key command touches together share a hash tag
// (`{…}`), so they land in the same Redis Cluster slot and never answer `CROSSSLOT`. Keys
// used on their own are untagged; the cluster spreads them over its shards.

fn session_owner_key(session_id: SessionId) -> String {
    format!("session:{{{session_id}}}:node")
}

fn session_mirror_key(session_id: SessionId) -> String {
    format!("session:{{{session_id}}}:mirror")
}

fn user_channels_key(user_id: UserId) -> String {
    format!("user:{{{user_id}}}:channels")
}

fn user_server_muted_key(user_id: UserId) -> String {
    format!("user:{{{user_id}}}:server_muted")
}

/// Redis-backed distributed state for cross-node coordination.
pub struct RedisStore {
    source: RedisSource,
    handle: arc_swap::ArcSwap<Handle>,
    /// Bumped on every master switch; long-lived consumers (Pub/Sub) reconnect on change.
    generation: tokio::sync::watch::Sender<u64>,
    /// `true` while the cross-node event subscriber is attached to the current master.
    /// Pub/Sub is not durable: events published while this is `false` never reach this node,
    /// so readiness reports it.
    subscribed: std::sync::atomic::AtomicBool,
    subscriber_started: std::sync::atomic::AtomicBool,
    node_id: MediaNodeId,
}

impl RedisStore {
    pub async fn connect(source: RedisSource, node_id: MediaNodeId) -> Result<Self> {
        let handle = if source.is_cluster() {
            Handle::Cluster(source.cluster_connection().await?)
        } else {
            let client = source.resolve().await?;
            Self::open_handle(client).await?
        };
        let (generation, _) = tokio::sync::watch::channel(0);
        let store = Self {
            source,
            handle: arc_swap::ArcSwap::from_pointee(handle),
            generation,
            subscribed: std::sync::atomic::AtomicBool::new(false),
            subscriber_started: std::sync::atomic::AtomicBool::new(false),
            node_id,
        };
        Ok(store)
    }

    async fn open_handle(client: redis::Client) -> Result<Handle> {
        let manager = tokio::time::timeout(
            Duration::from_secs(5),
            redis::aio::ConnectionManager::new(client.clone()),
        )
        .await
        .map_err(|_| AurixError::Redis("Connection timed out".into()))?
        .map_err(|e| AurixError::Redis(format!("Connection failed: {e}")))?;
        Ok(Handle::Single { client, manager })
    }

    pub fn node_id(&self) -> MediaNodeId {
        self.node_id
    }

    pub fn is_sentinel(&self) -> bool {
        self.source.is_sentinel()
    }

    pub fn is_cluster(&self) -> bool {
        self.source.is_cluster()
    }

    /// `direct`, `sentinel` or `cluster`, for logs and health output.
    pub fn backend(&self) -> &'static str {
        match &self.source {
            RedisSource::Direct(_) => "direct",
            RedisSource::Sentinel { .. } => "sentinel",
            RedisSource::Cluster { .. } => "cluster",
        }
    }

    /// Whether cross-node events currently reach this node. `true` when replication was never
    /// started (nothing to receive), otherwise only while the Pub/Sub subscriber is attached.
    pub fn event_subscriber_connected(&self) -> bool {
        use std::sync::atomic::Ordering;
        !self.subscriber_started.load(Ordering::Acquire) || self.subscribed.load(Ordering::Acquire)
    }

    /// Address of the master currently in use (`host:port`), or the cluster seeds, for logs
    /// and health output.
    pub fn master_addr(&self) -> String {
        match &**self.handle.load() {
            Handle::Single { client, .. } => client.get_connection_info().addr.to_string(),
            Handle::Cluster(_) => self.source.describe(),
        }
    }

    /// With Sentinel: follows master switches. Re-resolves the master periodically and after
    /// every failed `PING`, swaps the connection manager and tells Pub/Sub consumers to
    /// reconnect. A no-op for a direct URL (the connection manager reconnects by itself).
    pub fn start_sentinel_supervisor(self: &Arc<Self>) {
        if !self.source.is_sentinel() {
            return;
        }
        let store = self.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(SENTINEL_POLL);
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                interval.tick().await;
                let current = store.master_addr();
                let client = match store.source.resolve().await {
                    Ok(c) => c,
                    Err(e) => {
                        tracing::warn!("Redis Sentinel master lookup failed: {e}");
                        continue;
                    }
                };
                let next = client.get_connection_info().addr.to_string();
                if next == current && store.ping().await.is_ok() {
                    continue;
                }
                match Self::open_handle(client).await {
                    Ok(handle) => {
                        store.handle.store(Arc::new(handle));
                        store.generation.send_modify(|g| *g += 1);
                        aurix_metrics::REDIS_FAILOVERS.inc();
                        tracing::warn!("Redis master switched {current} -> {next}");
                    }
                    Err(e) => tracing::warn!("Redis master {next} not reachable yet: {e}"),
                }
            }
        });
    }

    /// A pooled, auto-reconnecting connection handle (cheap to clone).
    async fn conn(&self) -> Result<Conn> {
        Ok(match &**self.handle.load() {
            Handle::Single { manager, .. } => Conn::Single(manager.clone()),
            Handle::Cluster(conn) => Conn::Cluster(conn.clone()),
        })
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

    pub fn start_event_subscriber(self: &Arc<Self>, local_bus: Arc<EventBus>) {
        use std::sync::atomic::Ordering;
        self.subscriber_started.store(true, Ordering::Release);
        if self.source.is_cluster() {
            self.start_cluster_event_subscriber(local_bus);
            return;
        }
        let handle = self.handle.load_full();
        let mut generation = self.generation.subscribe();
        let source = self.source.clone();
        let self_id = self.node_id;
        let store = self.clone();
        tokio::spawn(async move {
            let mut seen = SeenEvents::default();
            let mut client = match &*handle {
                Handle::Single { client, .. } => client.clone(),
                Handle::Cluster(_) => unreachable!("cluster handled above"),
            };
            loop {
                generation.mark_unchanged();
                match client.get_async_pubsub().await {
                    Ok(mut pubsub) => {
                        if let Err(e) = pubsub.subscribe(EVENT_CHANNEL).await {
                            tracing::error!("Redis subscribe failed: {e}");
                            Self::backoff(&mut generation, Duration::from_secs(2)).await;
                            continue;
                        }
                        tracing::info!("Redis Pub/Sub subscriber connected");
                        store.subscribed.store(true, Ordering::Release);
                        let mut stream = pubsub.on_message();
                        loop {
                            let msg = tokio::select! {
                                msg = stream.next() => match msg {
                                    Some(msg) => msg,
                                    None => break,
                                },
                                changed = generation.changed() => {
                                    if changed.is_ok() {
                                        tracing::info!("Redis master changed; re-subscribing event bus");
                                    }
                                    break;
                                }
                            };
                            let Ok(payload) = msg.get_payload::<String>() else {
                                continue;
                            };
                            if let Some(event) = seen.accept(&payload, self_id) {
                                local_bus.deliver_remote(event);
                            }
                        }
                        store.subscribed.store(false, Ordering::Release);
                        tracing::warn!("Redis Pub/Sub subscriber disconnected");
                    }
                    Err(e) => {
                        tracing::error!("Redis Pub/Sub connection failed: {e}");
                        Self::backoff(&mut generation, Duration::from_secs(5)).await;
                    }
                }
                // Whatever the reason for the drop, point at the current master.
                if let Ok(next) = source.resolve().await {
                    client = next;
                }
            }
        });
    }

    /// Cluster variant of the subscriber: a RESP3 connection whose pushes arrive on a channel.
    /// The subscription is routed to the shard owning the event channel's slot; when a node
    /// connection drops (`PushKind::Disconnection`) or the server kicks us off the channel
    /// (`sunsubscribe` while a promoted replica re-adds the slot) the whole connection is
    /// rebuilt and re-subscribed, which lands on the new master once the cluster re-elects.
    /// An idle subscriber would otherwise never notice the loss.
    fn start_cluster_event_subscriber(self: &Arc<Self>, local_bus: Arc<EventBus>) {
        use std::sync::atomic::Ordering;
        let source = self.source.clone();
        let sharded = source.sharded_pubsub();
        let self_id = self.node_id;
        let store = self.clone();
        let mut generation = self.generation.subscribe();
        tokio::spawn(async move {
            let mut seen = SeenEvents::default();
            loop {
                let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<redis::PushInfo>();
                let mut conn = match source.cluster_pubsub_connection(tx).await {
                    Ok(c) => c,
                    Err(e) => {
                        tracing::error!("Redis Cluster Pub/Sub connection failed: {e}");
                        Self::backoff(&mut generation, Duration::from_secs(5)).await;
                        continue;
                    }
                };
                let subscribed = if sharded {
                    conn.ssubscribe(EVENT_CHANNEL).await
                } else {
                    conn.subscribe(EVENT_CHANNEL).await
                };
                if let Err(e) = subscribed {
                    tracing::error!(
                        "Redis Cluster {}subscribe failed: {e}",
                        if sharded { "s" } else { "" }
                    );
                    Self::backoff(&mut generation, Duration::from_secs(2)).await;
                    continue;
                }
                tracing::info!(sharded, "Redis Cluster Pub/Sub subscriber connected");
                store.subscribed.store(true, Ordering::Release);
                let mut dropped = false;
                while let Some(push) = rx.recv().await {
                    match push.kind {
                        PushKind::Message | PushKind::SMessage => {
                            let Some(msg) = redis::Msg::from_push_info(push) else {
                                continue;
                            };
                            let Ok(payload) = msg.get_payload::<String>() else {
                                continue;
                            };
                            if let Some(event) = seen.accept(&payload, self_id) {
                                local_bus.deliver_remote(event);
                            }
                        }
                        // A promoted replica re-adds its slots and kicks every sharded
                        // subscriber with an unsolicited `sunsubscribe`; treat it like a drop.
                        PushKind::Disconnection
                        | PushKind::SUnsubscribe
                        | PushKind::Unsubscribe => {
                            dropped = true;
                            break;
                        }
                        _ => {}
                    }
                }
                store.subscribed.store(false, Ordering::Release);
                if dropped {
                    tracing::warn!("Redis Cluster node connection dropped; re-subscribing");
                } else {
                    tracing::warn!("Redis Cluster Pub/Sub subscriber closed; reconnecting");
                }
                drop(conn);
                Self::backoff(&mut generation, Duration::from_secs(1)).await;
            }
        });
    }

    /// Sleep between subscriber attempts, cut short by a master switch.
    async fn backoff(generation: &mut tokio::sync::watch::Receiver<u64>, max: Duration) {
        tokio::select! {
            _ = tokio::time::sleep(max) => {}
            _ = generation.changed() => {}
        }
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
        let key = session_owner_key(session_id);
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
        let key = session_owner_key(session_id);
        let val: Option<String> = Self::with_timeout(conn.get(&key), "get_session_node").await?;
        Ok(val
            .and_then(|s| uuid::Uuid::parse_str(&s).ok())
            .map(MediaNodeId::from_uuid))
    }

    // ── Session mirror (cross-node resume) ──
    //
    // `session:{<id>}:node`   owner, refreshed with the mirror; the fence.
    // `session:{<id>}:mirror` JSON `SessionMirror`; only the owner writes it.
    // Both carry the same TTL so a dead node's session stays adoptable for exactly
    // `cluster.session_mirror_ttl_secs` and then disappears together with its locator.

    /// Writes (or refreshes) the mirror of a session this node owns. Refuses — without
    /// writing — when the locator names another node, i.e. the session was adopted elsewhere
    /// while this node still believed it owned it.
    pub async fn write_session_mirror(
        &self,
        mirror: &SessionMirror,
        ttl_secs: u64,
    ) -> Result<bool> {
        let json = serde_json::to_string(mirror)
            .map_err(|e| AurixError::Internal(format!("mirror encode: {e}")))?;
        let mut conn = self.conn().await?;
        let owner_key = session_owner_key(mirror.session_id);
        let mirror_key = session_mirror_key(mirror.session_id);
        let script = redis::Script::new(
            r#"
            local owner = redis.call('GET', KEYS[1])
            if owner and owner ~= ARGV[1] then return 0 end
            redis.call('SET', KEYS[1], ARGV[1], 'EX', ARGV[3])
            redis.call('SET', KEYS[2], ARGV[2], 'EX', ARGV[3])
            return 1
            "#,
        );
        let ok: i64 = Self::with_timeout(
            script
                .key(&owner_key)
                .key(&mirror_key)
                .arg(self.node_id.0.to_string())
                .arg(&json)
                .arg(ttl_secs.max(1))
                .invoke_async(&mut conn),
            "write_session_mirror",
        )
        .await?;
        Ok(ok == 1)
    }

    pub async fn get_session_mirror(&self, session_id: SessionId) -> Result<Option<SessionMirror>> {
        let mut conn = self.conn().await?;
        let key = session_mirror_key(session_id);
        let raw: Option<String> = Self::with_timeout(conn.get(&key), "get_session_mirror").await?;
        Ok(raw.and_then(|s| serde_json::from_str(&s).ok()))
    }

    /// Compare-and-set of the owner: succeeds only if the session is still owned by
    /// `expected` (or by nobody, when the locator expired but the mirror is still there).
    /// The winner becomes the owner; the mirror is left for it to rewrite.
    pub async fn claim_session(
        &self,
        session_id: SessionId,
        expected: MediaNodeId,
        ttl_secs: u64,
    ) -> Result<std::result::Result<(), TakeoverRefused>> {
        let mut conn = self.conn().await?;
        let owner_key = session_owner_key(session_id);
        let mirror_key = session_mirror_key(session_id);
        let script = redis::Script::new(
            r#"
            if redis.call('EXISTS', KEYS[2]) == 0 then return -1 end
            local owner = redis.call('GET', KEYS[1])
            if owner and owner ~= ARGV[1] and owner ~= ARGV[2] then return 0 end
            redis.call('SET', KEYS[1], ARGV[2], 'EX', ARGV[3])
            return 1
            "#,
        );
        let res: i64 = Self::with_timeout(
            script
                .key(&owner_key)
                .key(&mirror_key)
                .arg(expected.0.to_string())
                .arg(self.node_id.0.to_string())
                .arg(ttl_secs.max(1))
                .invoke_async(&mut conn),
            "claim_session",
        )
        .await?;
        Ok(match res {
            1 => Ok(()),
            -1 => Err(TakeoverRefused::NotMirrored),
            _ => Err(TakeoverRefused::Raced),
        })
    }

    /// Removes locator and mirror, but only if this node owns the session (a session closed
    /// for good; a stale former owner must not erase a migrated session).
    pub async fn delete_session_mirror(&self, session_id: SessionId) -> Result<bool> {
        self.release_session_claim(session_id, None).await
    }

    /// Undoes a claim this node cannot honour: hands the owner key back to `previous` (the
    /// mirror stays, so that node — or the next taker — can carry on) or, with `None`,
    /// removes locator and mirror. A no-op when somebody else owns the session by now.
    pub async fn release_session_claim(
        &self,
        session_id: SessionId,
        previous: Option<MediaNodeId>,
    ) -> Result<bool> {
        let mut conn = self.conn().await?;
        let owner_key = session_owner_key(session_id);
        let mirror_key = session_mirror_key(session_id);
        let script = redis::Script::new(
            r#"
            local owner = redis.call('GET', KEYS[1])
            if owner and owner ~= ARGV[1] then return 0 end
            if ARGV[2] == '' then
                redis.call('DEL', KEYS[1], KEYS[2])
            else
                local ttl = redis.call('TTL', KEYS[2])
                if ttl > 0 then
                    redis.call('SET', KEYS[1], ARGV[2], 'EX', ttl)
                else
                    redis.call('DEL', KEYS[1], KEYS[2])
                end
            end
            return 1
            "#,
        );
        let ok: i64 = Self::with_timeout(
            script
                .key(&owner_key)
                .key(&mirror_key)
                .arg(self.node_id.0.to_string())
                .arg(previous.map(|n| n.0.to_string()).unwrap_or_default())
                .invoke_async(&mut conn),
            "release_session_claim",
        )
        .await?;
        Ok(ok == 1)
    }

    /// Node liveness beacon (`node:{id}:alive`, TTL `ttl_secs`), independent of the database
    /// registry so a replacement node can tell a crashed node from a slow one.
    pub async fn beacon_alive(&self, ttl_secs: u64) -> Result<()> {
        let mut conn = self.conn().await?;
        let key = format!("node:{}:alive", self.node_id);
        Self::with_timeout(
            conn.set_ex::<_, _, ()>(&key, "1", ttl_secs.max(1)),
            "beacon_alive",
        )
        .await?;
        Ok(())
    }

    pub async fn is_node_alive(&self, node_id: MediaNodeId) -> Result<bool> {
        let mut conn = self.conn().await?;
        let key = format!("node:{}:alive", node_id);
        let n: i64 = Self::with_timeout(conn.exists(&key), "is_node_alive").await?;
        Ok(n > 0)
    }

    /// One node at a time reaps a lost node: `SET NX` on `reap:{node}` for `ttl_secs`.
    pub async fn claim_reaper(&self, lost: MediaNodeId, ttl_secs: u64) -> Result<bool> {
        self.claim_once(&format!("reap:{lost}"), ttl_secs).await
    }

    /// Record which channels a user is in (for cross-node queries).
    pub async fn add_user_channel(&self, user_id: UserId, channel_id: ChannelId) -> Result<()> {
        let mut conn = self.conn().await?;
        let key = user_channels_key(user_id);
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
        let key = user_channels_key(user_id);
        Self::with_timeout(
            conn.srem::<_, _, ()>(&key, channel_id.0.to_string()),
            "remove_user_channel",
        )
        .await?;
        Ok(())
    }

    pub async fn get_user_channels(&self, user_id: UserId) -> Result<Vec<ChannelId>> {
        let mut conn = self.conn().await?;
        let key = user_channels_key(user_id);
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

    /// Publish an event to all nodes via Redis Pub/Sub (sharded on a cluster when
    /// `redis.sharded_pubsub` is on, so it stays on the channel's shard instead of crossing
    /// the cluster bus).
    pub async fn publish_event(&self, event_json: &str) -> Result<()> {
        let mut conn = self.conn().await?;
        if self.source.sharded_pubsub() {
            Self::with_timeout(
                redis::cmd("SPUBLISH")
                    .arg(EVENT_CHANNEL)
                    .arg(event_json)
                    .query_async::<()>(&mut conn),
                "spublish",
            )
            .await
        } else {
            Self::with_timeout(
                conn.publish::<_, _, ()>(EVENT_CHANNEL, event_json),
                "publish",
            )
            .await
        }
    }

    /// Cluster only: `host:port` of the master currently serving the event channel's slot
    /// (`None` on a single Redis). Diagnostics: this is the shard whose loss interrupts
    /// cross-node events until the cluster promotes its replica.
    pub async fn event_shard_master(&self) -> Result<Option<String>> {
        if !self.is_cluster() {
            return Ok(None);
        }
        let mut conn = self.conn().await?;
        let slot: i64 = Self::with_timeout(
            redis::cmd("CLUSTER")
                .arg("KEYSLOT")
                .arg(EVENT_CHANNEL)
                .query_async(&mut conn),
            "cluster keyslot",
        )
        .await?;
        let ranges: Vec<Vec<redis::Value>> = Self::with_timeout(
            redis::cmd("CLUSTER").arg("SLOTS").query_async(&mut conn),
            "cluster slots",
        )
        .await?;
        for range in ranges {
            let (Some(redis::Value::Int(start)), Some(redis::Value::Int(end)), Some(master)) =
                (range.first(), range.get(1), range.get(2))
            else {
                continue;
            };
            if !(*start..=*end).contains(&slot) {
                continue;
            }
            let (host, port): (String, u16) = match master {
                redis::Value::Array(parts) if parts.len() >= 2 => (
                    redis::from_redis_value(&parts[0])
                        .map_err(|e| AurixError::Redis(format!("cluster slots host: {e}")))?,
                    redis::from_redis_value(&parts[1])
                        .map_err(|e| AurixError::Redis(format!("cluster slots port: {e}")))?,
                ),
                _ => continue,
            };
            return Ok(Some(format!("{host}:{port}")));
        }
        Ok(None)
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

    /// Fleet-wide token bucket: atomically takes `cost` tokens from `key`'s bucket (`rate`
    /// tokens/s, capacity `burst`) using the Redis clock, so every node sees the same bucket.
    /// Returns `Ok(None)` when admitted, `Ok(Some(wait))` when the caller must wait. The bucket
    /// expires once it would be full again.
    pub async fn take_tokens(
        &self,
        key: &str,
        rate: f64,
        burst: f64,
        cost: f64,
    ) -> Result<Option<Duration>> {
        let mut conn = self.conn().await?;
        let redis_key = format!("bucket:{{{key}}}");
        let script = redis::Script::new(
            r#"
            local rate = tonumber(ARGV[1])
            local burst = tonumber(ARGV[2])
            local cost = tonumber(ARGV[3])
            local t = redis.call('TIME')
            local now = tonumber(t[1]) * 1000 + math.floor(tonumber(t[2]) / 1000)
            local b = redis.call('HMGET', KEYS[1], 'tokens', 'at')
            local tokens = tonumber(b[1])
            local at = tonumber(b[2])
            if tokens == nil or at == nil then
                tokens = burst
                at = now
            elseif now > at then
                tokens = math.min(burst, tokens + (now - at) * rate / 1000)
                at = now
            end
            local wait = 0
            if tokens >= cost then
                tokens = tokens - cost
            elseif rate > 0 then
                wait = math.ceil((cost - tokens) * 1000 / rate)
            else
                wait = -1
            end
            redis.call('HSET', KEYS[1], 'tokens', tostring(tokens), 'at', tostring(at))
            local ttl = 1000
            if rate > 0 then ttl = ttl + math.ceil(burst * 1000 / rate) end
            redis.call('PEXPIRE', KEYS[1], ttl)
            return wait
            "#,
        );
        let wait: i64 = Self::with_timeout(
            script
                .key(&redis_key)
                .arg(rate)
                .arg(burst)
                .arg(cost)
                .invoke_async(&mut conn),
            "take_tokens",
        )
        .await?;
        Ok(match wait {
            0 => None,
            w if w < 0 => Some(Duration::MAX),
            w => Some(Duration::from_millis(w as u64)),
        })
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
        let key = user_server_muted_key(user_id);
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
        let key = user_server_muted_key(user_id);
        let val: Option<String> = Self::with_timeout(conn.get(&key), "is_globally_muted").await?;
        Ok(val.is_some())
    }

    /// Drops every per-user key (user erasure).
    pub async fn forget_user(&self, user_id: UserId) -> Result<()> {
        let mut conn = self.conn().await?;
        let keys = [user_server_muted_key(user_id), user_channels_key(user_id)];
        Self::with_timeout(conn.del::<_, ()>(&keys[..]), "forget_user").await?;
        Ok(())
    }
}

/// Envelope decoding plus a bounded window of recently seen event ids, so an event that
/// arrives twice (a re-subscription overlapping a publish, a duplicated push) is delivered once.
#[derive(Default)]
struct SeenEvents {
    ids: std::collections::VecDeque<uuid::Uuid>,
}

impl SeenEvents {
    const WINDOW: usize = 4096;

    fn accept(&mut self, payload: &str, self_id: MediaNodeId) -> Option<ServerEvent> {
        let Ok(env) = serde_json::from_str::<EventEnvelope>(payload) else {
            tracing::debug!("ignoring malformed cross-node event");
            return None;
        };
        if env.origin == self_id || self.ids.contains(&env.id) {
            return None;
        }
        if self.ids.len() == Self::WINDOW {
            self.ids.pop_front();
        }
        self.ids.push_back(env.id);
        Some(env.event)
    }
}
