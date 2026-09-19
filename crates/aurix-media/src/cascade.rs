//! Node-to-node audio relay ("cascade") for channels spanning several SFU nodes.
//!
//! Every relayed datagram is a `Relay` envelope (see `AurixPacket::relay_envelope`) carrying the
//! original client packet, sealed (AES-256-CTR + HMAC) with keys derived from the cluster-wide
//! `media.cascade_secret`. The envelope's SSRC is a per-process random node id and its 64-bit
//! counter is spread over the sequence/timestamp fields, so IVs never repeat between nodes and
//! the per-peer anti-replay window sees one monotonic sequence. Packets are accepted only from
//! allowed peers (statically configured via `media.cascade_peers` and/or discovered from the
//! `media_nodes` registry), only with a valid tag and only once per (peer, sequence).
//!
//! Topology is per channel: a packet is forwarded only to the nodes that currently host
//! participants of that channel (see `set_channel_peers`, driven by `CascadeTopology`).

use aurix_common::crypto::MediaKeys;
use aurix_common::error::{AurixError, Result};
use aurix_common::protocol::{
    AurixPacket, PacketFlags, PacketType, ReplayWindow, HEADER_SIZE, MAX_RELAY_PACKET_SIZE,
};
use aurix_common::types::*;
use dashmap::DashMap;
use parking_lot::Mutex;
use std::collections::HashSet;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use tokio::net::UdpSocket;
use tracing::{debug, error, info, warn};

pub struct CascadeRelay {
    /// channel_id -> remote node addresses with participants in that channel
    channel_peers: Arc<DashMap<ChannelId, Vec<SocketAddr>>>,
    /// Peers we accept relayed traffic from, each with its own anti-replay window.
    allowed_peers: Arc<DashMap<SocketAddr, Mutex<ReplayWindow>>>,
    /// Peers from static configuration; never removed by discovery.
    static_peers: HashSet<SocketAddr>,
    /// Peers currently known through discovery (healthy nodes from the registry).
    dynamic_peers: Mutex<HashSet<SocketAddr>>,
    socket: Arc<UdpSocket>,
    local_addr: SocketAddr,
    keys: MediaKeys,
    relay_ssrc: u32,
    relay_counter: AtomicU64,
    local_node_id: MediaNodeId,
}

impl CascadeRelay {
    pub async fn new(
        bind_addr: &str,
        local_node_id: MediaNodeId,
        secret: &str,
        peers: &[String],
    ) -> Result<Self> {
        if secret.len() < 16 {
            return Err(AurixError::InvalidConfiguration(
                "media.cascade_secret must be at least 16 characters".into(),
            ));
        }
        let socket = UdpSocket::bind(bind_addr)
            .await
            .map_err(|e| AurixError::Transport(format!("Cascade bind failed: {e}")))?;
        let local_addr = socket
            .local_addr()
            .map_err(|e| AurixError::Transport(format!("Cascade local_addr failed: {e}")))?;
        let allowed_peers: Arc<DashMap<SocketAddr, Mutex<ReplayWindow>>> = Arc::new(DashMap::new());
        let mut static_peers = HashSet::new();
        for p in peers {
            let addr: SocketAddr = p.parse().map_err(|_| {
                AurixError::InvalidConfiguration(format!("Invalid cascade peer address: {p}"))
            })?;
            allowed_peers.insert(addr, Mutex::new(ReplayWindow::default()));
            static_peers.insert(addr);
        }
        info!(
            "Cascade relay listening on {} with {} static peers",
            local_addr,
            allowed_peers.len()
        );
        Ok(Self {
            channel_peers: Arc::new(DashMap::new()),
            allowed_peers,
            static_peers,
            dynamic_peers: Mutex::new(HashSet::new()),
            socket: Arc::new(socket),
            local_addr,
            keys: MediaKeys::derive(secret.as_bytes()),
            relay_ssrc: rand::random(),
            relay_counter: AtomicU64::new(0),
            local_node_id,
        })
    }

    pub fn node_id(&self) -> MediaNodeId {
        self.local_node_id
    }

    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    pub fn allowed_peers(&self) -> Vec<SocketAddr> {
        self.allowed_peers.iter().map(|e| *e.key()).collect()
    }

    pub fn static_peers(&self) -> Vec<SocketAddr> {
        self.static_peers.iter().copied().collect()
    }

    /// Replace the set of discovered peers. Newly seen addresses become accepted sources
    /// (fresh replay window); addresses that disappeared and are not statically configured
    /// are dropped from the allow-list and from every channel's forwarding list.
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
            for mut entry in self.channel_peers.iter_mut() {
                entry.value_mut().retain(|a| a != removed);
            }
            info!("Cascade peer gone: {}", removed);
        }
        *current = peers;
    }

    /// Register a remote node as having participants in a given channel. Unknown peers are refused.
    pub fn add_peer(&self, channel_id: ChannelId, peer_addr: SocketAddr) -> Result<()> {
        if !self.allowed_peers.contains_key(&peer_addr) {
            return Err(AurixError::AuthorizationDenied(format!(
                "{peer_addr} is not an allowed cascade peer"
            )));
        }
        let mut entry = self.channel_peers.entry(channel_id).or_default();
        if !entry.contains(&peer_addr) {
            entry.push(peer_addr);
        }
        Ok(())
    }

    /// Replace the forwarding list for a channel with exactly the allowed peers in `peers`.
    /// Used by discovery; an empty list removes the channel from the relay entirely.
    pub fn set_channel_peers(&self, channel_id: ChannelId, peers: &[SocketAddr]) {
        let filtered: Vec<SocketAddr> = peers
            .iter()
            .copied()
            .filter(|p| self.allowed_peers.contains_key(p))
            .collect();
        if filtered.is_empty() {
            self.channel_peers.remove(&channel_id);
        } else {
            self.channel_peers.insert(channel_id, filtered);
        }
    }

    /// Subscribe every statically configured peer to `channel_id` (legacy full-mesh mode,
    /// used when no `media_nodes` registry is available).
    pub fn add_all_peers(&self, channel_id: ChannelId) {
        for peer in self.static_peers() {
            let _ = self.add_peer(channel_id, peer);
        }
    }

    pub fn remove_peer(&self, channel_id: ChannelId, peer_addr: &SocketAddr) {
        if let Some(mut entry) = self.channel_peers.get_mut(&channel_id) {
            entry.retain(|a| a != peer_addr);
        }
    }

    pub fn remove_channel(&self, channel_id: &ChannelId) {
        self.channel_peers.remove(channel_id);
    }

    pub fn channel_peers(&self, channel_id: &ChannelId) -> Vec<SocketAddr> {
        self.channel_peers
            .get(channel_id)
            .map(|p| p.value().clone())
            .unwrap_or_default()
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
        let peers: Vec<SocketAddr> = match self.channel_peers.get(channel_id) {
            Some(p) if !p.is_empty() => p.value().clone(),
            _ => return,
        };
        let labelled = level.map(|l| packet.with_audio_level(l));
        let packet = labelled.as_ref().unwrap_or(packet);
        let counter = self.relay_counter.fetch_add(1, Ordering::Relaxed);
        let data = AurixPacket::relay_envelope(packet, self.relay_ssrc, counter, sender)
            .seal(&self.keys)
            .freeze();
        for peer_addr in peers {
            if let Err(e) = self.socket.send_to(&data, peer_addr).await {
                warn!("Cascade forward to {} failed: {}", peer_addr, e);
            }
        }
    }

    /// Validate an inbound relayed datagram (known peer, tag, Relay envelope, replay window)
    /// and return the sending user plus the plaintext client packet it carries.
    pub fn authenticate_inbound(
        &self,
        data: &[u8],
        src: SocketAddr,
    ) -> Result<(UserId, AurixPacket)> {
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
        let (sender, inner) = envelope.relay_inner()?;
        if inner.header.packet_type != PacketType::Audio
            && inner.header.packet_type != PacketType::AudioFec
        {
            return Err(AurixError::Transport(
                "Relay envelope must carry an audio packet".into(),
            ));
        }
        Ok((sender, inner))
    }

    /// Receive relayed packets from peers and hand authenticated ones to `on_packet`.
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
                            Ok((sender, packet)) => on_packet(sender, packet),
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
        self.channel_peers
            .get(channel_id)
            .is_some_and(|p| !p.is_empty())
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
            "127.0.0.1:0",
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
        let (from, inner) = relay.authenticate_inbound(&good, peer).unwrap();
        assert_eq!(from, sender);
        assert_eq!(inner.header.ssrc, 42);
        assert_eq!(&inner.payload[..], b"opus");
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
            "127.0.0.1:0",
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
}
