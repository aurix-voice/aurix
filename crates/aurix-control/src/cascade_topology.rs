//! Automatic cascade topology.
//!
//! Instead of a hand-written `media.cascade_peers` list, every node advertises its cascade
//! UDP port in `media_nodes` and the topology is derived from the database:
//!
//! * allowed peers  = healthy nodes (fresh heartbeat) other than this one;
//! * per-channel routes = derived from the nodes that currently hold a live
//!   `channel_memberships` row for the channel, in one of two shapes
//!   (`media.cascade_topology`):
//!   * `mesh` — every hosting node sends its participants' audio directly to every other
//!     hosting node (one hop);
//!   * `region_tree` — hosting nodes in the same region still talk directly, but traffic
//!     between regions goes through one *hub* per region
//!     (origin → own hub → remote hub → hosting node, at most [`MAX_RELAY_HOPS`] hops).
//!     Hubs are chosen deterministically per channel from the registry *and* the measured
//!     link table every node publishes (`media_node_links`): nodes that reach everyone they
//!     must talk to first, `relay_only` nodes next, then the lowest RTT, tie-broken by a
//!     channel/node hash so hub duty spreads across the region. Two hubs that do not reach
//!     each other (or reach each other much slower than through a third hub) are joined via a
//!     *core* hub — one more level in the tree — and a region whose hosts cannot reach each
//!     other becomes a star around its hub. Every node computes the same tree from the same
//!     snapshot, and only hubs need inter-regional cascade reachability.
//!
//! Reconciliation runs periodically and immediately after remote `ParticipantJoined` /
//! `ParticipantLeft` / `SessionMigrated` / `NodeHealthChanged` events, so a channel spanning
//! two nodes starts relaying within one event round-trip rather than a full interval.

use crate::cascade_links::{
    LinkCost, LinkMatrix, RANK_BUCKET_MS, RELAY_GAIN_MS, UNCONFIRMED_LINK_MS,
};
use crate::event_bus::{EventBus, ServerEvent};
use crate::node_manager::NodeManager;
use aurix_common::addr::canonical;
use aurix_common::config::CascadeTopologyMode;
use aurix_common::net::BoundFamily;
use aurix_common::protocol::MAX_RELAY_HOPS;
use aurix_common::types::{ChannelId, MediaNodeId, MediaNodeInfo, Region};
use aurix_db::DbPool;
use aurix_media::cascade::{CascadeRelay, ChannelRoute};
use aurix_media::sfu::SfuNode;
use parking_lot::RwLock;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::broadcast::error::RecvError;
use tokio_util::sync::CancellationToken;
use tracing::{debug, warn};

pub struct CascadeTopology {
    pool: DbPool,
    nodes: Arc<NodeManager>,
    local: LocalNode,
    mode: CascadeTopologyMode,
    cascade: Arc<CascadeRelay>,
    sfu: Arc<RwLock<SfuNode>>,
    /// Reconciliation period; link rows older than a few periods are ignored.
    interval: Duration,
}

/// A remote node as the planner sees it: reachable cascade endpoint plus placement metadata.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PeerNode {
    pub addr: SocketAddr,
    pub region: Region,
    pub relay_only: bool,
}

/// Everything the planner needs about the local node.
#[derive(Debug, Clone, Copy)]
pub struct LocalNode {
    pub id: MediaNodeId,
    pub region: Region,
    pub relay_only: bool,
}

/// Channel/node mix used for deterministic tie-breaking (spreads hub duty across a region).
fn channel_node_mix(channel: &ChannelId, node: MediaNodeId) -> u128 {
    (channel.0.as_u128() ^ node.0.as_u128()).wrapping_mul(0x9E37_79B9_7F4A_7C15_F39C_C060_5CED_C835)
}

/// `(has unconfirmed link, !relay_only, RTT buckets, channel/node hash, id)` — lower wins.
type HubRank = (bool, bool, u32, u128, MediaNodeId);

/// Deterministic per-channel ranking of hub candidates: candidates that reach every node
/// they would have to talk to first, then relay-only nodes, then the lowest summed
/// (bucketed) RTT towards the region's hosts and the other regions, then a channel/node hash
/// so different channels pick different hubs when links are equal.
fn hub_rank(
    channel: &ChannelId,
    node: MediaNodeId,
    relay_only: bool,
    reach: &[LinkCost],
) -> HubRank {
    let unconfirmed = reach.iter().any(|c| c.is_unconfirmed());
    let bucket = reach.iter().map(|c| c.ms() / RANK_BUCKET_MS).sum();
    (
        unconfirmed,
        !relay_only,
        bucket,
        channel_node_mix(channel, node),
        node,
    )
}

/// Fleet-wide plan of one channel in `region_tree` mode, identical on every node that sees the
/// same registry and link snapshot. [`plan_region_tree`] projects it onto one node's route.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegionTreePlan {
    /// region → hosting nodes (only reachable ones).
    pub hosting_by_region: BTreeMap<Region, BTreeSet<MediaNodeId>>,
    /// region → elected hub (a relay-only node or one of the hosts).
    pub hubs: BTreeMap<Region, MediaNodeId>,
    /// Regions whose hosts do not all reach each other: in-region traffic goes via the hub.
    pub star_regions: BTreeSet<Region>,
    /// `(from_hub, to_hub)` → intermediate core hub carrying that direction.
    pub relays: BTreeMap<(MediaNodeId, MediaNodeId), MediaNodeId>,
}

impl RegionTreePlan {
    pub fn compute(
        channel: &ChannelId,
        local: LocalNode,
        hosting: &HashSet<MediaNodeId>,
        peers: &HashMap<MediaNodeId, PeerNode>,
        links: &LinkMatrix,
    ) -> Self {
        let mut hosting_by_region: BTreeMap<Region, BTreeSet<MediaNodeId>> = BTreeMap::new();
        for id in hosting {
            let region = if *id == local.id {
                local.region
            } else if let Some(p) = peers.get(id) {
                p.region
            } else {
                continue;
            };
            hosting_by_region.entry(region).or_default().insert(*id);
        }
        let relay_only_in = |region: Region| -> Vec<MediaNodeId> {
            let mut v: Vec<MediaNodeId> = peers
                .iter()
                .filter(|(_, p)| p.relay_only && p.region == region)
                .map(|(id, _)| *id)
                .collect();
            if local.relay_only && local.region == region {
                v.push(local.id);
            }
            v.sort_unstable();
            v
        };
        // Everything that may act as a region's hub: its relay nodes plus its hosts.
        let candidates_of: BTreeMap<Region, Vec<(MediaNodeId, bool)>> = hosting_by_region
            .iter()
            .map(|(region, hosts)| {
                let mut c: Vec<(MediaNodeId, bool)> = relay_only_in(*region)
                    .into_iter()
                    .map(|id| (id, true))
                    .collect();
                c.extend(hosts.iter().map(|id| (*id, false)));
                (*region, c)
            })
            .collect();

        let mut hubs: BTreeMap<Region, MediaNodeId> = BTreeMap::new();
        for (region, candidates) in &candidates_of {
            let hosts = &hosting_by_region[region];
            let mut best: Option<(HubRank, MediaNodeId)> = None;
            for (id, relay_only) in candidates {
                let mut reach: Vec<LinkCost> = hosts
                    .iter()
                    .filter(|h| *h != id)
                    .map(|h| links.cost(*id, *h))
                    .collect();
                for (other, others) in &candidates_of {
                    if other == region {
                        continue;
                    }
                    let nearest = others
                        .iter()
                        .map(|(n, _)| links.cost(*id, *n))
                        .min_by_key(|c| c.ms())
                        .unwrap_or(LinkCost::Unknown);
                    reach.push(nearest);
                }
                let rank = hub_rank(channel, *id, *relay_only, &reach);
                if best.as_ref().is_none_or(|(r, _)| rank < *r) {
                    best = Some((rank, *id));
                }
            }
            if let Some((_, id)) = best {
                hubs.insert(*region, id);
            }
        }

        // In-region star when two hosts (neither being the hub) cannot reach each other.
        let mut star_regions = BTreeSet::new();
        for (region, hosts) in &hosting_by_region {
            let hub = hubs.get(region);
            let others: Vec<MediaNodeId> =
                hosts.iter().copied().filter(|h| Some(h) != hub).collect();
            let blocked = others.iter().enumerate().any(|(i, a)| {
                others[i + 1..]
                    .iter()
                    .any(|b| links.cost(*a, *b).is_unconfirmed())
            });
            if blocked {
                star_regions.insert(*region);
            }
        }

        // Core hubs reach every other hub; they may carry traffic between two hubs that do not
        // reach each other (or reach each other much slower than via the core hub).
        let hub_ids: Vec<MediaNodeId> = hubs.values().copied().collect();
        let core: Vec<MediaNodeId> = hub_ids
            .iter()
            .copied()
            .filter(|c| {
                hub_ids
                    .iter()
                    .all(|z| z == c || !links.cost(*c, *z).is_unconfirmed())
            })
            .collect();
        let mut relays = BTreeMap::new();
        for x in &hub_ids {
            if core.contains(x) {
                continue;
            }
            for z in &hub_ids {
                if z == x {
                    continue;
                }
                let direct = links.cost(*x, *z);
                let budget = if direct.is_unconfirmed() {
                    UNCONFIRMED_LINK_MS
                } else {
                    direct.ms().saturating_sub(RELAY_GAIN_MS)
                };
                let via = core
                    .iter()
                    .filter(|c| *c != z)
                    .map(|c| {
                        let sum = links
                            .cost(*x, *c)
                            .ms()
                            .saturating_add(links.cost(*c, *z).ms());
                        (sum, channel_node_mix(channel, *c), *c)
                    })
                    .filter(|(sum, _, _)| *sum < budget)
                    .min();
                if let Some((_, _, c)) = via {
                    relays.insert((*x, *z), c);
                }
            }
            // A core hub that carries some of x's traffic already receives x's packet
            // directly, so x must not reach *it* through yet another relay (duplicate).
            let used: Vec<MediaNodeId> = hub_ids
                .iter()
                .filter_map(|z| relays.get(&(*x, *z)).copied())
                .collect();
            for c in used {
                relays.remove(&(*x, c));
            }
        }
        Self {
            hosting_by_region,
            hubs,
            star_regions,
            relays,
        }
    }

    /// Hub `from` sends a packet bound for hub `to` to this node (either `to` or its relay).
    pub fn next_hub(&self, from: MediaNodeId, to: MediaNodeId) -> MediaNodeId {
        self.relays.get(&(from, to)).copied().unwrap_or(to)
    }
}

/// Plans this node's [`ChannelRoute`] for `channel` in `region_tree` mode.
///
/// * `hosting` — nodes with live participants of the channel (may include `local.id`);
/// * `peers` — reachable healthy remote nodes (hosting or not; relay-only hubs are here too);
/// * `links` — fresh link measurements (empty ⇒ pre-measurement behaviour).
///
/// Hosting nodes without a reachable peer entry are ignored (same as in mesh mode). One hub per
/// region is elected ([`hub_rank`]). Inside a region hosts talk directly, unless two of them do
/// not reach each other — then the region is a star around its hub. Between regions the hubs
/// talk directly, unless a pair does not reach each other (or is much slower than a two-hop
/// path): then a *core* hub, one that reaches every hub, carries that direction. The local
/// node's route is:
/// * as a hosting node: `origin` = its in-region peers (or just the hub in a star region) plus
///   its region's hub — or, if it *is* the hub, the other regions' hubs / their relays;
/// * as its region's hub: forward in-region ingress to the other regions (and, in a star, to the
///   other in-region hosts), and remote-hub ingress to the in-region hosts plus the hubs it
///   relays for — never back to the ingress peer, and at most [`MAX_RELAY_HOPS`] hops:
///   host → hub → core hub → hub → host.
pub fn plan_region_tree(
    channel: &ChannelId,
    local: LocalNode,
    hosting: &HashSet<MediaNodeId>,
    peers: &HashMap<MediaNodeId, PeerNode>,
    links: &LinkMatrix,
) -> ChannelRoute {
    let plan = RegionTreePlan::compute(channel, local, hosting, peers, links);
    let local_hosts = hosting.contains(&local.id);
    let addr_of = |id: MediaNodeId| peers.get(&id).map(|p| p.addr);
    let local_hub = plan.hubs.get(&local.region).copied();
    let local_is_hub = local_hub == Some(local.id);
    let star = plan.star_regions.contains(&local.region);

    let mut in_region_hosts: Vec<SocketAddr> = plan
        .hosting_by_region
        .get(&local.region)
        .into_iter()
        .flatten()
        .filter(|id| **id != local.id)
        .filter_map(|id| addr_of(*id))
        .collect();
    in_region_hosts.sort_unstable();

    // Where this hub sends the traffic of its own region: each remote hub, or its relay.
    let mut cross_targets: Vec<SocketAddr> = Vec::new();
    // Remote hubs whose ingress this hub carries on to a third hub.
    let mut relayed_for: BTreeMap<SocketAddr, Vec<SocketAddr>> = BTreeMap::new();
    if local_is_hub {
        for (region, hub) in &plan.hubs {
            if *region == local.region {
                continue;
            }
            if let Some(a) = addr_of(plan.next_hub(local.id, *hub)) {
                cross_targets.push(a);
            }
            let Some(from) = addr_of(*hub) else { continue };
            for (other_region, z) in &plan.hubs {
                if other_region == region || *other_region == local.region {
                    continue;
                }
                if plan.relays.get(&(*hub, *z)) == Some(&local.id) {
                    if let Some(a) = addr_of(*z) {
                        relayed_for.entry(from).or_default().push(a);
                    }
                }
            }
        }
        cross_targets.sort_unstable();
        cross_targets.dedup();
    }

    let mut route = ChannelRoute::default();
    if local_hosts {
        if local_is_hub {
            route.origin.extend(in_region_hosts.iter().copied());
            route.origin.extend(cross_targets.iter().copied());
        } else {
            if !star {
                route.origin.extend(in_region_hosts.iter().copied());
            }
            // A relay-only hub only matters once traffic has to be forwarded somewhere.
            if star || plan.hubs.len() > 1 {
                if let Some(hub) = local_hub.and_then(addr_of) {
                    route.origin.push(hub);
                }
            }
        }
    }
    if local_is_hub {
        for src in &in_region_hosts {
            let mut targets = cross_targets.clone();
            if star {
                targets.extend(in_region_hosts.iter().copied().filter(|a| a != src));
            }
            route.forward.insert(*src, targets);
        }
        for (region, hub) in &plan.hubs {
            if *region == local.region {
                continue;
            }
            let Some(from) = addr_of(*hub) else { continue };
            let mut targets = in_region_hosts.clone();
            targets.extend(relayed_for.remove(&from).into_iter().flatten());
            route.forward.insert(from, targets);
        }
    }
    route.origin.sort_unstable();
    route.origin.dedup();
    for targets in route.forward.values_mut() {
        targets.sort_unstable();
        targets.dedup();
        targets.retain(|t| peers.values().any(|p| p.addr == *t));
    }
    route.forward.retain(|_, t| !t.is_empty());
    route
}

/// Cascade socket address of a registered node reachable from a relay of `family`, or `None`
/// if the node does not run a cascade relay, advertises no address of a usable family, or its
/// host name cannot be resolved. IPv4 is preferred on a dual-stack relay (`address` is the
/// primary endpoint every node has); `address_ipv6` is used when IPv4 cannot be.
pub async fn cascade_addr_of(node: &MediaNodeInfo, family: BoundFamily) -> Option<SocketAddr> {
    let port = node.cascade_port?;
    let usable = |a: SocketAddr| {
        if a.is_ipv4() {
            family.accepts_v4()
        } else {
            family.accepts_v6()
        }
    };
    let v6 = node
        .address_ipv6
        .as_deref()
        .and_then(|a| {
            a.trim_start_matches('[')
                .trim_end_matches(']')
                .parse::<IpAddr>()
                .ok()
        })
        .map(|ip| SocketAddr::new(ip, port))
        .filter(|a| usable(*a));
    if let Ok(ip) = node.address.parse::<IpAddr>() {
        let primary = canonical(SocketAddr::new(ip, port));
        return if usable(primary) { Some(primary) } else { v6 };
    }
    match tokio::net::lookup_host((node.address.as_str(), port)).await {
        Ok(addrs) => {
            let mut addrs: Vec<SocketAddr> = addrs.map(canonical).filter(|a| usable(*a)).collect();
            addrs.sort_by_key(|a| !a.is_ipv4());
            addrs.into_iter().next().or(v6)
        }
        Err(e) => {
            warn!(
                "Cannot resolve cascade address {} of node {}: {}",
                node.address, node.id, e
            );
            v6
        }
    }
}

impl CascadeTopology {
    pub fn new(
        pool: DbPool,
        nodes: Arc<NodeManager>,
        local: LocalNode,
        mode: CascadeTopologyMode,
        cascade: Arc<CascadeRelay>,
        sfu: Arc<RwLock<SfuNode>>,
        interval: Duration,
    ) -> Self {
        Self {
            pool,
            nodes,
            local,
            mode,
            cascade,
            sfu,
            interval: interval.max(Duration::from_millis(500)),
        }
    }

    /// Link rows older than this are stale: three missed publications plus the probe window.
    pub fn link_max_age(&self) -> Duration {
        (self.interval * 3 + self.cascade.link_max_age()).max(Duration::from_secs(10))
    }

    /// Publishes what this node's relay measured towards registry peers and loads the fresh
    /// fleet-wide link matrix (empty when the database is unavailable — pre-measurement rules).
    async fn sync_links(&self, peers: &HashMap<MediaNodeId, PeerNode>) -> LinkMatrix {
        let by_addr: HashMap<SocketAddr, MediaNodeId> =
            peers.iter().map(|(id, p)| (p.addr, *id)).collect();
        let reports = self.cascade.link_reports();
        let rows: Vec<(uuid::Uuid, &str, i32)> = reports
            .iter()
            .filter_map(|r| {
                let id = by_addr.get(&r.peer)?;
                Some((
                    id.0,
                    r.transport?.as_str(),
                    i32::try_from(r.rtt_ms?).unwrap_or(i32::MAX),
                ))
            })
            .collect();
        if let Err(e) =
            aurix_db::queries::replace_media_node_links(&self.pool, self.local.id.0, &rows).await
        {
            warn!("Cannot publish cascade link measurements: {}", e);
        }
        let max_age = self.link_max_age().as_secs() as i64;
        let mut matrix = LinkMatrix::new();
        match aurix_db::queries::fresh_media_node_links(&self.pool, max_age).await {
            Ok((links, reporters)) => {
                for id in reporters {
                    matrix.reporter(MediaNodeId::from_uuid(id));
                }
                for l in links {
                    matrix.report(
                        MediaNodeId::from_uuid(l.node_id),
                        MediaNodeId::from_uuid(l.peer_id),
                        u32::try_from(l.rtt_ms).unwrap_or(0),
                        l.transport == "tcp",
                    );
                }
            }
            Err(e) => warn!("Cannot load cascade link measurements: {}", e),
        }
        matrix
    }

    /// Healthy remote nodes with a reachable cascade endpoint, keyed by node id.
    async fn peer_nodes(&self) -> HashMap<MediaNodeId, PeerNode> {
        let mut out = HashMap::new();
        for node in self.nodes.get_all_nodes() {
            if node.id == self.local.id || !node.healthy {
                continue;
            }
            if let Some(addr) = cascade_addr_of(&node, self.cascade.family()).await {
                out.insert(
                    node.id,
                    PeerNode {
                        addr,
                        region: node.region,
                        relay_only: node.relay_only,
                    },
                );
            } else {
                debug!(
                    "Node {} ({} / {:?}) has no cascade address reachable from a {:?} relay",
                    node.id,
                    node.address,
                    node.address_ipv6,
                    self.cascade.family()
                );
            }
        }
        out
    }

    /// One reconciliation pass. Returns the number of channels this node relays (as origin
    /// or hub).
    pub async fn reconcile(&self) -> usize {
        self.nodes.refresh_from_db().await;
        let peers = self.peer_nodes().await;
        self.cascade
            .set_dynamic_peers(peers.values().map(|p| p.addr).collect::<HashSet<_>>());
        let links = self.sync_links(&peers).await;

        let local_channels: Vec<ChannelId> = self.sfu.read().channel_ids();
        let rows = match self.mode {
            CascadeTopologyMode::Mesh => {
                let ids: Vec<uuid::Uuid> = local_channels.iter().map(|c| c.0).collect();
                aurix_db::queries::remote_nodes_for_channels(&self.pool, &ids, self.local.id.0)
                    .await
            }
            CascadeTopologyMode::RegionTree => {
                aurix_db::queries::multi_node_channel_hosts(&self.pool).await
            }
        };
        let rows = match rows {
            Ok(rows) => rows,
            Err(e) => {
                warn!("Cascade topology query failed: {}", e);
                return 0;
            }
        };

        // channel → nodes hosting it (the local node counts if the SFU has the channel).
        let mut hosting: HashMap<ChannelId, HashSet<MediaNodeId>> = HashMap::new();
        for channel in &local_channels {
            hosting.entry(*channel).or_default().insert(self.local.id);
        }
        for (channel, node) in rows {
            let node = MediaNodeId::from_uuid(node);
            if node == self.local.id || peers.contains_key(&node) {
                hosting
                    .entry(ChannelId::from_uuid(channel))
                    .or_default()
                    .insert(node);
            }
        }
        let local = self.local;
        let static_peers = self.cascade.static_peers();
        let mut relayed = 0;
        let mut hub_channels = 0;
        let mut keep = HashSet::new();
        for (channel, hosts) in &hosting {
            let mut route = match self.mode {
                CascadeTopologyMode::Mesh => {
                    if !hosts.contains(&local.id) {
                        continue;
                    }
                    let mesh: Vec<SocketAddr> = hosts
                        .iter()
                        .filter(|id| **id != local.id)
                        .filter_map(|id| peers.get(id).map(|p| p.addr))
                        .collect();
                    ChannelRoute::mesh(&mesh)
                }
                CascadeTopologyMode::RegionTree => {
                    plan_region_tree(channel, local, hosts, &peers, &links)
                }
            };
            // Hand-configured peers (outside the registry) always receive the channels this
            // node hosts, exactly as before auto-discovery existed.
            if hosts.contains(&local.id) {
                for static_peer in &static_peers {
                    if !route.origin.contains(static_peer) {
                        route.origin.push(*static_peer);
                    }
                }
            }
            route.origin.sort_unstable();
            route.origin.dedup();
            if route.is_empty() {
                continue;
            }
            relayed += 1;
            if route.is_hub() {
                hub_channels += 1;
            }
            keep.insert(*channel);
            self.cascade.set_channel_route(*channel, route);
        }
        self.cascade.retain_channels(&keep);
        aurix_metrics::CASCADE_HUB_CHANNELS.set(hub_channels as i64);
        debug!(
            "Cascade topology ({:?}): {} peers, {} channels relayed, hub for {} (max {} hops)",
            self.mode,
            peers.len(),
            relayed,
            hub_channels,
            MAX_RELAY_HOPS
        );
        relayed
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
                | ServerEvent::SessionMigrated { .. }
                | ServerEvent::NodeHealthChanged { .. }
                | ServerEvent::LiveStreamStarted { .. }
                | ServerEvent::LiveStreamStopped { .. }
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
            address_ipv6: None,
            media_port: 9000,
            api_port: 8080,
            cascade_port,
            ws_url: None,
            api_url: None,
            location: None,
            active_channels: 0,
            active_participants: 0,
            cpu_usage: 0.0,
            memory_usage: 0.0,
            bandwidth_in_mbps: 0.0,
            bandwidth_out_mbps: 0.0,
            healthy: true,
            last_heartbeat: Utc::now(),
            version: "1.2.0".into(),
            registered_at: None,
            capacity: 100,
            relay_only: false,
            drain: None,
        }
    }

    #[tokio::test]
    async fn cascade_addr_uses_ip_or_resolves_hostname() {
        assert_eq!(
            cascade_addr_of(&node("10.1.2.3", Some(9001)), BoundFamily::V4).await,
            Some("10.1.2.3:9001".parse().unwrap())
        );
        assert_eq!(
            cascade_addr_of(&node("10.1.2.3", None), BoundFamily::V4).await,
            None
        );
        let resolved = cascade_addr_of(&node("localhost", Some(9001)), BoundFamily::DualStack)
            .await
            .unwrap();
        assert!(resolved.ip().is_loopback());
        assert_eq!(resolved.port(), 9001);
    }

    #[tokio::test]
    async fn cascade_addr_picks_the_family_the_relay_can_reach() {
        let mut dual = node("10.1.2.3", Some(9001));
        dual.address_ipv6 = Some("2001:db8::7".into());
        // Dual-stack and IPv4 relays prefer the primary IPv4 address.
        assert_eq!(
            cascade_addr_of(&dual, BoundFamily::DualStack).await,
            Some("10.1.2.3:9001".parse().unwrap())
        );
        assert_eq!(
            cascade_addr_of(&dual, BoundFamily::V4).await,
            Some("10.1.2.3:9001".parse().unwrap())
        );
        // An IPv6-only relay can only use the IPv6 address.
        assert_eq!(
            cascade_addr_of(&dual, BoundFamily::V6Only).await,
            Some("[2001:db8::7]:9001".parse().unwrap())
        );
        // A node that only advertises IPv4 is unreachable from an IPv6-only relay.
        assert_eq!(
            cascade_addr_of(&node("10.1.2.3", Some(9001)), BoundFamily::V6Only).await,
            None
        );
        // An IPv6-only node (primary address is IPv6) is unreachable from an IPv4 relay.
        let v6_only = node("2001:db8::9", Some(9001));
        assert_eq!(cascade_addr_of(&v6_only, BoundFamily::V4).await, None);
        assert_eq!(
            cascade_addr_of(&v6_only, BoundFamily::DualStack).await,
            Some("[2001:db8::9]:9001".parse().unwrap())
        );
    }

    // ── region-tree planner ──

    struct Fleet {
        nodes: Vec<(MediaNodeId, PeerNode)>,
        links: LinkMatrix,
    }

    impl Fleet {
        fn new(spec: &[(Region, bool)]) -> Self {
            let nodes = spec
                .iter()
                .enumerate()
                .map(|(i, (region, relay_only))| {
                    (
                        MediaNodeId::new(),
                        PeerNode {
                            addr: format!("10.0.0.{}:9001", i + 1).parse().unwrap(),
                            region: *region,
                            relay_only: *relay_only,
                        },
                    )
                })
                .collect();
            Self {
                nodes,
                links: LinkMatrix::new(),
            }
        }

        /// Both `i` and `j` measured each other at `rtt_ms` (over UDP unless `tcp`).
        fn link(&mut self, i: usize, j: usize, rtt_ms: u32, tcp: bool) {
            self.links.report(self.id(i), self.id(j), rtt_ms, tcp);
            self.links.report(self.id(j), self.id(i), rtt_ms, tcp);
        }

        /// Every node publishes a link table; pairs without a `link` are unconfirmed.
        fn all_report(&mut self) {
            for (id, _) in &self.nodes {
                self.links.reporter(*id);
            }
        }

        fn id(&self, i: usize) -> MediaNodeId {
            self.nodes[i].0
        }

        fn addr(&self, i: usize) -> SocketAddr {
            self.nodes[i].1.addr
        }

        /// Plans the route of node `i` given the reachable set `alive`.
        fn plan(
            &self,
            channel: &ChannelId,
            i: usize,
            hosting: &[usize],
            alive: &[usize],
        ) -> ChannelRoute {
            let local = LocalNode {
                id: self.id(i),
                region: self.nodes[i].1.region,
                relay_only: self.nodes[i].1.relay_only,
            };
            let peers: HashMap<MediaNodeId, PeerNode> = alive
                .iter()
                .filter(|j| **j != i)
                .map(|j| self.nodes[*j])
                .collect();
            let hosting: HashSet<MediaNodeId> = hosting.iter().map(|j| self.id(*j)).collect();
            plan_region_tree(channel, local, &hosting, &peers, &self.links)
        }

        /// Every alive node plans independently; then each hosting node originates one packet
        /// and the cascade forwarding rules are simulated. Returns, per origin, the list of
        /// nodes that delivered the packet locally (with hop counts) — and asserts nothing
        /// exceeds `MAX_RELAY_HOPS`.
        fn simulate(
            &self,
            channel: &ChannelId,
            hosting: &[usize],
            alive: &[usize],
        ) -> HashMap<usize, Vec<(usize, u8)>> {
            let routes: HashMap<SocketAddr, (usize, ChannelRoute)> = alive
                .iter()
                .map(|i| (self.addr(*i), (*i, self.plan(channel, *i, hosting, alive))))
                .collect();
            let mut out = HashMap::new();
            for origin in hosting {
                if !alive.contains(origin) {
                    continue;
                }
                let mut delivered = Vec::new();
                let mut queue: Vec<(SocketAddr, SocketAddr, u8)> = routes[&self.addr(*origin)]
                    .1
                    .origin
                    .iter()
                    .map(|to| (self.addr(*origin), *to, 1))
                    .collect();
                let mut steps = 0;
                while let Some((from, to, hops)) = queue.pop() {
                    steps += 1;
                    assert!(steps < 1000, "forwarding loop");
                    assert!(hops <= MAX_RELAY_HOPS, "hop cap exceeded");
                    let (idx, route) = &routes[&to];
                    delivered.push((*idx, hops));
                    if hops >= MAX_RELAY_HOPS {
                        continue;
                    }
                    if let Some(next) = route.forward.get(&from) {
                        for n in next {
                            assert_ne!(*n, from, "forwarded back to ingress");
                            queue.push((to, *n, hops + 1));
                        }
                    }
                }
                delivered.sort_unstable();
                out.insert(*origin, delivered);
            }
            out
        }
    }

    use Region::{AsiaPacific, EuWest, SouthAmerica, UsEast};

    /// Every other *hosting* node gets exactly one copy; relay-only nodes may see it too.
    fn assert_exactly_once(
        fleet: &Fleet,
        sim: &HashMap<usize, Vec<(usize, u8)>>,
        hosting: &[usize],
        alive: &[usize],
    ) {
        for origin in hosting.iter().filter(|o| alive.contains(o)) {
            let got = &sim[origin];
            for target in hosting.iter().filter(|t| alive.contains(t)) {
                let copies = got.iter().filter(|(i, _)| i == target).count();
                let expected = usize::from(target != origin);
                assert_eq!(
                    copies, expected,
                    "origin {origin} → node {target}: {copies} copies (route dump: {got:?})"
                );
            }
            for (i, _) in got {
                assert!(
                    hosting.contains(i) || fleet.nodes[*i].1.relay_only,
                    "non-hosting, non-relay node {i} received traffic"
                );
            }
        }
    }

    #[test]
    fn single_region_is_a_plain_mesh() {
        let fleet = Fleet::new(&[(EuWest, false), (EuWest, false), (EuWest, false)]);
        let ch = ChannelId::new();
        let route = fleet.plan(&ch, 0, &[0, 1, 2], &[0, 1, 2]);
        assert_eq!(route.origin, vec![fleet.addr(1), fleet.addr(2)]);
        assert!(!route.is_hub());
        // A node that does not host the channel has nothing to do.
        assert!(fleet.plan(&ch, 2, &[0, 1], &[0, 1, 2]).is_empty());
        let sim = fleet.simulate(&ch, &[0, 1, 2], &[0, 1, 2]);
        assert_exactly_once(&fleet, &sim, &[0, 1, 2], &[0, 1, 2]);
        assert!(sim[&0].iter().all(|(_, hops)| *hops == 1));
    }

    #[test]
    fn two_regions_without_relay_nodes_elect_hosting_hubs() {
        let fleet = Fleet::new(&[
            (EuWest, false),
            (EuWest, false),
            (UsEast, false),
            (UsEast, false),
        ]);
        let all = [0, 1, 2, 3];
        for _ in 0..8 {
            let ch = ChannelId::new();
            let sim = fleet.simulate(&ch, &all, &all);
            assert_exactly_once(&fleet, &sim, &all, &all);
            // In-region delivery is direct; cross-region traffic takes up to 3 hops
            // (hub → hub is 1 hop when both ends are the hubs themselves).
            let mut multi_hop = 0;
            for origin in all {
                for (target, hops) in &sim[&origin] {
                    let same_region = fleet.nodes[origin].1.region == fleet.nodes[*target].1.region;
                    if same_region {
                        assert_eq!(*hops, 1);
                    } else {
                        assert!((1..=3).contains(hops), "{hops}");
                        multi_hop += usize::from(*hops > 1);
                    }
                }
            }
            assert!(multi_hop >= 6, "non-hub hosts must be relayed via the hubs");
            // Exactly one node per region is a hub, and both hubs agree on each other.
            let hubs: Vec<usize> = all
                .iter()
                .copied()
                .filter(|i| fleet.plan(&ch, *i, &all, &all).is_hub())
                .collect();
            assert_eq!(hubs.len(), 2);
            assert_ne!(fleet.nodes[hubs[0]].1.region, fleet.nodes[hubs[1]].1.region);
            let hub_route = fleet.plan(&ch, hubs[0], &all, &all);
            assert!(hub_route.origin.contains(&fleet.addr(hubs[1])));
            assert!(hub_route.forward.contains_key(&fleet.addr(hubs[1])));
            // Non-hub hosting nodes only ever talk to their own region.
            for i in all.iter().filter(|i| !hubs.contains(i)) {
                let r = fleet.plan(&ch, *i, &all, &all);
                assert!(r.origin.iter().all(|a| {
                    let j = fleet.nodes.iter().position(|n| n.1.addr == *a).unwrap();
                    fleet.nodes[j].1.region == fleet.nodes[*i].1.region
                }));
            }
        }
    }

    #[test]
    fn relay_only_nodes_are_preferred_hubs_and_carry_channels_they_do_not_host() {
        // EU: 2 hosts + relay hub; US: 2 hosts + relay hub; APAC: 1 host, no relay node.
        let fleet = Fleet::new(&[
            (EuWest, false),
            (EuWest, false),
            (EuWest, true),
            (UsEast, false),
            (UsEast, false),
            (UsEast, true),
            (AsiaPacific, false),
        ]);
        let all = [0, 1, 2, 3, 4, 5, 6];
        let hosting = [0, 1, 3, 4, 6];
        let ch = ChannelId::new();
        let sim = fleet.simulate(&ch, &hosting, &all);
        assert_exactly_once(&fleet, &sim, &hosting, &all);

        let eu_hub = fleet.plan(&ch, 2, &hosting, &all);
        assert!(eu_hub.is_hub() && eu_hub.origin.is_empty());
        assert_eq!(
            eu_hub.forward[&fleet.addr(0)],
            vec![fleet.addr(5), fleet.addr(6)],
            "in-region ingress fans out to the other regions' hubs (US relay, APAC host)"
        );
        assert_eq!(
            eu_hub.forward[&fleet.addr(5)],
            vec![fleet.addr(0), fleet.addr(1)]
        );
        assert_eq!(
            eu_hub.forward[&fleet.addr(6)],
            vec![fleet.addr(0), fleet.addr(1)]
        );
        // EU hosts send in-region directly plus to their hub — never across regions.
        let host0 = fleet.plan(&ch, 0, &hosting, &all);
        assert_eq!(host0.origin, vec![fleet.addr(1), fleet.addr(2)]);
        assert!(!host0.is_hub());
        // The lone APAC host is its own hub: it originates straight to the remote hubs and
        // has no in-region forwarding.
        let apac = fleet.plan(&ch, 6, &hosting, &all);
        assert_eq!(apac.origin, vec![fleet.addr(2), fleet.addr(5)]);
        assert!(!apac.is_hub());
        // Every hosting node receives each cross-region packet in ≤ 3 hops.
        assert!(sim[&0].iter().any(|(i, hops)| *i == 3 && *hops == 3));
        assert!(sim[&0].iter().any(|(i, hops)| *i == 6 && *hops == 2));
        // A relay node in a region without participants of this channel idles.
        let idle = Fleet::new(&[(EuWest, false), (UsEast, false), (AsiaPacific, true)]);
        assert!(idle.plan(&ch, 2, &[0, 1], &[0, 1, 2]).is_empty());
    }

    #[test]
    fn hub_failure_reelects_and_unreachable_hosts_are_skipped() {
        let fleet = Fleet::new(&[
            (EuWest, false),
            (EuWest, true),
            (UsEast, false),
            (UsEast, false),
        ]);
        let hosting = [0, 2, 3];
        let ch = ChannelId::new();
        let all = [0, 1, 2, 3];
        assert!(fleet.plan(&ch, 1, &hosting, &all).is_hub());
        assert_exactly_once(&fleet, &fleet.simulate(&ch, &hosting, &all), &hosting, &all);

        // The EU relay node dies (dropped from the healthy registry): node 0 becomes EU hub.
        let alive = [0, 2, 3];
        let r0 = fleet.plan(&ch, 0, &hosting, &alive);
        assert!(
            !r0.is_hub(),
            "sole host of its region needs no in-region forwarding"
        );
        assert_eq!(r0.origin.len(), 1, "sends to the US hub only");
        assert_exactly_once(
            &fleet,
            &fleet.simulate(&ch, &hosting, &alive),
            &hosting,
            &alive,
        );

        // A US host that is unreachable (e.g. IPv6-only) is simply not part of the tree.
        let alive = [0, 1, 2];
        let sim = fleet.simulate(&ch, &hosting, &alive);
        assert_exactly_once(&fleet, &sim, &hosting, &alive);
        assert!(sim[&0].iter().all(|(i, _)| *i != 3));
    }

    #[test]
    fn hub_election_is_deterministic_and_spreads_across_channels() {
        let fleet = Fleet::new(&[
            (EuWest, false),
            (EuWest, false),
            (EuWest, false),
            (UsEast, false),
        ]);
        let all = [0, 1, 2, 3];
        let mut eu_hubs = HashSet::new();
        for _ in 0..64 {
            let ch = ChannelId::new();
            let hubs: Vec<usize> = [0, 1, 2]
                .into_iter()
                .filter(|i| fleet.plan(&ch, *i, &all, &all).is_hub())
                .collect();
            assert_eq!(hubs.len(), 1);
            eu_hubs.insert(hubs[0]);
            // Same inputs → same plan, from every node's point of view.
            assert_eq!(
                fleet.plan(&ch, hubs[0], &all, &all),
                fleet.plan(&ch, hubs[0], &all, &all)
            );
        }
        assert!(eu_hubs.len() > 1, "hub duty must not pin to one node");
    }

    // ── measured links ──

    fn hops_to(sim: &HashMap<usize, Vec<(usize, u8)>>, origin: usize, target: usize) -> u8 {
        sim[&origin]
            .iter()
            .find(|(i, _)| *i == target)
            .map(|(_, h)| *h)
            .unwrap_or_else(|| panic!("{origin} → {target} not delivered"))
    }

    #[test]
    fn measured_rtt_elects_the_hub_closest_to_the_other_regions() {
        let mut fleet = Fleet::new(&[
            (EuWest, false),
            (EuWest, false),
            (EuWest, false),
            (UsEast, false),
        ]);
        let all = [0, 1, 2, 3];
        fleet.all_report();
        fleet.link(0, 1, 8, false);
        fleet.link(0, 2, 9, false);
        fleet.link(1, 2, 7, false);
        fleet.link(1, 3, 40, false);
        fleet.link(0, 3, 120, false);
        fleet.link(2, 3, 130, false);
        for _ in 0..32 {
            let ch = ChannelId::new();
            let hubs: Vec<usize> = [0, 1, 2]
                .into_iter()
                .filter(|i| fleet.plan(&ch, *i, &all, &all).is_hub())
                .collect();
            assert_eq!(
                hubs,
                vec![1],
                "the EU node 40 ms from the US must be the hub"
            );
            assert_exactly_once(&fleet, &fleet.simulate(&ch, &all, &all), &all, &all);
        }
        // A TCP-only link is charged its penalty: node 1 falls behind node 0 (50 vs 40+100).
        fleet.link(1, 3, 40, true);
        fleet.link(0, 3, 50, false);
        let ch = ChannelId::new();
        assert!(fleet.plan(&ch, 0, &all, &all).is_hub());
        // Jitter inside one RTT bucket does not re-elect on its own: 49 vs 45 ms tie.
        fleet.link(1, 3, 45, false);
        fleet.link(0, 3, 49, false);
        let stable: HashSet<usize> = (0..32)
            .map(|_| {
                let ch = ChannelId::new();
                [0, 1, 2]
                    .into_iter()
                    .find(|i| fleet.plan(&ch, *i, &all, &all).is_hub())
                    .unwrap()
            })
            .collect();
        assert_eq!(
            stable.len(),
            2,
            "equal buckets fall back to the channel hash spread"
        );
        assert!(!stable.contains(&2));
    }

    #[test]
    fn candidates_that_cannot_reach_their_hosts_lose_even_when_relay_only() {
        // EU relay node 2 is cut off from host 1; host 0 reaches everyone and becomes hub.
        let mut fleet = Fleet::new(&[
            (EuWest, false),
            (EuWest, false),
            (EuWest, true),
            (UsEast, false),
        ]);
        let all = [0, 1, 2, 3];
        let hosting = [0, 1, 3];
        fleet.all_report();
        fleet.link(0, 1, 5, false);
        fleet.link(0, 2, 5, false);
        fleet.link(0, 3, 60, false);
        fleet.link(1, 3, 95, false);
        fleet.link(2, 3, 50, false);
        let ch = ChannelId::new();
        assert!(fleet.plan(&ch, 0, &hosting, &all).is_hub());
        assert!(fleet.plan(&ch, 2, &hosting, &all).is_empty());
        assert_exactly_once(&fleet, &fleet.simulate(&ch, &hosting, &all), &hosting, &all);
        // Once the relay node reaches host 1 too, it is preferred again.
        fleet.link(1, 2, 6, false);
        assert!(fleet.plan(&ch, 2, &hosting, &all).is_hub());
    }

    #[test]
    fn hosts_that_do_not_reach_each_other_use_the_hub_as_an_in_region_star() {
        let mut fleet = Fleet::new(&[
            (EuWest, false),
            (EuWest, false),
            (EuWest, false),
            (EuWest, true),
        ]);
        let all = [0, 1, 2, 3];
        let hosting = [0, 1, 2];
        fleet.all_report();
        fleet.link(0, 3, 4, false);
        fleet.link(1, 3, 4, false);
        fleet.link(2, 3, 4, false);
        fleet.link(0, 2, 3, false);
        fleet.link(1, 2, 3, false);
        // 0 and 1 never confirmed each other: everything goes through the relay hub.
        let ch = ChannelId::new();
        let r0 = fleet.plan(&ch, 0, &hosting, &all);
        assert_eq!(r0.origin, vec![fleet.addr(3)]);
        assert!(!r0.is_hub());
        let hub = fleet.plan(&ch, 3, &hosting, &all);
        assert!(hub.is_hub() && hub.origin.is_empty());
        assert_eq!(
            hub.forward[&fleet.addr(0)],
            vec![fleet.addr(1), fleet.addr(2)]
        );
        let sim = fleet.simulate(&ch, &hosting, &all);
        assert_exactly_once(&fleet, &sim, &hosting, &all);
        assert_eq!(hops_to(&sim, 0, 1), 2);
        // With a single region and working links the relay node is not involved at all.
        fleet.link(0, 1, 3, false);
        let r0 = fleet.plan(&ch, 0, &hosting, &all);
        assert_eq!(r0.origin, vec![fleet.addr(1), fleet.addr(2)]);
        assert!(fleet.plan(&ch, 3, &hosting, &all).is_empty());
        // A node that does not publish measurements is assumed reachable (legacy nodes).
        let mut legacy = Fleet::new(&[(EuWest, false), (EuWest, false), (EuWest, true)]);
        legacy.links.reporter(legacy.id(0));
        legacy.links.reporter(legacy.id(2));
        assert_eq!(
            legacy.plan(&ch, 0, &[0, 1], &[0, 1, 2]).origin,
            vec![legacy.addr(1)]
        );
    }

    #[test]
    fn hubs_that_do_not_reach_each_other_relay_through_a_core_hub() {
        // Three regions, each a host behind a relay-only hub; the EU and APAC hubs are cut off
        // from each other, the US hub reaches both.
        let mut fleet = Fleet::new(&[
            (EuWest, false),
            (EuWest, true),
            (UsEast, false),
            (UsEast, true),
            (AsiaPacific, false),
            (AsiaPacific, true),
        ]);
        let all = [0, 1, 2, 3, 4, 5];
        let hosting = [0, 2, 4];
        fleet.all_report();
        fleet.link(0, 1, 5, false);
        fleet.link(2, 3, 5, false);
        fleet.link(4, 5, 5, false);
        fleet.link(1, 3, 80, false);
        fleet.link(3, 5, 100, false);
        let ch = ChannelId::new();
        let eu = fleet.plan(&ch, 1, &hosting, &all);
        assert!(eu.is_hub());
        assert_eq!(
            eu.forward[&fleet.addr(0)],
            vec![fleet.addr(3)],
            "EU ingress goes to the US core hub only"
        );
        let us = fleet.plan(&ch, 3, &hosting, &all);
        assert_eq!(
            us.forward[&fleet.addr(1)],
            vec![fleet.addr(2), fleet.addr(5)],
            "the core hub delivers locally and carries EU traffic on to APAC"
        );
        assert_eq!(
            us.forward[&fleet.addr(5)],
            vec![fleet.addr(1), fleet.addr(2)]
        );
        assert_eq!(
            us.forward[&fleet.addr(2)],
            vec![fleet.addr(1), fleet.addr(5)]
        );
        let apac = fleet.plan(&ch, 5, &hosting, &all);
        assert_eq!(
            apac.forward[&fleet.addr(3)],
            vec![fleet.addr(4)],
            "relayed traffic is not forwarded any further"
        );
        let sim = fleet.simulate(&ch, &hosting, &all);
        assert_exactly_once(&fleet, &sim, &hosting, &all);
        assert_eq!(hops_to(&sim, 0, 4), 4);
        assert_eq!(hops_to(&sim, 4, 0), 4);
        assert_eq!(hops_to(&sim, 0, 2), 3);
        assert!(sim.values().flatten().all(|(_, h)| *h <= MAX_RELAY_HOPS));

        // A fourth region the EU hub cannot reach keeps EU a non-core hub; its direct APAC
        // link now exists but is much slower than via the US core hub (300 > 80 + 100 + margin),
        // so it is bypassed …
        fleet.nodes.push((
            MediaNodeId::new(),
            PeerNode {
                addr: "10.0.0.7:9001".parse().unwrap(),
                region: SouthAmerica,
                relay_only: false,
            },
        ));
        fleet.links.reporter(fleet.id(6));
        fleet.link(6, 3, 120, false);
        fleet.link(6, 5, 100, false);
        let all = [0, 1, 2, 3, 4, 5, 6];
        let hosting = [0, 2, 4, 6];
        fleet.link(1, 5, 300, false);
        let sim = fleet.simulate(&ch, &hosting, &all);
        assert_exactly_once(&fleet, &sim, &hosting, &all);
        assert_eq!(hops_to(&sim, 0, 4), 4);
        assert_eq!(hops_to(&sim, 0, 6), 3, "EU → US core → SA host");
        // … while a direct link that is nearly as fast is used as before.
        fleet.link(1, 5, 170, false);
        let sim = fleet.simulate(&ch, &hosting, &all);
        assert_exactly_once(&fleet, &sim, &hosting, &all);
        assert_eq!(hops_to(&sim, 0, 4), 3);
        assert!(fleet.plan(&ch, 3, &hosting, &all).forward[&fleet.addr(1)]
            .iter()
            .all(|a| *a != fleet.addr(5)));
        // Core hubs (reaching every hub) always send directly, so relayed traffic is never
        // forwarded twice: the US hub reaches EU, APAC and SA itself.
        let us = fleet.plan(&ch, 3, &hosting, &all);
        assert_eq!(
            us.forward[&fleet.addr(2)],
            vec![fleet.addr(1), fleet.addr(5), fleet.addr(6)]
        );
    }

    /// Random fleets (regions, relay nodes, hosting sets, partial/unconfirmed/TCP links):
    /// every plan delivers exactly once within the hop cap and never back to the ingress.
    #[test]
    fn random_fleets_and_link_tables_always_deliver_exactly_once() {
        let mut seed: u64 = 0x5EED_CA5C_ADE0_0001;
        let mut next = move |n: u64| {
            seed = seed
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            (seed >> 33) % n
        };
        let regions = [EuWest, UsEast, AsiaPacific, SouthAmerica];
        for _ in 0..400 {
            let n = 2 + next(7) as usize;
            let spec: Vec<(Region, bool)> = (0..n)
                .map(|_| (regions[next(4) as usize], next(4) == 0))
                .collect();
            let mut fleet = Fleet::new(&spec);
            let all: Vec<usize> = (0..n).collect();
            let hosting: Vec<usize> = all
                .iter()
                .copied()
                .filter(|i| !spec[*i].1 && next(4) != 0)
                .collect();
            for i in 0..n {
                if next(5) != 0 {
                    fleet.links.reporter(fleet.id(i));
                }
            }
            for i in 0..n {
                for j in i + 1..n {
                    match next(5) {
                        0 => {}
                        1 => fleet.link(i, j, 5 + next(300) as u32, true),
                        _ => fleet.link(i, j, 5 + next(300) as u32, false),
                    }
                }
            }
            let ch = ChannelId::new();
            let sim = fleet.simulate(&ch, &hosting, &all);
            assert_exactly_once(&fleet, &sim, &hosting, &all);
            for i in &all {
                let route = fleet.plan(&ch, *i, &hosting, &all);
                assert!(!route.origin.contains(&fleet.addr(*i)));
                for (src, targets) in &route.forward {
                    assert!(!targets.contains(src));
                    assert!(!targets.contains(&fleet.addr(*i)));
                }
            }
        }
    }

    #[test]
    fn without_a_core_hub_cut_off_hubs_still_try_the_direct_link() {
        let mut fleet = Fleet::new(&[(EuWest, false), (UsEast, false), (AsiaPacific, false)]);
        let all = [0, 1, 2];
        fleet.all_report();
        fleet.link(0, 1, 80, false);
        // 1–2 and 0–2 unconfirmed: nobody reaches APAC, so APAC keeps the legacy direct rule.
        let ch = ChannelId::new();
        let sim = fleet.simulate(&ch, &all, &all);
        assert_exactly_once(&fleet, &sim, &all, &all);
        assert!(sim.values().flatten().all(|(_, h)| *h == 1));
        // Same snapshot on every node → identical plans.
        for i in all {
            assert_eq!(
                fleet.plan(&ch, i, &all, &all),
                fleet.plan(&ch, i, &all, &all)
            );
        }
    }
}
