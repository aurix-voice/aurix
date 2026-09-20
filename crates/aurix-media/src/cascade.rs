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

use crate::transport::{bind_media_socket, MediaSocket};
use aurix_common::crypto::MediaKeys;
use aurix_common::error::{AurixError, Result};
use aurix_common::protocol::{
    channel_id_hash, AurixPacket, PacketFlags, PacketType, ReplayWindow, HEADER_SIZE,
    MAX_RELAY_HOPS, MAX_RELAY_PACKET_SIZE,
};
use aurix_common::types::*;
use dashmap::DashMap;
use parking_lot::Mutex;
use std::collections::{BTreeMap, HashSet};
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use tracing::{debug, error, info, warn};

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

pub struct CascadeRelay {
    /// channel_id -> where this node sends and re-forwards that channel's audio.
    routes: Arc<DashMap<ChannelId, ChannelRoute>>,
    /// `channel_id_hash` -> channel for the routes above (envelopes only carry the hash).
    routes_by_hash: Arc<DashMap<u32, ChannelId>>,
    /// Peers we accept relayed traffic from, each with its own anti-replay window.
    allowed_peers: Arc<DashMap<SocketAddr, Mutex<ReplayWindow>>>,
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
}

impl CascadeRelay {
    pub async fn new(
        bind_addr: SocketAddr,
        local_node_id: MediaNodeId,
        secret: &str,
        peers: &[String],
    ) -> Result<Self> {
        if secret.len() < 16 {
            return Err(AurixError::InvalidConfiguration(
                "media.cascade_secret must be at least 16 characters".into(),
            ));
        }
        let socket = bind_media_socket(bind_addr)
            .map_err(|e| AurixError::Transport(format!("Cascade bind failed: {e}")))?;
        let local_addr = socket.local_addr();
        let allowed_peers: Arc<DashMap<SocketAddr, Mutex<ReplayWindow>>> = Arc::new(DashMap::new());
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
            allowed_peers.insert(addr, Mutex::new(ReplayWindow::default()));
            static_peers.insert(addr);
        }
        info!(
            "Cascade relay listening on {} with {} static peers",
            local_addr,
            allowed_peers.len()
        );
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
                .or_insert_with(|| Mutex::new(ReplayWindow::default()));
            info!("Cascade peer discovered: {}", added);
        }
        for removed in current.difference(&peers) {
            if self.static_peers.contains(removed) {
                continue;
            }
            self.allowed_peers.remove(removed);
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
        for peer_addr in peers {
            if let Err(e) = self.socket.send_to(data, *peer_addr).await {
                warn!("Cascade forward to {} failed: {}", peer_addr, e);
            } else {
                aurix_metrics::CASCADE_FORWARDED
                    .with_label_values(&[role])
                    .inc();
            }
        }
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

    /// Validate an inbound relayed datagram (known peer, tag, Relay envelope, replay window)
    /// and return the sending user, the hop count and the plaintext client packet it carries.
    pub fn authenticate_inbound(&self, data: &[u8], src: SocketAddr) -> Result<InboundRelay> {
        let window = self.allowed_peers.get(&src).ok_or_else(|| {
            AurixError::AuthorizationDenied(format!("Cascade packet from unknown peer {src}"))
        })?;
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
        if !window.lock().check_and_update_u64(counter) {
            return Err(AurixError::AuthenticationFailed(
                "Replayed cascade packet".into(),
            ));
        }
        let (sender, hop, inner) = envelope.relay_inner_hop()?;
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
        Ok(InboundRelay {
            sender,
            hops,
            packet: inner,
        })
    }

    /// Receive relayed packets from peers, re-forward them along this node's hub rules and
    /// hand authenticated ones to `on_packet` for local delivery.
    pub fn start_receiver(
        self: Arc<Self>,
        on_packet: Arc<dyn Fn(UserId, AurixPacket) + Send + Sync>,
    ) {
        let socket = self.socket.clone();
        tokio::spawn(async move {
            let mut buf = vec![0u8; 2048];
            loop {
                match socket.recv_from(&mut buf).await {
                    Ok((len, src)) => {
                        if len < HEADER_SIZE {
                            continue;
                        }
                        match self.authenticate_inbound(&buf[..len], src) {
                            Ok(inbound) => {
                                self.forward_inbound(src, &inbound).await;
                                on_packet(inbound.sender, inbound.packet);
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

    /// Origin → hub → remote hub → leaf over real loopback sockets: the leaf receives the
    /// packet once with the original sender / SSRC / payload and hop count 3; the origin's
    /// own envelope never comes back to it, and a leaf never re-forwards.
    #[tokio::test]
    async fn three_hop_tree_delivers_once_and_stops_at_leaf() {
        let secret = "0123456789abcdef";
        let mk = || async {
            Arc::new(
                CascadeRelay::new(
                    "127.0.0.1:0".parse().unwrap(),
                    MediaNodeId::new(),
                    secret,
                    &[],
                )
                .await
                .unwrap(),
            )
        };
        let origin = mk().await;
        let hub1 = mk().await;
        let hub2 = mk().await;
        let leaf = mk().await;
        let (ao, a1, a2, al) = (
            origin.local_addr(),
            hub1.local_addr(),
            hub2.local_addr(),
            leaf.local_addr(),
        );
        for r in [&origin, &hub1, &hub2, &leaf] {
            r.set_dynamic_peers([ao, a1, a2, al].into_iter().collect());
        }
        let ch = ChannelId::new();
        origin.set_channel_route(ch, ChannelRoute::mesh(&[a1]));
        hub1.set_channel_route(
            ch,
            ChannelRoute {
                origin: vec![],
                forward: [(ao, vec![a2])].into_iter().collect(),
            },
        );
        hub2.set_channel_route(
            ch,
            ChannelRoute {
                origin: vec![],
                forward: [(a1, vec![al])].into_iter().collect(),
            },
        );
        // The leaf has a (bogus) rule pointing back at the origin: a hop-3 envelope must
        // still not be re-forwarded.
        leaf.set_channel_route(
            ch,
            ChannelRoute {
                origin: vec![],
                forward: [(a2, vec![ao])].into_iter().collect(),
            },
        );

        let (tx, mut rx) =
            tokio::sync::mpsc::unbounded_channel::<(&'static str, UserId, AurixPacket)>();
        for (name, relay) in [
            ("origin", &origin),
            ("hub1", &hub1),
            ("hub2", &hub2),
            ("leaf", &leaf),
        ] {
            let tx = tx.clone();
            relay
                .clone()
                .start_receiver(Arc::new(move |sender, packet| {
                    let _ = tx.send((name, sender, packet));
                }));
        }
        let sender = UserId::new();
        origin
            .forward_to_peers(&ch, &sender, &audio(&ch, 4242), Some(30))
            .await;

        let mut got = Vec::new();
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_millis(800);
        while let Ok(Some(item)) = tokio::time::timeout_at(deadline, rx.recv()).await {
            got.push(item);
            if got.len() >= 3 {
                // Give any stray (looped / duplicated) datagram a chance to show up.
                tokio::time::sleep(std::time::Duration::from_millis(150)).await;
                while let Ok(item) = rx.try_recv() {
                    got.push(item);
                }
                break;
            }
        }
        let mut names: Vec<&str> = got.iter().map(|g| g.0).collect();
        names.sort_unstable();
        assert_eq!(
            names,
            vec!["hub1", "hub2", "leaf"],
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
}
