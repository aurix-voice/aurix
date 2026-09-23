//! Node-to-node audio relay ("cascade") for channels spanning several SFU nodes.
//!
//! Every relayed datagram is a `Relay` envelope (see `AurixPacket::relay_envelope`) carrying the
//! original client packet, sealed (AES-256-CTR + HMAC) with keys derived from the cluster-wide
//! `media.cascade_secret`. The envelope's SSRC is a per-process random node id and its 64-bit
//! counter is spread over the sequence/timestamp fields, so IVs never repeat between nodes and
//! the per-peer anti-replay window sees one monotonic sequence. The counter starts at the
//! process start time (Unix seconds `<< 32`), so a restarted node continues *above* everything
//! its peers already accepted from the same address instead of being rejected as a replay
//! until it overtakes the previous process. Packets are accepted only from
//! allowed peers (statically configured via `media.cascade_peers` and/or discovered from the
//! `media_nodes` registry), only with a valid tag and only once per (peer, sequence).
//!
//! Topology is per channel (see [`ChannelRoute`], driven by `CascadeTopology`): a locally
//! received client packet goes to the channel's `origin` peers, and a node that is a relay-tree
//! *hub* re-forwards envelopes arriving from one peer to the peers listed for it in `forward`
//! (never back to the peer it came from). Re-forwarded envelopes carry a hop byte
//! (`PacketFlags::RelayHop`) and nothing travels more than `MAX_RELAY_HOPS` node-to-node hops,
//! so a stale or inconsistent plan can at worst lose a packet, never loop one. One-hop mesh
//! envelopes (no hop byte) are what every node has always sent, so a relay-tree hub can be
//! introduced next to nodes that predate relay trees.
//!
//! Peers are probed continuously (see [`crate::cascade_link`]): the relay knows each peer's
//! round-trip time and whether UDP reaches it, and falls back to a TCP link on the same port
//! carrying the same sealed envelopes when UDP does not.

use crate::cascade_link::{self, CascadeControl, LinkReport, LinkState, LinkTransport};
use crate::transport::{bind_media_socket, MediaSocket};
use aurix_common::crypto::MediaKeys;
use aurix_common::error::{AurixError, Result};
use aurix_common::protocol::{
    channel_id_hash, AurixPacket, PacketFlags, PacketType, WideReplayWindow, HEADER_SIZE,
    MAX_RELAY_HOPS, MAX_RELAY_PACKET_SIZE,
};
use aurix_common::types::*;
use bytes::Bytes;
use dashmap::DashMap;
use parking_lot::Mutex;
use std::collections::{BTreeMap, HashSet};
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::io::AsyncWriteExt;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

/// Link probing / fallback knobs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CascadeOptions {
    /// Listen for and open TCP fallback links on the cascade port.
    pub tcp_fallback: bool,
    /// How often every allowed peer is pinged.
    pub probe_interval: Duration,
}

impl Default for CascadeOptions {
    fn default() -> Self {
        Self {
            tcp_fallback: true,
            probe_interval: Duration::from_secs(1),
        }
    }
}

/// UDP pings without a pong before the TCP fallback is tried.
const UDP_PROBES_BEFORE_TCP: u32 = 3;
/// Spacing between TCP connect attempts to one peer: five probe intervals, at least this.
const TCP_CONNECT_BACKOFF_MIN: Duration = Duration::from_millis(200);
const TCP_CONNECT_BACKOFF_MAX: Duration = Duration::from_secs(5);
const TCP_CONNECT_TIMEOUT: Duration = Duration::from_secs(3);
/// A TCP link that carried nothing for this long while UDP is back is closed.
const TCP_IDLE_CLOSE: Duration = Duration::from_secs(30);
/// An inbound TCP connection must present its `Hello` within this time.
const TCP_HELLO_TIMEOUT: Duration = Duration::from_secs(5);
/// An inbound TCP connection without any frame for this long is closed.
const TCP_INBOUND_IDLE: Duration = Duration::from_secs(60);
/// Bounded per-peer send queue of a TCP link (envelopes); overflow drops, never buffers.
const TCP_QUEUE_FRAMES: usize = 256;
const MAX_INBOUND_TCP: usize = 1024;
/// A pong whose nonce is older than this is ignored (clock/nonce confusion, not a sample).
const MAX_PLAUSIBLE_RTT: Duration = Duration::from_secs(30);

/// Forwarding plan of one channel on this node.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ChannelRoute {
    /// Peers that receive packets originating from this node's own participants.
    pub origin: Vec<SocketAddr>,
    /// Hub rules: an envelope arriving from the key peer is re-forwarded to the listed peers.
    pub forward: BTreeMap<SocketAddr, Vec<SocketAddr>>,
}

impl ChannelRoute {
    /// A plain one-hop mesh route: send to `peers`, never re-forward.
    pub fn mesh(peers: &[SocketAddr]) -> Self {
        Self {
            origin: peers.to_vec(),
            forward: BTreeMap::new(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.origin.is_empty() && self.forward.values().all(|v| v.is_empty())
    }

    pub fn is_hub(&self) -> bool {
        self.forward.values().any(|v| !v.is_empty())
    }
}

/// An authenticated inbound relay envelope.
#[derive(Debug, Clone)]
pub struct InboundRelay {
    pub sender: UserId,
    /// Node-to-node hops the client packet has travelled so far (1 for a mesh envelope).
    pub hops: u8,
    pub packet: AurixPacket,
}

/// An authenticated inbound envelope: relayed audio or a node-to-node control message.
#[derive(Debug, Clone)]
pub enum Inbound {
    Audio(InboundRelay),
    Control(CascadeControl),
}

pub struct CascadeRelay {
    /// channel_id -> where this node sends and re-forwards that channel's audio.
    routes: Arc<DashMap<ChannelId, ChannelRoute>>,
    /// `channel_id_hash` -> channel for the routes above (envelopes only carry the hash).
    routes_by_hash: Arc<DashMap<u32, ChannelId>>,
    /// Peers we accept relayed traffic from, each with its own anti-replay window.
    allowed_peers: Arc<DashMap<SocketAddr, Mutex<WideReplayWindow>>>,
    /// Peers from static configuration; never removed by discovery.
    static_peers: HashSet<SocketAddr>,
    /// Peers currently known through discovery (healthy nodes from the registry).
    dynamic_peers: Mutex<HashSet<SocketAddr>>,
    socket: Arc<MediaSocket>,
    local_addr: SocketAddr,
    keys: MediaKeys,
    relay_ssrc: u32,
    relay_counter: AtomicU64,
    local_node_id: MediaNodeId,
    options: CascadeOptions,
    /// Per allowed peer: probe results and TCP link bookkeeping.
    links: DashMap<SocketAddr, LinkState>,
    /// Outbound TCP links (send queues), one per peer that needed the fallback.
    tcp_links: DashMap<SocketAddr, mpsc::Sender<Bytes>>,
    tcp_listener: Mutex<Option<TcpListener>>,
    tcp_enabled: bool,
    inbound_tcp: AtomicUsize,
    /// This node's cascade addresses as peers know them (registry `address` + cascade port),
    /// announced in the TCP `Hello`.
    advertised: Mutex<Vec<SocketAddr>>,
    started: Instant,
    shutdown: CancellationToken,
}

impl CascadeRelay {
    pub async fn new(
        bind_addr: SocketAddr,
        local_node_id: MediaNodeId,
        secret: &str,
        peers: &[String],
        options: CascadeOptions,
    ) -> Result<Self> {
        let socket = bind_media_socket(bind_addr)
            .map_err(|e| AurixError::Transport(format!("Cascade bind failed: {e}")))?;
        let local_addr = socket.local_addr();
        let tcp_listener = if options.tcp_fallback {
            match aurix_common::net::bind_tcp(local_addr, true) {
                Ok((listener, _)) => Some(listener),
                Err(e) => {
                    warn!(
                        "Cascade TCP fallback disabled: cannot listen on {}/tcp: {}",
                        local_addr, e
                    );
                    None
                }
            }
        } else {
            None
        };
        Self::with_sockets(socket, tcp_listener, local_node_id, secret, peers, options)
    }

    /// Build a relay over sockets the caller bound: `socket` carries UDP envelopes and
    /// `tcp_listener` (when TCP fallback is wanted) accepts framed envelopes from peers.
    /// Peers address both transports with the *same* `host:port`, so in production the
    /// listener shares the UDP socket's port; tests may split them to simulate blocked UDP.
    pub fn with_sockets(
        socket: Arc<MediaSocket>,
        tcp_listener: Option<TcpListener>,
        local_node_id: MediaNodeId,
        secret: &str,
        peers: &[String],
        options: CascadeOptions,
    ) -> Result<Self> {
        if secret.len() < 16 {
            return Err(AurixError::InvalidConfiguration(
                "media.cascade_secret must be at least 16 characters".into(),
            ));
        }
        let local_addr = socket.local_addr();
        let allowed_peers: Arc<DashMap<SocketAddr, Mutex<WideReplayWindow>>> =
            Arc::new(DashMap::new());
        let mut static_peers = HashSet::new();
        for p in peers {
            let addr: SocketAddr = p.parse().map(aurix_common::addr::canonical).map_err(|_| {
                AurixError::InvalidConfiguration(format!("Invalid cascade peer address: {p}"))
            })?;
            if !socket.can_reach(addr) {
                return Err(AurixError::InvalidConfiguration(format!(
                    "cascade peer {p} is not reachable from the cascade socket bound to {local_addr}"
                )));
            }
            allowed_peers.insert(addr, Mutex::new(WideReplayWindow::default()));
            static_peers.insert(addr);
        }
        let tcp_listener = if options.tcp_fallback {
            tcp_listener
        } else {
            None
        };
        info!(
            "Cascade relay listening on {} (udp{}) with {} static peers",
            local_addr,
            if tcp_listener.is_some() { "+tcp" } else { "" },
            allowed_peers.len()
        );
        let links = DashMap::new();
        for peer in &static_peers {
            links.insert(*peer, LinkState::default());
        }
        Ok(Self {
            routes: Arc::new(DashMap::new()),
            routes_by_hash: Arc::new(DashMap::new()),
            allowed_peers,
            static_peers,
            dynamic_peers: Mutex::new(HashSet::new()),
            socket,
            local_addr,
            keys: MediaKeys::derive(secret.as_bytes()),
            relay_ssrc: rand::random(),
            relay_counter: AtomicU64::new(Self::initial_relay_counter()),
            local_node_id,
            options,
            links,
            tcp_links: DashMap::new(),
            tcp_enabled: tcp_listener.is_some(),
            tcp_listener: Mutex::new(tcp_listener),
            inbound_tcp: AtomicUsize::new(0),
            advertised: Mutex::new(Vec::new()),
            started: Instant::now(),
            shutdown: CancellationToken::new(),
        })
    }

    /// First envelope counter of this process: the start time in the high 32 bits. A peer's
    /// replay window for our address survives our restart, and a process never sends
    /// `2^32` envelopes per second of uptime, so this is always above the last counter the
    /// previous process could have used (assuming the host clock did not go backwards).
    fn initial_relay_counter() -> u64 {
        let secs = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        (secs & 0xFFFF_FFFF) << 32
    }

    pub fn node_id(&self) -> MediaNodeId {
        self.local_node_id
    }

    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    /// Whether TCP fallback links are in use on this node (listener bound at start-up).
    pub fn tcp_fallback(&self) -> bool {
        self.tcp_enabled
    }

    /// Tell the relay how peers address this node (registry address + cascade port, one per
    /// address family); used in the TCP `Hello`. Defaults to the bind address.
    pub fn set_advertised(&self, addrs: Vec<SocketAddr>) {
        *self.advertised.lock() = addrs;
    }

    fn hello_addr_for(&self, peer: SocketAddr) -> SocketAddr {
        let advertised = self.advertised.lock();
        advertised
            .iter()
            .copied()
            .find(|a| a.is_ipv4() == peer.is_ipv4())
            .or_else(|| advertised.first().copied())
            .unwrap_or(self.local_addr)
    }

    /// How long a probe answer stays valid before the peer counts as unconfirmed.
    pub fn link_max_age(&self) -> Duration {
        (self.options.probe_interval * (UDP_PROBES_BEFORE_TCP + 1)).max(Duration::from_secs(3))
    }

    fn tcp_backoff(&self) -> Duration {
        (self.options.probe_interval * 5).clamp(TCP_CONNECT_BACKOFF_MIN, TCP_CONNECT_BACKOFF_MAX)
    }

    /// Stop the receiver, prober, TCP listener and every TCP link (idempotent). UDP
    /// envelopes are no longer read afterwards.
    pub fn shutdown(&self) {
        self.shutdown.cancel();
        self.tcp_links.clear();
    }

    /// Which address families this relay can exchange datagrams with.
    pub fn family(&self) -> aurix_common::net::BoundFamily {
        self.socket.family()
    }

    pub fn allowed_peers(&self) -> Vec<SocketAddr> {
        self.allowed_peers.iter().map(|e| *e.key()).collect()
    }

    pub fn static_peers(&self) -> Vec<SocketAddr> {
        self.static_peers.iter().copied().collect()
    }

    /// Replace the set of discovered peers. Newly seen addresses become accepted sources
    /// (fresh replay window); addresses that disappeared and are not statically configured
    /// are dropped from the allow-list and from every channel's route.
    pub fn set_dynamic_peers(&self, peers: HashSet<SocketAddr>) {
        let mut current = self.dynamic_peers.lock();
        for added in peers.difference(&current) {
            self.allowed_peers
                .entry(*added)
                .or_insert_with(|| Mutex::new(WideReplayWindow::default()));
            self.links.entry(*added).or_default();
            info!("Cascade peer discovered: {}", added);
        }
        for removed in current.difference(&peers) {
            if self.static_peers.contains(removed) {
                continue;
            }
            self.allowed_peers.remove(removed);
            self.links.remove(removed);
            self.tcp_links.remove(removed);
            for mut entry in self.routes.iter_mut() {
                let route = entry.value_mut();
                route.origin.retain(|a| a != removed);
                route.forward.remove(removed);
                for targets in route.forward.values_mut() {
                    targets.retain(|a| a != removed);
                }
            }
            info!("Cascade peer gone: {}", removed);
        }
        *current = peers;
        self.refresh_hub_gauge();
    }

    /// Register a remote node as having participants in a given channel. Unknown peers are refused.
    pub fn add_peer(&self, channel_id: ChannelId, peer_addr: SocketAddr) -> Result<()> {
        if !self.allowed_peers.contains_key(&peer_addr) {
            return Err(AurixError::AuthorizationDenied(format!(
                "{peer_addr} is not an allowed cascade peer"
            )));
        }
        let mut entry = self.routes.entry(channel_id).or_default();
        if !entry.origin.contains(&peer_addr) {
            entry.origin.push(peer_addr);
        }
        drop(entry);
        self.routes_by_hash
            .insert(channel_id_hash(&channel_id), channel_id);
        Ok(())
    }

    /// Replace the route of a channel with a one-hop mesh to exactly the allowed peers in
    /// `peers`; an empty list removes the channel from the relay entirely.
    pub fn set_channel_peers(&self, channel_id: ChannelId, peers: &[SocketAddr]) {
        self.set_channel_route(channel_id, ChannelRoute::mesh(peers));
    }

    /// Replace the route of a channel. Unknown peers are dropped from every list, a forward
    /// rule never targets the peer it is keyed by, and an empty result removes the channel.
    pub fn set_channel_route(&self, channel_id: ChannelId, route: ChannelRoute) {
        let allowed = |p: &SocketAddr| self.allowed_peers.contains_key(p);
        let mut origin: Vec<SocketAddr> = route.origin.into_iter().filter(allowed).collect();
        origin.sort_unstable();
        origin.dedup();
        let mut forward = BTreeMap::new();
        for (from, targets) in route.forward {
            if !allowed(&from) {
                continue;
            }
            let mut targets: Vec<SocketAddr> = targets
                .into_iter()
                .filter(|t| *t != from && allowed(t))
                .collect();
            targets.sort_unstable();
            targets.dedup();
            if !targets.is_empty() {
                forward.insert(from, targets);
            }
        }
        let route = ChannelRoute { origin, forward };
        let hash = channel_id_hash(&channel_id);
        if route.is_empty() {
            self.routes.remove(&channel_id);
            self.routes_by_hash
                .remove_if(&hash, |_, c| *c == channel_id);
        } else {
            self.routes.insert(channel_id, route);
            self.routes_by_hash.insert(hash, channel_id);
        }
        self.refresh_hub_gauge();
    }

    /// Drop every route whose channel is not in `keep` (channels that vanished from the plan,
    /// e.g. hub duty for a channel this node never hosted).
    pub fn retain_channels(&self, keep: &HashSet<ChannelId>) {
        let stale: Vec<ChannelId> = self
            .routes
            .iter()
            .map(|e| *e.key())
            .filter(|c| !keep.contains(c))
            .collect();
        for c in stale {
            self.remove_channel(&c);
        }
    }

    fn refresh_hub_gauge(&self) {
        let hubs = self.routes.iter().filter(|e| e.value().is_hub()).count();
        aurix_metrics::CASCADE_HUB_CHANNELS.set(hubs as i64);
    }

    /// Subscribe every statically configured peer to `channel_id` (legacy full-mesh mode,
    /// used when no `media_nodes` registry is available).
    pub fn add_all_peers(&self, channel_id: ChannelId) {
        for peer in self.static_peers() {
            let _ = self.add_peer(channel_id, peer);
        }
    }

    pub fn remove_peer(&self, channel_id: ChannelId, peer_addr: &SocketAddr) {
        if let Some(mut entry) = self.routes.get_mut(&channel_id) {
            entry.origin.retain(|a| a != peer_addr);
        }
    }

    pub fn remove_channel(&self, channel_id: &ChannelId) {
        self.routes.remove(channel_id);
        self.routes_by_hash
            .remove_if(&channel_id_hash(channel_id), |_, c| c == channel_id);
        self.refresh_hub_gauge();
    }

    /// Peers that receive this node's own participants' audio for the channel.
    pub fn channel_peers(&self, channel_id: &ChannelId) -> Vec<SocketAddr> {
        self.routes
            .get(channel_id)
            .map(|r| r.origin.clone())
            .unwrap_or_default()
    }

    pub fn channel_route(&self, channel_id: &ChannelId) -> Option<ChannelRoute> {
        self.routes.get(channel_id).map(|r| r.value().clone())
    }

    /// Channels this node currently has any route for (hosted or hub duty).
    pub fn routed_channels(&self) -> Vec<ChannelId> {
        self.routes.iter().map(|e| *e.key()).collect()
    }

    /// Forward a locally originated audio packet from `sender` to all peers for this channel.
    /// `level` is the sender-reported loudness of the frame (`-dBov`), re-attached so the peer
    /// node can rank the speaker for its ambient receivers exactly as this node does.
    pub async fn forward_to_peers(
        &self,
        channel_id: &ChannelId,
        sender: &UserId,
        packet: &AurixPacket,
        level: Option<u8>,
    ) {
        if packet.header.has_flag(PacketFlags::Relay) {
            return;
        }
        let peers: Vec<SocketAddr> = match self.routes.get(channel_id) {
            Some(r) if !r.origin.is_empty() => r.origin.clone(),
            _ => return,
        };
        let labelled = level.map(|l| packet.with_audio_level(l));
        let packet = labelled.as_ref().unwrap_or(packet);
        let counter = self.relay_counter.fetch_add(1, Ordering::Relaxed);
        let data = AurixPacket::relay_envelope(packet, self.relay_ssrc, counter, sender)
            .seal(&self.keys)
            .freeze();
        self.send_all(&data, &peers, "origin").await;
    }

    async fn send_all(&self, data: &[u8], peers: &[SocketAddr], role: &str) {
        let now = Instant::now();
        let max_age = self.link_max_age();
        for peer_addr in peers {
            let sent = match self.transport_for(*peer_addr, now, max_age) {
                LinkTransport::Udp => match self.socket.send_to(data, *peer_addr).await {
                    Ok(_) => true,
                    Err(e) => {
                        warn!("Cascade forward to {} failed: {}", peer_addr, e);
                        false
                    }
                },
                LinkTransport::Tcp => self.send_tcp(*peer_addr, data, now),
            };
            if sent {
                aurix_metrics::CASCADE_FORWARDED
                    .with_label_values(&[role])
                    .inc();
            }
        }
    }

    /// The transport envelopes to `peer` take right now (see [`LinkState::transport`]).
    pub fn transport_for(
        &self,
        peer: SocketAddr,
        now: Instant,
        max_age: Duration,
    ) -> LinkTransport {
        match self.links.get(&peer) {
            Some(state) if self.tcp_enabled => state.transport(now, max_age),
            _ => LinkTransport::Udp,
        }
    }

    /// Current transport towards a peer (diagnostics / tests).
    pub fn peer_transport(&self, peer: SocketAddr) -> LinkTransport {
        self.transport_for(peer, Instant::now(), self.link_max_age())
    }

    /// Queue a sealed envelope on the peer's TCP link; drops (counted) when the link is down
    /// or its bounded queue is full.
    fn send_tcp(&self, peer: SocketAddr, data: &[u8], now: Instant) -> bool {
        let Some(tx) = self.tcp_links.get(&peer) else {
            aurix_metrics::CASCADE_TCP_DROPPED.inc();
            return false;
        };
        match tx.try_send(cascade_link::frame(data)) {
            Ok(()) => {
                if let Some(mut state) = self.links.get_mut(&peer) {
                    state.tcp_used = Some(now);
                }
                true
            }
            Err(_) => {
                aurix_metrics::CASCADE_TCP_DROPPED.inc();
                false
            }
        }
    }

    fn seal_control(&self, control: CascadeControl) -> Bytes {
        let counter = self.relay_counter.fetch_add(1, Ordering::Relaxed);
        AurixPacket::relay_envelope(
            &control.to_packet(),
            self.relay_ssrc,
            counter,
            &UserId::from_uuid(uuid::Uuid::nil()),
        )
        .seal(&self.keys)
        .freeze()
    }

    fn nonce_now(&self) -> u64 {
        self.started.elapsed().as_micros() as u64
    }

    /// Round trip implied by an echoed nonce, if plausible.
    fn rtt_from_nonce(&self, nonce: u64) -> Option<u32> {
        let elapsed = Duration::from_micros(self.nonce_now().checked_sub(nonce)?);
        (elapsed <= MAX_PLAUSIBLE_RTT).then_some(elapsed.as_millis() as u32)
    }

    /// What this node measured towards every allowed peer (for the registry).
    pub fn link_reports(&self) -> Vec<LinkReport> {
        let now = Instant::now();
        let max_age = self.link_max_age();
        self.links
            .iter()
            .map(|e| {
                let confirmed = e.value().confirmed(now, max_age);
                LinkReport {
                    peer: *e.key(),
                    transport: confirmed.map(|(t, _)| t),
                    rtt_ms: confirmed.map(|(_, rtt)| rtt),
                }
            })
            .collect()
    }

    /// Peers an envelope that arrived from `src` for the channel with `hash` must be
    /// re-forwarded to, if this node is a hub for that channel and the envelope may still hop.
    /// Returns `None` when there is no rule, `Some(vec![])` when a rule exists but the hop
    /// limit was reached (counted as `hop_limit`).
    pub fn forward_targets(&self, hash: u32, src: SocketAddr, hops: u8) -> Option<Vec<SocketAddr>> {
        let channel = *self.routes_by_hash.get(&hash)?.value();
        let route = self.routes.get(&channel)?;
        let targets = route.forward.get(&src)?;
        if targets.is_empty() {
            return None;
        }
        if hops >= MAX_RELAY_HOPS {
            aurix_metrics::CASCADE_FORWARDED
                .with_label_values(&["hop_limit"])
                .inc();
            return Some(Vec::new());
        }
        Some(targets.iter().copied().filter(|t| *t != src).collect())
    }

    /// Re-forward an authenticated inbound envelope along this node's hub rules (no-op when
    /// there are none). The client packet is re-wrapped with the same sender and `hops + 1`.
    pub async fn forward_inbound(&self, src: SocketAddr, inbound: &InboundRelay) {
        let Some(targets) =
            self.forward_targets(inbound.packet.header.channel_id_hash, src, inbound.hops)
        else {
            return;
        };
        if targets.is_empty() {
            return;
        }
        let counter = self.relay_counter.fetch_add(1, Ordering::Relaxed);
        let data = AurixPacket::relay_envelope_hop(
            &inbound.packet,
            self.relay_ssrc,
            counter,
            &inbound.sender,
            inbound.hops + 1,
        )
        .seal(&self.keys)
        .freeze();
        self.send_all(&data, &targets, "hub").await;
    }

    /// Decode and open a sealed envelope: returns its anti-replay counter, the sending user,
    /// the hop byte and the inner packet. Does not touch any replay window.
    fn open_envelope(&self, data: &[u8]) -> Result<(u64, UserId, Option<u8>, AurixPacket)> {
        let mut envelope = AurixPacket::decode_bounded(data, MAX_RELAY_PACKET_SIZE)?;
        if envelope.header.packet_type != PacketType::Relay
            || !envelope.header.has_flag(PacketFlags::Relay)
        {
            return Err(AurixError::Transport(
                "Cascade datagram is not a Relay envelope".into(),
            ));
        }
        if !envelope.open(&self.keys) {
            return Err(AurixError::AuthenticationFailed(
                "Cascade packet authentication failed".into(),
            ));
        }
        let counter = ((envelope.header.timestamp as u64) << 32) | envelope.header.sequence as u64;
        let (sender, hop, inner) = envelope.relay_inner_hop()?;
        Ok((counter, sender, hop, inner))
    }

    fn replay_check(&self, src: SocketAddr, counter: u64) -> Result<()> {
        let window = self.allowed_peers.get(&src).ok_or_else(|| {
            AurixError::AuthorizationDenied(format!("Cascade packet from unknown peer {src}"))
        })?;
        if !window.lock().check_and_update_u64(counter) {
            return Err(AurixError::AuthenticationFailed(
                "Replayed cascade packet".into(),
            ));
        }
        Ok(())
    }

    /// Validate an inbound envelope from `src` (known peer, tag, Relay envelope, replay
    /// window) and classify it as relayed audio or a control message.
    pub fn authenticate(&self, data: &[u8], src: SocketAddr) -> Result<Inbound> {
        if !self.allowed_peers.contains_key(&src) {
            return Err(AurixError::AuthorizationDenied(format!(
                "Cascade packet from unknown peer {src}"
            )));
        }
        let (counter, sender, hop, inner) = self.open_envelope(data)?;
        self.replay_check(src, counter)?;
        if let Some(control) = CascadeControl::from_packet(&inner)? {
            return Ok(Inbound::Control(control));
        }
        if inner.header.packet_type != PacketType::Audio
            && inner.header.packet_type != PacketType::AudioFec
        {
            return Err(AurixError::Transport(
                "Relay envelope must carry an audio packet".into(),
            ));
        }
        let hops = match hop {
            None => 1,
            Some(h) if (1..=MAX_RELAY_HOPS).contains(&h) => h,
            Some(h) => {
                return Err(AurixError::Transport(format!(
                    "Relay envelope hop count {h} out of range"
                )))
            }
        };
        Ok(Inbound::Audio(InboundRelay {
            sender,
            hops,
            packet: inner,
        }))
    }

    /// [`Self::authenticate`] for relayed audio only (control envelopes are an error).
    pub fn authenticate_inbound(&self, data: &[u8], src: SocketAddr) -> Result<InboundRelay> {
        match self.authenticate(data, src)? {
            Inbound::Audio(inbound) => Ok(inbound),
            Inbound::Control(_) => Err(AurixError::Transport(
                "Relay envelope must carry an audio packet".into(),
            )),
        }
    }

    /// Authenticate the first frame of an inbound TCP connection from `remote`: a sealed
    /// `Hello` naming an allowed peer whose IP is the one the connection comes from. Returns
    /// the peer address frames on that connection are attributed to.
    fn authenticate_hello(&self, data: &[u8], remote: SocketAddr) -> Result<SocketAddr> {
        let (counter, _, _, inner) = self.open_envelope(data)?;
        match CascadeControl::from_packet(&inner)? {
            Some(CascadeControl::Hello { addr }) => {
                let addr = aurix_common::addr::canonical(addr);
                if aurix_common::addr::canonical(remote).ip() != addr.ip() {
                    return Err(AurixError::AuthorizationDenied(format!(
                        "Cascade TCP Hello for {addr} from {remote}"
                    )));
                }
                self.replay_check(addr, counter)?;
                Ok(addr)
            }
            _ => Err(AurixError::Transport(
                "Cascade TCP connection did not start with Hello".into(),
            )),
        }
    }

    async fn handle_udp_control(&self, control: CascadeControl, src: SocketAddr) {
        match control {
            CascadeControl::Ping { nonce } => {
                let pong = self.seal_control(CascadeControl::Pong { nonce });
                if let Err(e) = self.socket.send_to(&pong, src).await {
                    debug!("Cascade pong to {} failed: {}", src, e);
                }
            }
            CascadeControl::Pong { nonce } => {
                if let Some(rtt) = self.rtt_from_nonce(nonce) {
                    if let Some(mut state) = self.links.get_mut(&src) {
                        state.record_udp(rtt, Instant::now());
                    }
                }
            }
            CascadeControl::Hello { .. } => {}
        }
    }

    /// Deliver an authenticated audio envelope: re-forward along hub rules, hand to the SFU.
    async fn deliver(
        &self,
        src: SocketAddr,
        inbound: InboundRelay,
        on_packet: &(dyn Fn(UserId, AurixPacket) + Send + Sync),
    ) {
        self.forward_inbound(src, &inbound).await;
        on_packet(inbound.sender, inbound.packet);
    }

    /// Receive relayed packets from peers (UDP and, when enabled, TCP), re-forward them along
    /// this node's hub rules and hand authenticated ones to `on_packet` for local delivery.
    /// Also starts the link prober.
    pub fn start_receiver(
        self: Arc<Self>,
        on_packet: Arc<dyn Fn(UserId, AurixPacket) + Send + Sync>,
    ) {
        if let Some(listener) = self.tcp_listener.lock().take() {
            tokio::spawn(self.clone().run_tcp_accept(listener, on_packet.clone()));
        }
        tokio::spawn(self.clone().run_prober());
        let socket = self.socket.clone();
        tokio::spawn(async move {
            let mut buf = vec![0u8; 2048];
            loop {
                let received = tokio::select! {
                    _ = self.shutdown.cancelled() => break,
                    r = socket.recv_from(&mut buf) => r,
                };
                match received {
                    Ok((len, src)) => {
                        if len < HEADER_SIZE {
                            continue;
                        }
                        match self.authenticate(&buf[..len], src) {
                            Ok(Inbound::Audio(inbound)) => {
                                self.deliver(src, inbound, on_packet.as_ref()).await;
                            }
                            Ok(Inbound::Control(control)) => {
                                self.handle_udp_control(control, src).await;
                            }
                            Err(e) => {
                                aurix_metrics::PACKETS_DROPPED.inc();
                                debug!("Dropping cascade packet from {}: {}", src, e);
                            }
                        }
                    }
                    Err(e) => {
                        error!("Cascade recv error: {}", e);
                        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                    }
                }
            }
        });
    }

    /// Ping every allowed peer each probe interval; open (and later close) TCP fallback links.
    async fn run_prober(self: Arc<Self>) {
        let interval = self.options.probe_interval;
        let max_age = self.link_max_age();
        loop {
            tokio::select! {
                _ = self.shutdown.cancelled() => return,
                _ = tokio::time::sleep(interval) => {}
            }
            let now = Instant::now();
            let peers: Vec<SocketAddr> = self.allowed_peers.iter().map(|e| *e.key()).collect();
            let (mut udp, mut tcp, mut unconfirmed) = (0i64, 0i64, 0i64);
            for peer in peers {
                let mut state = self.links.entry(peer).or_default();
                let reachable = self.socket.can_reach(peer);
                if reachable {
                    let ping = self.seal_control(CascadeControl::Ping {
                        nonce: self.nonce_now(),
                    });
                    if self.socket.try_send_to(&ping, peer).is_ok() {
                        state.udp_unanswered = state.udp_unanswered.saturating_add(1);
                    }
                }
                if self.tcp_enabled {
                    let udp_alive = state.transport(now, max_age) == LinkTransport::Udp
                        && state.confirmed(now, max_age).is_some();
                    let want_tcp =
                        !reachable || (!udp_alive && state.udp_unanswered >= UDP_PROBES_BEFORE_TCP);
                    if want_tcp {
                        let connected = self.tcp_links.contains_key(&peer);
                        let may_retry = state
                            .tcp_attempt
                            .is_none_or(|t| now.duration_since(t) >= self.tcp_backoff());
                        if !connected && may_retry {
                            state.tcp_attempt = Some(now);
                            self.clone().spawn_tcp_link(peer);
                        }
                        if let Some(tx) = self.tcp_links.get(&peer) {
                            let ping = self.seal_control(CascadeControl::Ping {
                                nonce: self.nonce_now(),
                            });
                            let _ = tx.try_send(cascade_link::frame(&ping));
                        }
                    } else if udp_alive
                        && state
                            .tcp_used
                            .is_none_or(|t| now.duration_since(t) >= TCP_IDLE_CLOSE)
                        && state
                            .tcp_attempt
                            .is_none_or(|t| now.duration_since(t) >= TCP_IDLE_CLOSE)
                        && self.tcp_links.remove(&peer).is_some()
                    {
                        debug!("Cascade TCP link to {} closed: UDP is back", peer);
                    }
                }
                match state.confirmed(now, max_age) {
                    Some((LinkTransport::Udp, _)) => udp += 1,
                    Some((LinkTransport::Tcp, _)) => tcp += 1,
                    None => unconfirmed += 1,
                }
            }
            aurix_metrics::CASCADE_LINKS
                .with_label_values(&["udp"])
                .set(udp);
            aurix_metrics::CASCADE_LINKS
                .with_label_values(&["tcp"])
                .set(tcp);
            aurix_metrics::CASCADE_LINKS
                .with_label_values(&["unconfirmed"])
                .set(unconfirmed);
        }
    }

    /// Open an outbound TCP link to `peer`: connect, send `Hello`, then pump queued frames out
    /// and read the peer's answers (pongs). The queue is registered immediately so probes and
    /// audio can be queued while connecting; it disappears when the link ends.
    fn spawn_tcp_link(self: Arc<Self>, peer: SocketAddr) {
        let (tx, mut rx) = mpsc::channel::<Bytes>(TCP_QUEUE_FRAMES);
        self.tcp_links.insert(peer, tx.clone());
        tokio::spawn(async move {
            let result: std::io::Result<()> = async {
                let stream = tokio::time::timeout(TCP_CONNECT_TIMEOUT, TcpStream::connect(peer))
                    .await
                    .map_err(|_| {
                        std::io::Error::new(std::io::ErrorKind::TimedOut, "connect timeout")
                    })??;
                let _ = stream.set_nodelay(true);
                let (mut rd, mut wr) = stream.into_split();
                let hello = self.seal_control(CascadeControl::Hello {
                    addr: self.hello_addr_for(peer),
                });
                cascade_link::write_frame(&mut wr, &hello).await?;
                debug!("Cascade TCP link to {} up", peer);
                let cancel = CancellationToken::new();
                let writer_cancel = cancel.clone();
                let writer = tokio::spawn(async move {
                    while let Some(frame) = rx.recv().await {
                        if wr.write_all(&frame).await.is_err() {
                            break;
                        }
                    }
                    writer_cancel.cancel();
                });
                let mut buf = vec![0u8; cascade_link::MAX_FRAME_BYTES];
                loop {
                    let read = tokio::select! {
                        _ = cancel.cancelled() => break,
                        _ = self.shutdown.cancelled() => break,
                        r = cascade_link::read_frame(&mut rd, &mut buf) => r,
                    };
                    match read? {
                        None => break,
                        Some(n) => match self.authenticate(&buf[..n], peer) {
                            Ok(Inbound::Control(CascadeControl::Pong { nonce })) => {
                                if let Some(rtt) = self.rtt_from_nonce(nonce) {
                                    if let Some(mut state) = self.links.get_mut(&peer) {
                                        state.record_tcp(rtt, Instant::now());
                                    }
                                }
                            }
                            Ok(_) => {}
                            Err(e) => {
                                aurix_metrics::PACKETS_DROPPED.inc();
                                debug!("Dropping cascade TCP frame from {}: {}", peer, e);
                            }
                        },
                    }
                }
                writer.abort();
                Ok(())
            }
            .await;
            if let Err(e) = result {
                debug!("Cascade TCP link to {} ended: {}", peer, e);
            }
            self.tcp_links
                .remove_if(&peer, |_, current| current.same_channel(&tx));
        });
    }

    /// Accept inbound TCP links: each must start with an authenticated `Hello` naming an
    /// allowed peer; frames after that are that peer's envelopes (pings are answered on the
    /// same connection).
    async fn run_tcp_accept(
        self: Arc<Self>,
        listener: TcpListener,
        on_packet: Arc<dyn Fn(UserId, AurixPacket) + Send + Sync>,
    ) {
        loop {
            let accepted = tokio::select! {
                _ = self.shutdown.cancelled() => return,
                r = listener.accept() => r,
            };
            let (stream, remote) = match accepted {
                Ok(conn) => conn,
                Err(e) => {
                    warn!("Cascade TCP accept error: {}", e);
                    tokio::time::sleep(Duration::from_millis(50)).await;
                    continue;
                }
            };
            if self.inbound_tcp.load(Ordering::Relaxed) >= MAX_INBOUND_TCP {
                warn!(
                    "Cascade TCP connection from {} refused: limit reached",
                    remote
                );
                continue;
            }
            self.inbound_tcp.fetch_add(1, Ordering::Relaxed);
            let relay = self.clone();
            let on_packet = on_packet.clone();
            tokio::spawn(async move {
                relay.serve_inbound_tcp(stream, remote, on_packet).await;
                relay.inbound_tcp.fetch_sub(1, Ordering::Relaxed);
            });
        }
    }

    async fn serve_inbound_tcp(
        &self,
        stream: TcpStream,
        remote: SocketAddr,
        on_packet: Arc<dyn Fn(UserId, AurixPacket) + Send + Sync>,
    ) {
        let _ = stream.set_nodelay(true);
        let (mut rd, mut wr) = stream.into_split();
        let mut buf = vec![0u8; cascade_link::MAX_FRAME_BYTES];
        let peer = match tokio::time::timeout(
            TCP_HELLO_TIMEOUT,
            cascade_link::read_frame(&mut rd, &mut buf),
        )
        .await
        {
            Ok(Ok(Some(n))) => match self.authenticate_hello(&buf[..n], remote) {
                Ok(peer) => peer,
                Err(e) => {
                    aurix_metrics::PACKETS_DROPPED.inc();
                    debug!("Cascade TCP connection from {} refused: {}", remote, e);
                    return;
                }
            },
            _ => {
                debug!("Cascade TCP connection from {} closed before Hello", remote);
                return;
            }
        };
        debug!("Cascade TCP link from {} ({}) up", peer, remote);
        let (tx, mut rx) = mpsc::channel::<Bytes>(64);
        let cancel = CancellationToken::new();
        let writer_cancel = cancel.clone();
        let writer = tokio::spawn(async move {
            while let Some(frame) = rx.recv().await {
                if wr.write_all(&frame).await.is_err() {
                    break;
                }
            }
            writer_cancel.cancel();
        });
        loop {
            let read = tokio::select! {
                _ = cancel.cancelled() => break,
                _ = self.shutdown.cancelled() => break,
                r = tokio::time::timeout(TCP_INBOUND_IDLE, cascade_link::read_frame(&mut rd, &mut buf)) => r,
            };
            let n = match read {
                Ok(Ok(Some(n))) => n,
                Ok(Ok(None)) => break,
                Ok(Err(e)) => {
                    debug!("Cascade TCP link from {} ended: {}", peer, e);
                    break;
                }
                Err(_) => {
                    debug!("Cascade TCP link from {} idle, closing", peer);
                    break;
                }
            };
            match self.authenticate(&buf[..n], peer) {
                Ok(Inbound::Audio(inbound)) => {
                    self.deliver(peer, inbound, on_packet.as_ref()).await;
                }
                Ok(Inbound::Control(CascadeControl::Ping { nonce })) => {
                    let pong = self.seal_control(CascadeControl::Pong { nonce });
                    let _ = tx.try_send(cascade_link::frame(&pong));
                }
                Ok(Inbound::Control(_)) => {}
                Err(e) => {
                    aurix_metrics::PACKETS_DROPPED.inc();
                    debug!("Dropping cascade TCP frame from {}: {}", peer, e);
                }
            }
        }
        writer.abort();
    }

    pub fn has_peers(&self, channel_id: &ChannelId) -> bool {
        self.routes
            .get(channel_id)
            .is_some_and(|r| !r.origin.is_empty())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;

    #[tokio::test]
    async fn rejects_unknown_peer_and_bad_tag() {
        let peer: SocketAddr = "127.0.0.1:45001".parse().unwrap();
        let relay = CascadeRelay::new(
            "127.0.0.1:0".parse().unwrap(),
            MediaNodeId::new(),
            "0123456789abcdef",
            &[peer.to_string()],
            CascadeOptions::default(),
        )
        .await
        .unwrap();

        let pkt = AurixPacket::audio(1, 0, 42, 7, Bytes::from_static(b"opus"));
        let sender = UserId::new();
        let env = AurixPacket::relay_envelope(&pkt, 0x1234, 1, &sender);
        let good = env.seal(&MediaKeys::derive(b"0123456789abcdef"));
        let bad = env.seal(&MediaKeys::derive(b"wrong-secret-wrong-secret"));
        let unauth = env.encode();
        // A bare (non-envelope) client packet sealed with the right key is refused too.
        let mut bare = pkt.clone();
        bare.header.set_flag(PacketFlags::Relay);
        let bare = bare.seal(&MediaKeys::derive(b"0123456789abcdef"));

        assert!(relay
            .authenticate_inbound(&good, "127.0.0.1:45002".parse().unwrap())
            .is_err());
        assert!(relay.authenticate_inbound(&bad, peer).is_err());
        assert!(relay.authenticate_inbound(&unauth, peer).is_err());
        assert!(relay.authenticate_inbound(&bare, peer).is_err());
        let inbound = relay.authenticate_inbound(&good, peer).unwrap();
        assert_eq!(inbound.sender, sender);
        assert_eq!(inbound.hops, 1);
        assert_eq!(inbound.packet.header.ssrc, 42);
        assert_eq!(&inbound.packet.payload[..], b"opus");
        assert!(
            relay.authenticate_inbound(&good, peer).is_err(),
            "replay must be rejected"
        );
        assert!(relay
            .add_peer(ChannelId::new(), "127.0.0.1:45002".parse().unwrap())
            .is_err());
    }

    #[tokio::test]
    async fn dynamic_peers_are_added_and_pruned_but_static_kept() {
        let static_peer: SocketAddr = "127.0.0.1:45001".parse().unwrap();
        let dyn_a: SocketAddr = "127.0.0.1:45010".parse().unwrap();
        let dyn_b: SocketAddr = "127.0.0.1:45011".parse().unwrap();
        let relay = CascadeRelay::new(
            "127.0.0.1:0".parse().unwrap(),
            MediaNodeId::new(),
            "0123456789abcdef",
            &[static_peer.to_string()],
            CascadeOptions::default(),
        )
        .await
        .unwrap();
        let ch = ChannelId::new();

        relay.set_dynamic_peers([dyn_a, dyn_b].into_iter().collect());
        assert_eq!(relay.allowed_peers().len(), 3);
        // Unknown addresses are silently filtered out of the channel list.
        relay.set_channel_peers(ch, &[dyn_a, dyn_b, "127.0.0.1:1".parse().unwrap()]);
        assert_eq!(relay.channel_peers(&ch).len(), 2);

        let keys = MediaKeys::derive(b"0123456789abcdef");
        let pkt = AurixPacket::audio(1, 0, 42, 7, Bytes::from_static(b"opus"));
        let good = AurixPacket::relay_envelope(&pkt, 0x1234, 1, &UserId::new()).seal(&keys);
        assert!(relay.authenticate_inbound(&good, dyn_b).is_ok());

        // Node B went away: its address must be refused and removed from channels.
        relay.set_dynamic_peers([dyn_a].into_iter().collect());
        assert_eq!(relay.channel_peers(&ch), vec![dyn_a]);
        let pkt2 = AurixPacket::relay_envelope(
            &AurixPacket::audio(1, 0, 43, 7, Bytes::from_static(b"opus")),
            0x1234,
            2,
            &UserId::new(),
        )
        .seal(&keys);
        assert!(relay.authenticate_inbound(&pkt2, dyn_b).is_err());
        assert!(relay.authenticate_inbound(&pkt2, dyn_a).is_ok());

        // Static peer survives an empty discovery result.
        relay.set_dynamic_peers(HashSet::new());
        assert_eq!(relay.allowed_peers(), vec![static_peer]);
        assert!(relay.channel_peers(&ch).is_empty());
        relay.set_channel_peers(ch, &[]);
        assert!(!relay.has_peers(&ch));
    }

    #[tokio::test]
    async fn restarted_peer_continues_above_its_previous_counters() {
        let peer: SocketAddr = "127.0.0.1:45020".parse().unwrap();
        let relay = CascadeRelay::new(
            "127.0.0.1:0".parse().unwrap(),
            MediaNodeId::new(),
            "0123456789abcdef",
            &[peer.to_string()],
            CascadeOptions::default(),
        )
        .await
        .unwrap();
        let keys = MediaKeys::derive(b"0123456789abcdef");
        let sender = UserId::new();
        let envelope = |counter: u64| {
            AurixPacket::relay_envelope(
                &AurixPacket::audio(1, 0, 42, 7, Bytes::from_static(b"opus")),
                0x1234,
                counter,
                &sender,
            )
            .seal(&keys)
        };

        // The previous process on `peer` started now and relayed 1000 envelopes.
        let start = CascadeRelay::initial_relay_counter();
        assert!(start >= 1 << 32, "counter must carry the start time");
        for i in 0..1000 {
            assert!(relay
                .authenticate_inbound(&envelope(start + i), peer)
                .is_ok());
        }
        // A process that restarted from zero would be stuck behind the replay window ...
        assert!(relay.authenticate_inbound(&envelope(0), peer).is_err());
        assert!(relay.authenticate_inbound(&envelope(500), peer).is_err());
        // ... whereas one started a second later is accepted at once.
        let restarted = start + (1 << 32);
        assert!(relay
            .authenticate_inbound(&envelope(restarted), peer)
            .is_ok());
        assert!(relay
            .authenticate_inbound(&envelope(restarted + 1), peer)
            .is_ok());
        assert!(
            relay
                .authenticate_inbound(&envelope(restarted), peer)
                .is_err(),
            "replay must still be rejected"
        );
    }

    fn audio(ch: &ChannelId, ssrc: u32) -> AurixPacket {
        AurixPacket::audio(1, 0, ssrc, channel_id_hash(ch), Bytes::from_static(b"opus"))
    }

    #[tokio::test]
    async fn routes_filter_unknown_peers_and_never_forward_back_to_source() {
        let a: SocketAddr = "127.0.0.1:45030".parse().unwrap();
        let b: SocketAddr = "127.0.0.1:45031".parse().unwrap();
        let c: SocketAddr = "127.0.0.1:45032".parse().unwrap();
        let stranger: SocketAddr = "127.0.0.1:45039".parse().unwrap();
        let relay = CascadeRelay::new(
            "127.0.0.1:0".parse().unwrap(),
            MediaNodeId::new(),
            "0123456789abcdef",
            &[],
            CascadeOptions::default(),
        )
        .await
        .unwrap();
        relay.set_dynamic_peers([a, b, c].into_iter().collect());
        let ch = ChannelId::new();
        let hash = channel_id_hash(&ch);
        relay.set_channel_route(
            ch,
            ChannelRoute {
                origin: vec![a, stranger, a],
                forward: [
                    (a, vec![b, c, a, stranger]),
                    (b, vec![b]),
                    (stranger, vec![a]),
                ]
                .into_iter()
                .collect(),
            },
        );
        let route = relay.channel_route(&ch).unwrap();
        assert_eq!(route.origin, vec![a]);
        assert_eq!(route.forward.len(), 1, "self-only and unknown rules vanish");
        assert_eq!(route.forward[&a], vec![b, c]);
        assert!(route.is_hub());
        assert_eq!(relay.forward_targets(hash, a, 1), Some(vec![b, c]));
        assert_eq!(relay.forward_targets(hash, b, 1), None);
        assert_eq!(relay.forward_targets(hash, a, MAX_RELAY_HOPS), Some(vec![]));
        assert_eq!(relay.forward_targets(hash ^ 1, a, 1), None);

        // Peer C disappears: it leaves the forward lists; peer A disappearing removes its rule.
        relay.set_dynamic_peers([a, b].into_iter().collect());
        assert_eq!(relay.forward_targets(hash, a, 2), Some(vec![b]));
        relay.set_dynamic_peers([b].into_iter().collect());
        assert!(relay.channel_route(&ch).is_none() || !relay.channel_route(&ch).unwrap().is_hub());
        assert_eq!(relay.forward_targets(hash, a, 1), None);

        // retain_channels drops hub duty for channels no longer planned.
        relay.set_dynamic_peers([a, b].into_iter().collect());
        relay.set_channel_route(
            ch,
            ChannelRoute {
                origin: vec![],
                forward: [(a, vec![b])].into_iter().collect(),
            },
        );
        assert_eq!(relay.routed_channels(), vec![ch]);
        relay.retain_channels(&HashSet::new());
        assert!(relay.routed_channels().is_empty());
        assert_eq!(relay.forward_targets(hash, a, 1), None);
    }

    /// Origin → `MAX_RELAY_HOPS - 1` chained hubs → leaf over real loopback sockets: every
    /// node receives the packet once with the original sender / SSRC / payload; the origin's
    /// own envelope never comes back to it, and the leaf (at the hop cap) never re-forwards.
    #[tokio::test]
    async fn hop_capped_chain_delivers_once_and_stops_at_leaf() {
        let secret = "0123456789abcdef";
        let mk = || async {
            Arc::new(
                CascadeRelay::new(
                    "127.0.0.1:0".parse().unwrap(),
                    MediaNodeId::new(),
                    secret,
                    &[],
                    CascadeOptions::default(),
                )
                .await
                .unwrap(),
            )
        };
        let origin = mk().await;
        let mut hubs = Vec::new();
        for _ in 1..MAX_RELAY_HOPS {
            hubs.push(mk().await);
        }
        let leaf = mk().await;
        let chain: Vec<Arc<CascadeRelay>> = std::iter::once(origin.clone())
            .chain(hubs.iter().cloned())
            .chain(std::iter::once(leaf.clone()))
            .collect();
        let addrs: Vec<SocketAddr> = chain.iter().map(|r| r.local_addr()).collect();
        for r in &chain {
            r.set_dynamic_peers(addrs.iter().copied().collect());
        }
        let ch = ChannelId::new();
        origin.set_channel_route(ch, ChannelRoute::mesh(&[addrs[1]]));
        for (i, hub) in hubs.iter().enumerate() {
            let me = i + 1;
            hub.set_channel_route(
                ch,
                ChannelRoute {
                    origin: vec![],
                    forward: [(addrs[me - 1], vec![addrs[me + 1]])].into_iter().collect(),
                },
            );
        }
        // The leaf has a (bogus) rule pointing back at the origin: an envelope at
        // MAX_RELAY_HOPS must still not be re-forwarded.
        leaf.set_channel_route(
            ch,
            ChannelRoute {
                origin: vec![],
                forward: [(addrs[addrs.len() - 2], vec![addrs[0]])]
                    .into_iter()
                    .collect(),
            },
        );

        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<(usize, UserId, AurixPacket)>();
        for (idx, relay) in chain.iter().enumerate() {
            let tx = tx.clone();
            relay
                .clone()
                .start_receiver(Arc::new(move |sender, packet| {
                    let _ = tx.send((idx, sender, packet));
                }));
        }
        let sender = UserId::new();
        origin
            .forward_to_peers(&ch, &sender, &audio(&ch, 4242), Some(30))
            .await;

        let expected: Vec<usize> = (1..chain.len()).collect();
        let mut got = Vec::new();
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_millis(800);
        while let Ok(Some(item)) = tokio::time::timeout_at(deadline, rx.recv()).await {
            got.push(item);
            if got.len() >= expected.len() {
                // Give any stray (looped / duplicated) datagram a chance to show up.
                tokio::time::sleep(std::time::Duration::from_millis(150)).await;
                while let Ok(item) = rx.try_recv() {
                    got.push(item);
                }
                break;
            }
        }
        let mut idxs: Vec<usize> = got.iter().map(|g| g.0).collect();
        idxs.sort_unstable();
        assert_eq!(
            idxs, expected,
            "each node delivers locally exactly once, nothing returns to the origin"
        );
        for (_, from, mut pkt) in got {
            assert_eq!(from, sender);
            assert_eq!(pkt.header.ssrc, 4242);
            assert_eq!(pkt.take_audio_level(), Some(30));
            assert_eq!(&pkt.payload[..], b"opus");
        }
    }

    #[tokio::test]
    async fn hop_count_is_validated_and_incremented() {
        let peer: SocketAddr = "127.0.0.1:45040".parse().unwrap();
        let relay = CascadeRelay::new(
            "127.0.0.1:0".parse().unwrap(),
            MediaNodeId::new(),
            "0123456789abcdef",
            &[peer.to_string()],
            CascadeOptions::default(),
        )
        .await
        .unwrap();
        let keys = MediaKeys::derive(b"0123456789abcdef");
        let ch = ChannelId::new();
        let sender = UserId::new();
        let env = |counter: u64, hop: u8| {
            AurixPacket::relay_envelope_hop(&audio(&ch, 1), 0x1234, counter, &sender, hop)
                .seal(&keys)
        };
        assert_eq!(
            relay.authenticate_inbound(&env(1, 2), peer).unwrap().hops,
            2
        );
        assert_eq!(
            relay
                .authenticate_inbound(&env(2, MAX_RELAY_HOPS), peer)
                .unwrap()
                .hops,
            MAX_RELAY_HOPS
        );
        assert!(relay.authenticate_inbound(&env(3, 0), peer).is_err());
        assert!(relay
            .authenticate_inbound(&env(4, MAX_RELAY_HOPS + 1), peer)
            .is_err());
    }

    const FAST_PROBES: CascadeOptions = CascadeOptions {
        tcp_fallback: true,
        probe_interval: Duration::from_millis(40),
    };

    /// A relay whose TCP listener sits on a port where UDP is a black hole (bound, never
    /// read) while its real UDP socket lives elsewhere: peers addressing `tcp_addr` can only
    /// get through over TCP.
    async fn udp_blackholed_relay(
        secret: &str,
    ) -> (Arc<CascadeRelay>, SocketAddr, std::net::UdpSocket) {
        let (listener, _) =
            aurix_common::net::bind_tcp("127.0.0.1:0".parse().unwrap(), false).unwrap();
        let tcp_addr = listener.local_addr().unwrap();
        let blackhole = std::net::UdpSocket::bind(tcp_addr).unwrap();
        let udp = bind_media_socket("127.0.0.1:0".parse().unwrap()).unwrap();
        let relay = CascadeRelay::with_sockets(
            udp,
            Some(listener),
            MediaNodeId::new(),
            secret,
            &[],
            FAST_PROBES,
        )
        .unwrap();
        (Arc::new(relay), tcp_addr, blackhole)
    }

    async fn wait_for(deadline_ms: u64, what: &str, mut ok: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_millis(deadline_ms);
        while !ok() {
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            tokio::time::sleep(Duration::from_millis(15)).await;
        }
    }

    /// UDP towards B is silently dropped: A's probes go unanswered, A opens the TCP fallback,
    /// measures B over it and delivers audio through it; B still measures A over UDP.
    #[tokio::test]
    async fn udp_blackhole_falls_back_to_tcp_and_delivers_audio() {
        let secret = "0123456789abcdef";
        let a = Arc::new(
            CascadeRelay::new(
                "127.0.0.1:0".parse().unwrap(),
                MediaNodeId::new(),
                secret,
                &[],
                FAST_PROBES,
            )
            .await
            .unwrap(),
        );
        let (b, b_addr, _blackhole) = udp_blackholed_relay(secret).await;
        // B's pings leave from its real UDP socket, so A must accept that source as well.
        a.set_dynamic_peers([b_addr, b.local_addr()].into_iter().collect());
        b.set_dynamic_peers([a.local_addr()].into_iter().collect());
        let ch = ChannelId::new();
        a.set_channel_route(ch, ChannelRoute::mesh(&[b_addr]));
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<(UserId, AurixPacket)>();
        a.clone().start_receiver(Arc::new(|_, _| {}));
        b.clone().start_receiver(Arc::new(move |s, p| {
            let _ = tx.send((s, p));
        }));

        wait_for(5000, "TCP fallback towards B", || {
            a.link_reports()
                .iter()
                .any(|r| r.peer == b_addr && r.transport == Some(LinkTransport::Tcp))
        })
        .await;
        assert_eq!(a.peer_transport(b_addr), LinkTransport::Tcp);
        let sender = UserId::new();
        a.forward_to_peers(&ch, &sender, &audio(&ch, 7), Some(12))
            .await;
        let (from, mut pkt) = tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .expect("audio over the TCP fallback")
            .unwrap();
        assert_eq!(from, sender);
        assert_eq!(pkt.header.ssrc, 7);
        assert_eq!(pkt.take_audio_level(), Some(12));
        assert_eq!(&pkt.payload[..], b"opus");

        wait_for(3000, "B measuring A over UDP", || {
            b.link_reports().iter().any(|r| {
                r.peer == a.local_addr()
                    && r.transport == Some(LinkTransport::Udp)
                    && r.rtt_ms.is_some()
            })
        })
        .await;
    }

    /// Inbound TCP connections must open with a sealed `Hello` for an allowed peer whose IP
    /// matches the connection: garbage, a stranger's Hello and a Hello claiming another
    /// allowed node's address are all closed without any frame being accepted.
    #[tokio::test]
    async fn tcp_connections_without_a_valid_hello_are_closed() {
        let secret = "0123456789abcdef";
        let (b, b_addr, _blackhole) = udp_blackholed_relay(secret).await;
        let other_host: SocketAddr = "127.0.0.2:45050".parse().unwrap();
        b.set_dynamic_peers([other_host].into_iter().collect());
        let delivered = Arc::new(AtomicUsize::new(0));
        let counter = delivered.clone();
        b.clone().start_receiver(Arc::new(move |_, _| {
            counter.fetch_add(1, Ordering::Relaxed);
        }));
        let stranger = CascadeRelay::new(
            "127.0.0.1:0".parse().unwrap(),
            MediaNodeId::new(),
            secret,
            &[],
            CascadeOptions {
                tcp_fallback: false,
                ..FAST_PROBES
            },
        )
        .await
        .unwrap();
        let ch = ChannelId::new();
        let audio_env = AurixPacket::relay_envelope(&audio(&ch, 1), 1, 1, &UserId::new())
            .seal(&stranger.keys)
            .freeze();

        let attempts: Vec<(&str, Bytes)> = vec![
            ("garbage", Bytes::from_static(b"not an envelope at all")),
            (
                "stranger hello",
                stranger.seal_control(CascadeControl::Hello {
                    addr: stranger.local_addr(),
                }),
            ),
            (
                "hello claiming another host",
                stranger.seal_control(CascadeControl::Hello { addr: other_host }),
            ),
            ("audio before hello", audio_env),
        ];
        for (name, first) in attempts {
            let mut stream = TcpStream::connect(b_addr).await.unwrap();
            cascade_link::write_frame(&mut stream, &first)
                .await
                .unwrap();
            let mut buf = vec![0u8; cascade_link::MAX_FRAME_BYTES];
            let closed = tokio::time::timeout(
                Duration::from_secs(3),
                cascade_link::read_frame(&mut stream, &mut buf),
            )
            .await
            .unwrap_or_else(|_| panic!("{name}: connection not closed"));
            assert!(
                matches!(closed, Ok(None) | Err(_)),
                "{name}: expected the relay to close the connection"
            );
        }
        assert_eq!(delivered.load(Ordering::Relaxed), 0);
        assert_eq!(b.inbound_tcp.load(Ordering::Relaxed), 0);
    }

    /// The peer behind a TCP link restarts: the link dies, A reconnects after the backoff
    /// with a fresh `Hello`, and audio flows to the new instance (fresh replay window).
    #[tokio::test]
    async fn tcp_link_reconnects_after_peer_restart() {
        let secret = "0123456789abcdef";
        let a = Arc::new(
            CascadeRelay::new(
                "127.0.0.1:0".parse().unwrap(),
                MediaNodeId::new(),
                secret,
                &[],
                FAST_PROBES,
            )
            .await
            .unwrap(),
        );
        let (b, b_addr, _blackhole) = udp_blackholed_relay(secret).await;
        a.set_dynamic_peers([b_addr, b.local_addr()].into_iter().collect());
        b.set_dynamic_peers([a.local_addr()].into_iter().collect());
        let ch = ChannelId::new();
        a.set_channel_route(ch, ChannelRoute::mesh(&[b_addr]));
        a.clone().start_receiver(Arc::new(|_, _| {}));
        let (tx1, mut rx1) = tokio::sync::mpsc::unbounded_channel::<UserId>();
        b.clone().start_receiver(Arc::new(move |s, _| {
            let _ = tx1.send(s);
        }));
        wait_for(5000, "first TCP link", || {
            a.peer_transport(b_addr) == LinkTransport::Tcp
        })
        .await;
        let sender = UserId::new();
        a.forward_to_peers(&ch, &sender, &audio(&ch, 1), None).await;
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(2), rx1.recv())
                .await
                .unwrap(),
            Some(sender)
        );

        // B goes away; its listener port is reused by the restarted instance.
        b.shutdown();
        drop(b);
        wait_for(3000, "listener release", || {
            std::net::TcpListener::bind(b_addr).is_ok()
        })
        .await;
        let (listener, _) = aurix_common::net::bind_tcp(b_addr, false).unwrap();
        let udp = bind_media_socket("127.0.0.1:0".parse().unwrap()).unwrap();
        let b2 = Arc::new(
            CascadeRelay::with_sockets(
                udp,
                Some(listener),
                MediaNodeId::new(),
                secret,
                &[],
                FAST_PROBES,
            )
            .unwrap(),
        );
        b2.set_dynamic_peers([a.local_addr()].into_iter().collect());
        let (tx2, mut rx2) = tokio::sync::mpsc::unbounded_channel::<UserId>();
        b2.clone().start_receiver(Arc::new(move |s, _| {
            let _ = tx2.send(s);
        }));

        let deadline = Instant::now() + Duration::from_secs(8);
        loop {
            a.forward_to_peers(&ch, &sender, &audio(&ch, 2), None).await;
            if let Ok(Some(from)) =
                tokio::time::timeout(Duration::from_millis(100), rx2.recv()).await
            {
                assert_eq!(from, sender);
                break;
            }
            assert!(
                Instant::now() < deadline,
                "A never reconnected to the restarted B"
            );
        }
        assert_eq!(a.peer_transport(b_addr), LinkTransport::Tcp);
    }

    /// A relay started with `tcp_fallback = false` neither listens nor connects over TCP.
    #[tokio::test]
    async fn tcp_fallback_can_be_disabled() {
        let relay = CascadeRelay::new(
            "127.0.0.1:0".parse().unwrap(),
            MediaNodeId::new(),
            "0123456789abcdef",
            &[],
            CascadeOptions {
                tcp_fallback: false,
                ..FAST_PROBES
            },
        )
        .await
        .unwrap();
        assert!(!relay.tcp_enabled);
        assert!(TcpStream::connect(relay.local_addr()).await.is_err());
        let peer: SocketAddr = "127.0.0.1:45060".parse().unwrap();
        relay.set_dynamic_peers([peer].into_iter().collect());
        assert_eq!(relay.peer_transport(peer), LinkTransport::Udp);
    }
}
