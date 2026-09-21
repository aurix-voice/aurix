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
//!     between regions goes through exactly one *hub* per region
//!     (origin → own hub → remote hub → hosting node, at most [`MAX_RELAY_HOPS`] hops).
//!     Hubs are chosen deterministically per channel (`relay_only` nodes first, then hosting
//!     nodes, tie-broken by a channel/node hash so hub duty spreads across the region), so
//!     every node computes the same tree from the same registry snapshot, and only hubs need
//!     inter-regional cascade reachability.
//!
//! Reconciliation runs periodically and immediately after remote `ParticipantJoined` /
//! `ParticipantLeft` / `SessionMigrated` / `NodeHealthChanged` events, so a channel spanning
//! two nodes starts relaying within one event round-trip rather than a full interval.

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

/// Deterministic per-channel ranking of hub candidates: relay-only nodes first, then a
/// channel/node hash so different channels pick different hubs within a region.
fn hub_rank(channel: &ChannelId, node: MediaNodeId, relay_only: bool) -> (bool, u128, MediaNodeId) {
    let mix = (channel.0.as_u128() ^ node.0.as_u128())
        .wrapping_mul(0x9E37_79B9_7F4A_7C15_F39C_C060_5CED_C835);
    (!relay_only, mix, node)
}

/// Plans this node's [`ChannelRoute`] for `channel` in `region_tree` mode.
///
/// * `hosting` — nodes with live participants of the channel (may include `local.id`);
/// * `peers` — reachable healthy remote nodes (hosting or not; relay-only hubs are here too).
///
/// Hosting nodes without a reachable peer entry are ignored (same as in mesh mode). With a
/// single region the result is a plain mesh among the hosting nodes. Otherwise one hub per
/// region is elected; the local node's route is:
/// * as a hosting node: `origin` = other hosting nodes of its region + its region's hub
///   (or, if it *is* the hub, the other regions' hubs);
/// * as its region's hub: forward in-region ingress to the other regions' hubs, and remote
///   hubs' ingress to the in-region hosting nodes (never back to the ingress peer).
pub fn plan_region_tree(
    channel: &ChannelId,
    local: LocalNode,
    hosting: &HashSet<MediaNodeId>,
    peers: &HashMap<MediaNodeId, PeerNode>,
) -> ChannelRoute {
    let local_hosts = hosting.contains(&local.id);
    // region → hosting nodes (remote ones must be reachable).
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
    let addr_of = |id: MediaNodeId| peers.get(&id).map(|p| p.addr);

    if hosting_by_region.len() <= 1 {
        if !local_hosts {
            return ChannelRoute::default();
        }
        let mesh: Vec<SocketAddr> = hosting_by_region
            .values()
            .flatten()
            .filter(|id| **id != local.id)
            .filter_map(|id| addr_of(*id))
            .collect();
        return ChannelRoute::mesh(&mesh);
    }

    // One hub per region: relay-only nodes of the region first, then its hosting nodes.
    let mut hubs: BTreeMap<Region, MediaNodeId> = BTreeMap::new();
    for (region, hosts) in &hosting_by_region {
        let mut best: Option<((bool, u128, MediaNodeId), MediaNodeId)> = None;
        let mut consider = |id: MediaNodeId, relay_only: bool| {
            let rank = hub_rank(channel, id, relay_only);
            if best.as_ref().is_none_or(|(r, _)| rank < *r) {
                best = Some((rank, id));
            }
        };
        for (id, p) in peers {
            if p.relay_only && p.region == *region {
                consider(*id, true);
            }
        }
        if local.relay_only && local.region == *region {
            consider(local.id, true);
        }
        for id in hosts {
            consider(*id, false);
        }
        if let Some((_, id)) = best {
            hubs.insert(*region, id);
        }
    }
    let local_is_hub = hubs.get(&local.region) == Some(&local.id);
    let mut remote_hubs: Vec<SocketAddr> = hubs
        .iter()
        .filter(|(region, _)| **region != local.region)
        .filter_map(|(_, id)| addr_of(*id))
        .collect();
    remote_hubs.sort_unstable();
    let mut in_region_hosts: Vec<SocketAddr> = hosting_by_region
        .get(&local.region)
        .into_iter()
        .flatten()
        .filter(|id| **id != local.id)
        .filter_map(|id| addr_of(*id))
        .collect();
    in_region_hosts.sort_unstable();

    let mut route = ChannelRoute::default();
    if local_hosts {
        route.origin.extend(in_region_hosts.iter().copied());
        if local_is_hub {
            route.origin.extend(remote_hubs.iter().copied());
        } else if let Some(hub) = hubs.get(&local.region).and_then(|id| addr_of(*id)) {
            if !route.origin.contains(&hub) {
                route.origin.push(hub);
            }
        }
    }
    if local_is_hub {
        for src in &in_region_hosts {
            route.forward.insert(*src, remote_hubs.clone());
        }
        for src in &remote_hubs {
            route.forward.insert(*src, in_region_hosts.clone());
        }
    }
    route.origin.sort_unstable();
    route.origin.dedup();
    for targets in route.forward.values_mut() {
        targets.sort_unstable();
    }
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
    ) -> Self {
        Self {
            pool,
            nodes,
            local,
            mode,
            cascade,
            sfu,
        }
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
                CascadeTopologyMode::RegionTree => plan_region_tree(channel, local, hosts, &peers),
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
            Self { nodes }
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
            plan_region_tree(channel, local, &hosting, &peers)
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

    use Region::{AsiaPacific, EuWest, UsEast};

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
}
