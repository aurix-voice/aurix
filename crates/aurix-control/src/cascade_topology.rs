//! Automatic cascade topology.
//!
//! Instead of a hand-written `media.cascade_peers` list, every node advertises its cascade
//! UDP port in `media_nodes` and the topology is derived from the database:
//!
//! * allowed peers  = healthy nodes (fresh heartbeat) other than this one;
//! * per-channel peers = nodes that currently hold a live `channel_memberships` row for a
//!   channel this node also hosts.
//!
//! Reconciliation runs periodically and immediately after remote `ParticipantJoined` /
//! `ParticipantLeft` / `NodeHealthChanged` events, so a channel spanning two nodes starts
//! relaying within one event round-trip rather than a full interval.

use crate::event_bus::{EventBus, ServerEvent};
use crate::node_manager::NodeManager;
use aurix_common::types::{ChannelId, MediaNodeId, MediaNodeInfo};
use aurix_db::DbPool;
use aurix_media::cascade::CascadeRelay;
use aurix_media::sfu::SfuNode;
use parking_lot::RwLock;
use std::collections::{HashMap, HashSet};
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::broadcast::error::RecvError;
use tokio_util::sync::CancellationToken;
use tracing::{debug, warn};

pub struct CascadeTopology {
    pool: DbPool,
    nodes: Arc<NodeManager>,
    local_node: MediaNodeId,
    cascade: Arc<CascadeRelay>,
    sfu: Arc<RwLock<SfuNode>>,
}

/// Cascade socket address of a registered node, or `None` if it does not run a cascade relay
/// or its address cannot be resolved.
pub async fn cascade_addr_of(node: &MediaNodeInfo) -> Option<SocketAddr> {
    let port = node.cascade_port?;
    if let Ok(ip) = node.address.parse::<IpAddr>() {
        return Some(SocketAddr::new(ip, port));
    }
    match tokio::net::lookup_host((node.address.as_str(), port)).await {
        Ok(mut addrs) => addrs.next(),
        Err(e) => {
            warn!(
                "Cannot resolve cascade address {} of node {}: {}",
                node.address, node.id, e
            );
            None
        }
    }
}

impl CascadeTopology {
    pub fn new(
        pool: DbPool,
        nodes: Arc<NodeManager>,
        local_node: MediaNodeId,
        cascade: Arc<CascadeRelay>,
        sfu: Arc<RwLock<SfuNode>>,
    ) -> Self {
        Self {
            pool,
            nodes,
            local_node,
            cascade,
            sfu,
        }
    }

    /// Healthy remote nodes with a cascade port, keyed by node id.
    async fn peer_addresses(&self) -> HashMap<MediaNodeId, SocketAddr> {
        let mut out = HashMap::new();
        for node in self.nodes.get_all_nodes() {
            if node.id == self.local_node || !node.healthy {
                continue;
            }
            if let Some(addr) = cascade_addr_of(&node).await {
                out.insert(node.id, addr);
            }
        }
        out
    }

    /// One reconciliation pass. Returns the number of channels that have remote peers.
    pub async fn reconcile(&self) -> usize {
        self.nodes.refresh_from_db().await;
        let peers = self.peer_addresses().await;
        self.cascade
            .set_dynamic_peers(peers.values().copied().collect::<HashSet<_>>());

        let local_channels: Vec<ChannelId> = self.sfu.read().channel_ids();
        let ids: Vec<uuid::Uuid> = local_channels.iter().map(|c| c.0).collect();
        let rows =
            match aurix_db::queries::remote_nodes_for_channels(&self.pool, &ids, self.local_node.0)
                .await
            {
                Ok(rows) => rows,
                Err(e) => {
                    warn!("Cascade topology query failed: {}", e);
                    return 0;
                }
            };

        let mut by_channel: HashMap<ChannelId, Vec<SocketAddr>> = HashMap::new();
        for (channel, node) in rows {
            if let Some(addr) = peers.get(&MediaNodeId::from_uuid(node)) {
                by_channel
                    .entry(ChannelId::from_uuid(channel))
                    .or_default()
                    .push(*addr);
            }
        }
        let mut with_peers = 0;
        for channel in local_channels {
            let mut addrs = by_channel.remove(&channel).unwrap_or_default();
            addrs.sort_unstable();
            addrs.dedup();
            for static_peer in self.cascade.static_peers() {
                if !addrs.contains(&static_peer) {
                    addrs.push(static_peer);
                }
            }
            if !addrs.is_empty() {
                with_peers += 1;
            }
            self.cascade.set_channel_peers(channel, &addrs);
        }
        debug!(
            "Cascade topology: {} peers, {} channels relayed",
            peers.len(),
            with_peers
        );
        with_peers
    }

    /// Run until cancelled: periodic reconciliation plus event-triggered fast path.
    pub async fn run(
        self: Arc<Self>,
        events: Arc<EventBus>,
        interval: Duration,
        cancel: CancellationToken,
    ) {
        let mut rx = events.subscribe();
        let mut ticker = tokio::time::interval(interval.max(Duration::from_millis(500)));
        // Coalesce bursts of events into one pass.
        let debounce = Duration::from_millis(150);
        loop {
            tokio::select! {
                _ = cancel.cancelled() => return,
                _ = ticker.tick() => {
                    self.reconcile().await;
                }
                ev = rx.recv() => {
                    match ev {
                        Ok(ev) if Self::affects_topology(&ev) => {
                            tokio::time::sleep(debounce).await;
                            while let Ok(_more) = rx.try_recv() {}
                            self.reconcile().await;
                        }
                        Ok(_) => {}
                        Err(RecvError::Lagged(n)) => {
                            debug!("Cascade topology event stream lagged by {}", n);
                            self.reconcile().await;
                        }
                        Err(RecvError::Closed) => return,
                    }
                }
            }
        }
    }

    fn affects_topology(ev: &ServerEvent) -> bool {
        matches!(
            ev,
            ServerEvent::ParticipantJoined { .. }
                | ServerEvent::ParticipantLeft { .. }
                | ServerEvent::ChannelDestroyed { .. }
                | ServerEvent::NodeHealthChanged { .. }
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;

    fn node(addr: &str, cascade_port: Option<u16>) -> MediaNodeInfo {
        MediaNodeInfo {
            id: MediaNodeId::new(),
            region: aurix_common::types::Region::EuWest,
            address: addr.to_string(),
            media_port: 9000,
            api_port: 8080,
            cascade_port,
            active_channels: 0,
            active_participants: 0,
            cpu_usage: 0.0,
            memory_usage: 0.0,
            bandwidth_in_mbps: 0.0,
            bandwidth_out_mbps: 0.0,
            healthy: true,
            last_heartbeat: Utc::now(),
            capacity: 100,
        }
    }

    #[tokio::test]
    async fn cascade_addr_uses_ip_or_resolves_hostname() {
        assert_eq!(
            cascade_addr_of(&node("10.1.2.3", Some(9001))).await,
            Some("10.1.2.3:9001".parse().unwrap())
        );
        assert_eq!(cascade_addr_of(&node("10.1.2.3", None)).await, None);
        let resolved = cascade_addr_of(&node("localhost", Some(9001)))
            .await
            .unwrap();
        assert!(resolved.ip().is_loopback());
        assert_eq!(resolved.port(), 9001);
    }
}
