use aurix_common::error::{AurixError, Result};
use aurix_common::types::*;
use aurix_db::models::MediaNodeRow;
use aurix_db::DbPool;
use chrono::Utc;
use dashmap::DashMap;
use std::cmp::Ordering;
use std::collections::HashMap;
use std::sync::Arc;
use tracing::{info, warn};

const STALE_NODE_RETENTION_SECS: i64 = 24 * 3600;
const HEARTBEAT_TIMEOUT_SECS: i64 = 30;

/// What the caller knows about the client when asking for a region.
#[derive(Debug, Clone, Copy, Default)]
pub struct SelectionHint {
    /// Region the game prefers (matchmaking region, player setting). Wins when it has capacity.
    pub region: Option<Region>,
    /// Client's approximate coordinates (from the game's own geo data); ignored when invalid.
    pub location: Option<GeoLocation>,
}

pub struct NodeManager {
    pool: DbPool,
    nodes: Arc<DashMap<MediaNodeId, MediaNodeInfo>>,
}

impl NodeManager {
    pub fn new(pool: DbPool) -> Self {
        Self {
            pool,
            nodes: Arc::new(DashMap::new()),
        }
    }

    /// Seed the in-memory registry from the database (other nodes' heartbeats).
    pub async fn load_from_db(&self) -> Result<usize> {
        let rows = aurix_db::queries::get_all_media_nodes(&self.pool)
            .await
            .map_err(|e| AurixError::Database(format!("Failed to load media nodes: {e}")))?;
        let mut loaded = 0;
        for row in rows {
            let info = Self::info_from_row(&row);
            self.nodes.insert(info.id, info);
            loaded += 1;
        }
        Ok(loaded)
    }

    /// Refresh the registry from the database so nodes registered by other processes are visible.
    pub async fn refresh_from_db(&self) {
        if let Ok(rows) = aurix_db::queries::get_all_media_nodes(&self.pool).await {
            let db_ids: std::collections::HashSet<MediaNodeId> =
                rows.iter().map(|r| MediaNodeId::from_uuid(r.id)).collect();
            for row in rows {
                let info = Self::info_from_row(&row);
                // Never let a stale DB row downgrade a fresher local heartbeat; the drain flag
                // is operator-owned and always authoritative in the database.
                let kept_local = self
                    .nodes
                    .get_mut(&info.id)
                    .filter(|cur| cur.last_heartbeat >= info.last_heartbeat)
                    .map(|mut cur| cur.drain = info.drain.clone())
                    .is_some();
                if !kept_local {
                    self.nodes.insert(info.id, info);
                }
            }
            self.nodes.retain(|id, _| db_ids.contains(id));
        }
    }

    fn info_from_row(row: &MediaNodeRow) -> MediaNodeInfo {
        let age = Utc::now()
            .signed_duration_since(row.last_heartbeat)
            .num_seconds();
        MediaNodeInfo {
            id: MediaNodeId::from_uuid(row.id),
            region: Region::from_str_loose(&row.region),
            address: row.address.clone(),
            address_ipv6: row.address_ipv6.clone().filter(|a| !a.is_empty()),
            media_port: row.media_port as u16,
            api_port: row.api_port as u16,
            cascade_port: row.cascade_port.map(|p| p as u16),
            ws_url: row.ws_url.clone().filter(|u| !u.is_empty()),
            api_url: row.api_url.clone().filter(|u| !u.is_empty()),
            location: match (row.latitude, row.longitude) {
                (Some(latitude), Some(longitude)) => Some(GeoLocation {
                    latitude,
                    longitude,
                }),
                _ => None,
            },
            active_channels: row.active_channels.max(0) as u32,
            active_participants: row.active_participants.max(0) as u32,
            cpu_usage: row.cpu_usage as f32,
            memory_usage: row.memory_usage as f32,
            bandwidth_in_mbps: row.bandwidth_in_mbps as f32,
            bandwidth_out_mbps: row.bandwidth_out_mbps as f32,
            healthy: row.healthy && age <= HEARTBEAT_TIMEOUT_SECS,
            last_heartbeat: row.last_heartbeat,
            capacity: row.capacity.max(0) as u32,
            relay_only: row.relay_only,
            drain: Self::drain_from_row(row),
        }
    }

    fn drain_from_row(row: &MediaNodeRow) -> Option<NodeDrain> {
        row.draining.then(|| NodeDrain {
            reason: row.drain_reason.clone().filter(|r| !r.is_empty()),
            since: row.draining_since.unwrap_or(row.last_heartbeat),
            by: row.drained_by,
        })
    }

    fn row_from_info(info: &MediaNodeInfo) -> MediaNodeRow {
        MediaNodeRow {
            id: info.id.0,
            region: info.region.as_str().to_string(),
            address: info.address.clone(),
            address_ipv6: info.address_ipv6.clone(),
            media_port: info.media_port as i32,
            api_port: info.api_port as i32,
            cascade_port: info.cascade_port.map(i32::from),
            ws_url: info.ws_url.clone(),
            api_url: info.api_url.clone(),
            latitude: info.location.map(|l| l.latitude),
            longitude: info.location.map(|l| l.longitude),
            capacity: info.capacity as i32,
            active_channels: info.active_channels as i32,
            active_participants: info.active_participants as i32,
            cpu_usage: info.cpu_usage as f64,
            memory_usage: info.memory_usage as f64,
            bandwidth_in_mbps: info.bandwidth_in_mbps as f64,
            bandwidth_out_mbps: info.bandwidth_out_mbps as f64,
            healthy: info.healthy,
            relay_only: info.relay_only,
            draining: info.drain.is_some(),
            drain_reason: info.drain.as_ref().and_then(|d| d.reason.clone()),
            draining_since: info.drain.as_ref().map(|d| d.since),
            drained_by: info.drain.as_ref().and_then(|d| d.by),
            version: env!("CARGO_PKG_VERSION").to_string(),
            last_heartbeat: Utc::now(),
            registered_at: Utc::now(),
        }
    }

    /// Upserts the node's own row. The drain columns are operator-owned and not part of the
    /// upsert, so the persisted state is read back and applied to the local entry.
    async fn upsert_self(&self, info: MediaNodeInfo, what: &str) -> Result<()> {
        let row = Self::row_from_info(&info);
        let id = info.id;
        self.nodes.insert(id, info);
        let stored = aurix_db::queries::upsert_media_node(&self.pool, &row)
            .await
            .map_err(|e| AurixError::Database(format!("{what} failed: {e}")))?;
        let drain = Self::drain_from_row(&stored);
        if let Some(mut node) = self.nodes.get_mut(&id) {
            if node.drain.is_some() != drain.is_some() {
                match &drain {
                    Some(d) => info!(
                        "Node {id} is draining (reason: {})",
                        d.reason.as_deref().unwrap_or("-")
                    ),
                    None => info!("Node {id} drain ended"),
                }
            }
            node.drain = drain;
        }
        Ok(())
    }

    pub async fn register_node(&self, info: MediaNodeInfo) -> Result<()> {
        self.upsert_self(info, "Node registration").await
    }

    pub async fn heartbeat(&self, node_id: MediaNodeId, info: MediaNodeInfo) -> Result<()> {
        debug_assert_eq!(node_id, info.id);
        self.upsert_self(info, "Node heartbeat").await
    }

    /// Operator drain / undrain of any node in the fleet. The target node observes the change
    /// on its next heartbeat (`media.heartbeat_interval_ms`); this node applies it at once.
    /// `Ok(None)` when the node is unknown.
    pub async fn set_drain(
        &self,
        node_id: MediaNodeId,
        drain: Option<(Option<&str>, uuid::Uuid)>,
    ) -> Result<Option<MediaNodeInfo>> {
        let row = aurix_db::queries::set_media_node_drain(&self.pool, node_id.0, drain)
            .await
            .map_err(|e| AurixError::Database(format!("Failed to update node drain: {e}")))?;
        let Some(row) = row else {
            return Ok(None);
        };
        let stored = Self::drain_from_row(&row);
        let info = match self.nodes.get_mut(&node_id) {
            Some(mut node) => {
                node.drain = stored;
                node.clone()
            }
            None => {
                let info = Self::info_from_row(&row);
                self.nodes.insert(node_id, info.clone());
                info
            }
        };
        Ok(Some(info))
    }

    /// Whether `node_id` (normally this node) is in an operator drain.
    pub fn is_draining(&self, node_id: MediaNodeId) -> bool {
        self.nodes.get(&node_id).is_some_and(|n| n.is_draining())
    }

    pub fn select_node(&self, region: Region) -> Result<MediaNodeInfo> {
        let mut best: Option<MediaNodeInfo> = None;
        let mut best_load = f32::MAX;

        for entry in self.nodes.iter() {
            let node = entry.value();
            if node.region == region && node.is_available() {
                let load = node.load_factor();
                if load < best_load {
                    best_load = load;
                    best = Some(node.clone());
                }
            }
        }

        // Fallback: any available node
        if best.is_none() {
            for entry in self.nodes.iter() {
                let node = entry.value();
                if node.is_available() {
                    let load = node.load_factor();
                    if load < best_load {
                        best_load = load;
                        best = Some(node.clone());
                    }
                }
            }
        }

        best.ok_or_else(|| AurixError::MediaNodeUnavailable("No available media nodes".into()))
    }

    /// Region discovery for clients. One entry per region that has at least one healthy,
    /// non-saturated node advertising a public WebSocket URL; the entry carries that region's
    /// least-loaded node. Ordered best-first: the preferred region (if it has capacity), then by
    /// distance to the client's declared location (nodes without coordinates sort last), then
    /// by load. Callers that can measure RTT should still probe `probe_url` and pick the lowest.
    pub fn regions(&self, hint: &SelectionHint) -> Vec<RegionEndpoint> {
        let mut per_region: HashMap<Region, (MediaNodeInfo, u32)> = HashMap::new();
        for entry in self.nodes.iter() {
            let node = entry.value();
            let Some(ws_url) = node.ws_url.as_deref() else {
                continue;
            };
            if ws_url.is_empty() || !node.is_available() {
                continue;
            }
            match per_region.get_mut(&node.region) {
                Some((best, count)) => {
                    *count += 1;
                    if node.load_factor() < best.load_factor() {
                        *best = node.clone();
                    }
                }
                None => {
                    per_region.insert(node.region, (node.clone(), 1));
                }
            }
        }

        let client = hint.location.filter(GeoLocation::is_valid);
        let mut endpoints: Vec<RegionEndpoint> = per_region
            .into_values()
            .map(|(node, count)| RegionEndpoint {
                region: node.region,
                node_id: node.id,
                probe_url: node.api_url.as_deref().map(|api| format!("{api}/health")),
                distance_km: match (client, node.location) {
                    (Some(c), Some(n)) => Some(c.distance_km(&n)),
                    _ => None,
                },
                location: node.location,
                nodes: count,
                load_factor: node.load_factor(),
                ws_url: node.ws_url.clone().unwrap_or_default(),
            })
            .collect();

        endpoints.sort_by(|a, b| {
            let pref = |e: &RegionEndpoint| hint.region != Some(e.region);
            pref(a)
                .cmp(&pref(b))
                .then_with(|| match (a.distance_km, b.distance_km) {
                    (Some(x), Some(y)) => x.total_cmp(&y),
                    (Some(_), None) => Ordering::Less,
                    (None, Some(_)) => Ordering::Greater,
                    (None, None) => Ordering::Equal,
                })
                .then_with(|| a.load_factor.total_cmp(&b.load_factor))
                .then_with(|| a.region.as_str().cmp(b.region.as_str()))
        });
        endpoints
    }

    /// The best entry of [`Self::regions`], if any node is advertising an endpoint.
    pub fn recommend(&self, hint: &SelectionHint) -> Option<RegionEndpoint> {
        self.regions(hint).into_iter().next()
    }

    pub fn get_all_nodes(&self) -> Vec<MediaNodeInfo> {
        self.nodes.iter().map(|e| e.value().clone()).collect()
    }

    pub fn get_node(&self, id: &MediaNodeId) -> Option<MediaNodeInfo> {
        self.nodes.get(id).map(|e| e.value().clone())
    }

    /// Public WebSocket URLs of up to `limit` other healthy, non-saturated nodes for
    /// `SessionInitAck.failover`: same region as `self_id` first, then by load.
    pub fn failover_endpoints(&self, self_id: MediaNodeId, limit: usize) -> Vec<String> {
        if limit == 0 {
            return Vec::new();
        }
        let home = self.nodes.get(&self_id).map(|n| n.region);
        let mut candidates: Vec<(bool, f32, String)> = self
            .nodes
            .iter()
            .filter(|e| *e.key() != self_id)
            .filter_map(|e| {
                let node = e.value();
                let url = node.ws_url.as_deref().filter(|u| !u.is_empty())?;
                node.is_available().then(|| {
                    (
                        home != Some(node.region),
                        node.load_factor(),
                        url.to_string(),
                    )
                })
            })
            .collect();
        candidates.sort_by(|a, b| {
            a.0.cmp(&b.0)
                .then_with(|| a.1.total_cmp(&b.1))
                .then_with(|| a.2.cmp(&b.2))
        });
        candidates.into_iter().take(limit).map(|c| c.2).collect()
    }

    /// Nodes (other than `self_id`) whose last heartbeat is older than `after_secs`.
    pub fn lost_nodes(&self, self_id: MediaNodeId, after_secs: u64) -> Vec<MediaNodeId> {
        let now = Utc::now();
        self.nodes
            .iter()
            .filter(|e| *e.key() != self_id)
            .filter(|e| {
                now.signed_duration_since(e.value().last_heartbeat)
                    .num_seconds()
                    > after_secs as i64
            })
            .map(|e| *e.key())
            .collect()
    }

    /// Graceful shutdown: stop receiving new sessions immediately instead of waiting for the
    /// heartbeat timeout.
    pub async fn mark_offline(&self, node_id: MediaNodeId) {
        if let Some(mut node) = self.nodes.get_mut(&node_id) {
            node.healthy = false;
        }
        let _ = aurix_db::queries::mark_node_unhealthy(&self.pool, node_id.0).await;
    }

    pub async fn check_node_health(&self) {
        self.refresh_from_db().await;
        let now = Utc::now();
        let mut timed_out = Vec::new();
        let mut dead = Vec::new();

        for entry in self.nodes.iter() {
            let node = entry.value();
            let age = now.signed_duration_since(node.last_heartbeat).num_seconds();
            if age > STALE_NODE_RETENTION_SECS {
                dead.push(*entry.key());
            } else if age > HEARTBEAT_TIMEOUT_SECS {
                timed_out.push(*entry.key());
            }
        }

        for node_id in timed_out {
            let newly_unhealthy = self
                .nodes
                .get_mut(&node_id)
                .map(|mut node| std::mem::replace(&mut node.healthy, false))
                .unwrap_or(false);
            if newly_unhealthy {
                warn!("Node {} marked unhealthy (heartbeat timeout)", node_id);
                let _ = aurix_db::queries::mark_node_unhealthy(&self.pool, node_id.0).await;
            }
        }

        // Nodes silent for a long time are forgotten so the registry does not grow forever.
        for node_id in dead {
            self.nodes.remove(&node_id);
            if let Err(e) = aurix_db::queries::delete_media_node(&self.pool, node_id.0).await {
                warn!("Failed to prune stale node {node_id}: {e}");
            } else {
                info!("Pruned stale media node {node_id}");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manager() -> NodeManager {
        let pool = sqlx::postgres::PgPoolOptions::new()
            .connect_lazy("postgres://unused@127.0.0.1:1/unused")
            .expect("lazy pool");
        NodeManager::new(pool)
    }

    fn node(region: Region, ws: Option<&str>, load: u32, loc: Option<(f64, f64)>) -> MediaNodeInfo {
        MediaNodeInfo {
            id: MediaNodeId::new(),
            region,
            address: "10.0.0.1".into(),
            address_ipv6: None,
            media_port: 10000,
            api_port: 8080,
            cascade_port: None,
            ws_url: ws.map(str::to_string),
            api_url: ws.map(|_| format!("https://{}.example", region.as_str())),
            location: loc.map(|(latitude, longitude)| GeoLocation {
                latitude,
                longitude,
            }),
            active_channels: 0,
            active_participants: load,
            cpu_usage: 0.0,
            memory_usage: 0.0,
            bandwidth_in_mbps: 0.0,
            bandwidth_out_mbps: 0.0,
            healthy: true,
            last_heartbeat: Utc::now(),
            capacity: 100,
            relay_only: false,
            drain: None,
        }
    }

    fn insert(m: &NodeManager, n: MediaNodeInfo) -> MediaNodeId {
        let id = n.id;
        m.nodes.insert(id, n);
        id
    }

    #[tokio::test]
    async fn only_nodes_with_ws_url_and_capacity_are_advertised() {
        let m = manager();
        insert(&m, node(Region::EuWest, None, 0, None));
        insert(
            &m,
            node(Region::UsEast, Some("wss://us.example/ws"), 95, None),
        );
        let mut unhealthy = node(Region::UsWest, Some("wss://usw.example/ws"), 0, None);
        unhealthy.healthy = false;
        insert(&m, unhealthy);
        assert!(m.regions(&SelectionHint::default()).is_empty());
        assert!(m.recommend(&SelectionHint::default()).is_none());
    }

    #[tokio::test]
    async fn least_loaded_node_per_region_and_node_count() {
        let m = manager();
        insert(
            &m,
            node(Region::EuWest, Some("wss://eu1.example/ws"), 50, None),
        );
        let best = insert(
            &m,
            node(Region::EuWest, Some("wss://eu2.example/ws"), 10, None),
        );
        let regions = m.regions(&SelectionHint::default());
        assert_eq!(regions.len(), 1);
        assert_eq!(regions[0].node_id, best);
        assert_eq!(regions[0].nodes, 2);
        assert_eq!(regions[0].ws_url, "wss://eu2.example/ws");
        assert_eq!(
            regions[0].probe_url.as_deref(),
            Some("https://eu-west.example/health")
        );
        assert!((regions[0].load_factor - 0.1).abs() < 1e-6);
    }

    #[tokio::test]
    async fn draining_node_is_skipped_for_selection_discovery_and_failover() {
        let m = manager();
        let busy = insert(
            &m,
            node(Region::EuWest, Some("wss://eu1.example/ws"), 50, None),
        );
        let mut idle = node(Region::EuWest, Some("wss://eu2.example/ws"), 10, None);
        idle.drain = Some(NodeDrain {
            reason: Some("kernel upgrade".into()),
            since: Utc::now(),
            by: None,
        });
        assert!(!idle.is_available());
        let idle_id = insert(&m, idle);
        let self_id = insert(
            &m,
            node(Region::UsEast, Some("wss://us.example/ws"), 0, None),
        );

        assert_eq!(m.select_node(Region::EuWest).map(|n| n.id).ok(), Some(busy));
        let regions = m.regions(&SelectionHint::default());
        let eu = regions.iter().find(|r| r.region == Region::EuWest).unwrap();
        assert_eq!(eu.node_id, busy);
        assert_eq!(eu.nodes, 1);
        assert_eq!(
            m.failover_endpoints(self_id, 5),
            vec!["wss://eu1.example/ws".to_string()]
        );
        assert!(m.is_draining(idle_id));
        assert!(!m.is_draining(busy));
        // The overview still lists the node, with its drain state.
        assert!(m.get_all_nodes().iter().any(|n| n.id == idle_id
            && n.drain.as_ref().and_then(|d| d.reason.as_deref()) == Some("kernel upgrade")));
    }

    #[tokio::test]
    async fn preferred_region_wins_then_distance_then_load() {
        let m = manager();
        // Frankfurt, Virginia, Sydney, plus a region without coordinates.
        insert(
            &m,
            node(
                Region::EuCentral,
                Some("wss://eu/ws"),
                80,
                Some((50.11, 8.68)),
            ),
        );
        insert(
            &m,
            node(Region::UsEast, Some("wss://us/ws"), 5, Some((38.9, -77.0))),
        );
        insert(
            &m,
            node(
                Region::Australia,
                Some("wss://au/ws"),
                1,
                Some((-33.87, 151.21)),
            ),
        );
        insert(&m, node(Region::Africa, Some("wss://af/ws"), 0, None));

        // Client in Paris, no preference: nearest first, coordinate-less region last.
        let paris = SelectionHint {
            region: None,
            location: Some(GeoLocation {
                latitude: 48.85,
                longitude: 2.35,
            }),
        };
        let order: Vec<Region> = m.regions(&paris).iter().map(|r| r.region).collect();
        assert_eq!(
            order,
            vec![
                Region::EuCentral,
                Region::UsEast,
                Region::Australia,
                Region::Africa
            ]
        );
        let eu = &m.regions(&paris)[0];
        let d = eu.distance_km.expect("distance");
        assert!(
            (400.0..600.0).contains(&d),
            "Paris–Frankfurt ≈ 480 km, got {d}"
        );

        // Preferred region beats distance.
        let prefer_au = SelectionHint {
            region: Some(Region::Australia),
            ..paris
        };
        assert_eq!(m.recommend(&prefer_au).unwrap().region, Region::Australia);

        // Without a location: load order, and a saturated preferred region is skipped.
        let by_load: Vec<Region> = m
            .regions(&SelectionHint::default())
            .iter()
            .map(|r| r.region)
            .collect();
        assert_eq!(
            by_load,
            vec![
                Region::Africa,
                Region::Australia,
                Region::UsEast,
                Region::EuCentral
            ]
        );
        insert(
            &m,
            node(Region::SouthAmerica, Some("wss://sa/ws"), 99, None),
        );
        let hint = SelectionHint {
            region: Some(Region::SouthAmerica),
            location: None,
        };
        assert_eq!(m.recommend(&hint).unwrap().region, Region::Africa);
    }

    #[tokio::test]
    async fn invalid_client_location_is_ignored() {
        let m = manager();
        insert(
            &m,
            node(Region::EuWest, Some("wss://eu/ws"), 0, Some((51.5, -0.12))),
        );
        let hint = SelectionHint {
            region: None,
            location: Some(GeoLocation {
                latitude: 200.0,
                longitude: 0.0,
            }),
        };
        assert!(m.recommend(&hint).unwrap().distance_km.is_none());
    }
}
