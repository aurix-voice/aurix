use aurix_common::error::{AurixError, Result};
use aurix_common::types::*;
use aurix_db::models::MediaNodeRow;
use aurix_db::DbPool;
use chrono::Utc;
use dashmap::DashMap;
use std::sync::Arc;
use tracing::warn;

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

    pub async fn register_node(&self, info: MediaNodeInfo) -> Result<()> {
        let row = MediaNodeRow {
            id: info.id.0,
            region: format!("{:?}", info.region).to_lowercase().replace(' ', "-"),
            address: info.address.clone(),
            media_port: info.media_port as i32,
            api_port: info.api_port as i32,
            capacity: info.capacity as i32,
            active_channels: info.active_channels as i32,
            active_participants: info.active_participants as i32,
            cpu_usage: info.cpu_usage as f64,
            memory_usage: info.memory_usage as f64,
            bandwidth_in_mbps: info.bandwidth_in_mbps as f64,
            bandwidth_out_mbps: info.bandwidth_out_mbps as f64,
            healthy: info.healthy,
            version: "1.0.0".to_string(),
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
            region: format!("{:?}", info.region).to_lowercase().replace(' ', "-"),
            address: info.address,
            media_port: info.media_port as i32,
            api_port: info.api_port as i32,
            capacity: info.capacity as i32,
            active_channels: info.active_channels as i32,
            active_participants: info.active_participants as i32,
            cpu_usage: info.cpu_usage as f64,
            memory_usage: info.memory_usage as f64,
            bandwidth_in_mbps: info.bandwidth_in_mbps as f64,
            bandwidth_out_mbps: info.bandwidth_out_mbps as f64,
            healthy: info.healthy,
            version: "1.0.0".to_string(),
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

    pub async fn check_node_health(&self) {
        let mut unhealthy = Vec::new();

        for entry in self.nodes.iter() {
            let node = entry.value();
            let age = Utc::now()
                .signed_duration_since(node.last_heartbeat)
                .num_seconds();
            if age > 30 {
                unhealthy.push(*entry.key());
            }
        }

        for node_id in unhealthy {
            if let Some(mut node) = self.nodes.get_mut(&node_id) {
                node.healthy = false;
                warn!("Node {} marked unhealthy (heartbeat timeout)", node_id);
            }
            let _ = aurix_db::queries::mark_node_unhealthy(&self.pool, node_id.0).await;
        }
    }
}