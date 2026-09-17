use aurix_common::error::{AurixError, Result};
use aurix_common::types::*;
use aurix_db::models::MediaNodeRow;
use aurix_db::DbPool;
use chrono::Utc;
use dashmap::DashMap;
use std::sync::Arc;
use tracing::{info, warn};

const STALE_NODE_RETENTION_SECS: i64 = 24 * 3600;
const HEARTBEAT_TIMEOUT_SECS: i64 = 30;

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
                // Never let a stale DB row downgrade a fresher local heartbeat.
                let keep_local = self
                    .nodes
                    .get(&info.id)
                    .map(|cur| cur.last_heartbeat >= info.last_heartbeat)
                    .unwrap_or(false);
                if !keep_local {
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
            media_port: row.media_port as u16,
            api_port: row.api_port as u16,
            cascade_port: row.cascade_port.map(|p| p as u16),
            active_channels: row.active_channels.max(0) as u32,
            active_participants: row.active_participants.max(0) as u32,
            cpu_usage: row.cpu_usage as f32,
            memory_usage: row.memory_usage as f32,
            bandwidth_in_mbps: row.bandwidth_in_mbps as f32,
            bandwidth_out_mbps: row.bandwidth_out_mbps as f32,
            healthy: row.healthy && age <= HEARTBEAT_TIMEOUT_SECS,
            last_heartbeat: row.last_heartbeat,
            capacity: row.capacity.max(0) as u32,
        }
    }

    pub async fn register_node(&self, info: MediaNodeInfo) -> Result<()> {
        let row = MediaNodeRow {
            id: info.id.0,
            region: info.region.as_str().to_string(),
            address: info.address.clone(),
            media_port: info.media_port as i32,
            api_port: info.api_port as i32,
            cascade_port: info.cascade_port.map(i32::from),
            capacity: info.capacity as i32,
            active_channels: info.active_channels as i32,
            active_participants: info.active_participants as i32,
            cpu_usage: info.cpu_usage as f64,
            memory_usage: info.memory_usage as f64,
            bandwidth_in_mbps: info.bandwidth_in_mbps as f64,
            bandwidth_out_mbps: info.bandwidth_out_mbps as f64,
            healthy: info.healthy,
            version: env!("CARGO_PKG_VERSION").to_string(),
            last_heartbeat: Utc::now(),
            registered_at: Utc::now(),
        };

        aurix_db::queries::upsert_media_node(&self.pool, &row)
            .await
            .map_err(|e| AurixError::Database(format!("Failed to register node: {e}")))?;

        self.nodes.insert(info.id, info);
        Ok(())
    }

    pub async fn heartbeat(&self, node_id: MediaNodeId, info: MediaNodeInfo) -> Result<()> {
        self.nodes.insert(node_id, info.clone());

        let row = MediaNodeRow {
            id: info.id.0,
            region: info.region.as_str().to_string(),
            address: info.address,
            media_port: info.media_port as i32,
            api_port: info.api_port as i32,
            cascade_port: info.cascade_port.map(i32::from),
            capacity: info.capacity as i32,
            active_channels: info.active_channels as i32,
            active_participants: info.active_participants as i32,
            cpu_usage: info.cpu_usage as f64,
            memory_usage: info.memory_usage as f64,
            bandwidth_in_mbps: info.bandwidth_in_mbps as f64,
            bandwidth_out_mbps: info.bandwidth_out_mbps as f64,
            healthy: info.healthy,
            version: env!("CARGO_PKG_VERSION").to_string(),
            last_heartbeat: Utc::now(),
            registered_at: Utc::now(),
        };

        aurix_db::queries::upsert_media_node(&self.pool, &row)
            .await
            .map_err(|e| AurixError::Database(format!("Node heartbeat failed: {e}")))?;

        Ok(())
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

    pub fn get_all_nodes(&self) -> Vec<MediaNodeInfo> {
        self.nodes.iter().map(|e| e.value().clone()).collect()
    }

    pub fn get_node(&self, id: &MediaNodeId) -> Option<MediaNodeInfo> {
        self.nodes.get(id).map(|e| e.value().clone())
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
