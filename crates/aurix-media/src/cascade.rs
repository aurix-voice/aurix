//! Node-to-node audio relay ("cascade") for channels spanning several SFU nodes.
//!
//! Every relayed packet is an AURX packet with the `Relay` flag, authenticated with the
//! cluster-wide `media.cascade_secret` (HMAC). Packets are accepted only from configured
//! peers, only with a valid tag and only once per (peer, sequence) within the replay window.

use aurix_common::error::{AurixError, Result};
use aurix_common::protocol::{AurixPacket, PacketFlags, ReplayWindow, HEADER_SIZE};
use aurix_common::types::*;
use dashmap::DashMap;
use parking_lot::Mutex;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::net::UdpSocket;
use tracing::{debug, error, info, warn};

pub struct CascadeRelay {
    /// channel_id -> remote node addresses with participants in that channel
    channel_peers: Arc<DashMap<ChannelId, Vec<SocketAddr>>>,
    /// Peers we accept relayed traffic from, each with its own anti-replay window.
    allowed_peers: Arc<DashMap<SocketAddr, Mutex<ReplayWindow>>>,
    socket: Arc<UdpSocket>,
    secret: Vec<u8>,
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
        let allowed_peers: Arc<DashMap<SocketAddr, Mutex<ReplayWindow>>> = Arc::new(DashMap::new());
        for p in peers {
            let addr: SocketAddr = p.parse().map_err(|_| {
                AurixError::InvalidConfiguration(format!("Invalid cascade peer address: {p}"))
            })?;
            allowed_peers.insert(addr, Mutex::new(ReplayWindow::default()));
        }
        info!(
            "Cascade relay listening on {} with {} peers",
            bind_addr,
            allowed_peers.len()
        );
        Ok(Self {
            channel_peers: Arc::new(DashMap::new()),
            allowed_peers,
            socket: Arc::new(socket),
            secret: secret.as_bytes().to_vec(),
            local_node_id,
        })
    }

    pub fn node_id(&self) -> MediaNodeId {
        self.local_node_id
    }

    pub fn allowed_peers(&self) -> Vec<SocketAddr> {
        self.allowed_peers.iter().map(|e| *e.key()).collect()
    }

    /// Register a remote node as having participants in a given channel. Unknown peers are refused.
    pub fn add_peer(&self, channel_id: ChannelId, peer_addr: SocketAddr) -> Result<()> {
        if !self.allowed_peers.contains_key(&peer_addr) {
            return Err(AurixError::AuthorizationDenied(format!(
                "{peer_addr} is not a configured cascade peer"
            )));
        }
        let mut entry = self.channel_peers.entry(channel_id).or_default();
        if !entry.contains(&peer_addr) {
            entry.push(peer_addr);
        }
        Ok(())
    }

    /// Subscribe every configured peer to `channel_id` (static full-mesh topology).
    pub fn add_all_peers(&self, channel_id: ChannelId) {
        for peer in self.allowed_peers() {
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

    /// Forward a locally originated audio packet to all peers for this channel.
    pub async fn forward_to_peers(&self, channel_id: &ChannelId, packet: &AurixPacket) {
        if packet.header.has_flag(PacketFlags::Relay) {
            return;
        }
        let peers: Vec<SocketAddr> = match self.channel_peers.get(channel_id) {
            Some(p) if !p.is_empty() => p.value().clone(),
            _ => return,
        };
        let mut relay_packet = packet.clone();
        relay_packet.header.set_flag(PacketFlags::Relay);
        let data = relay_packet.encode_authenticated(&self.secret).freeze();
        for peer_addr in peers {
            if let Err(e) = self.socket.send_to(&data, peer_addr).await {
                warn!("Cascade forward to {} failed: {}", peer_addr, e);
            }
        }
    }

    /// Validate an inbound relayed datagram: known peer, HMAC, Relay flag, replay window.
    pub fn authenticate_inbound(&self, data: &[u8], src: SocketAddr) -> Result<AurixPacket> {
        let window = self.allowed_peers.get(&src).ok_or_else(|| {
            AurixError::AuthorizationDenied(format!("Cascade packet from unknown peer {src}"))
        })?;
        let packet = AurixPacket::decode(data)?;
        if !packet.header.has_flag(PacketFlags::Relay) {
            return Err(AurixError::Transport(
                "Cascade packet without Relay flag".into(),
            ));
        }
        if !packet.is_authenticated() || !packet.verify_auth(&self.secret) {
            return Err(AurixError::AuthenticationFailed(
                "Cascade packet authentication failed".into(),
            ));
        }
        // Sequence space is per (peer, ssrc); fold the ssrc in so multiple remote users don't collide.
        let seq = packet.header.sequence ^ packet.header.ssrc.rotate_left(16);
        if !window.lock().check_and_update(seq) {
            return Err(AurixError::AuthenticationFailed(
                "Replayed cascade packet".into(),
            ));
        }
        Ok(packet)
    }

    /// Receive relayed packets from peers and hand authenticated ones to `on_packet`.
    pub fn start_receiver(self: Arc<Self>, on_packet: Arc<dyn Fn(AurixPacket) + Send + Sync>) {
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
                            Ok(packet) => on_packet(packet),
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

        let mut pkt = AurixPacket::audio(1, 0, 42, 7, Bytes::from_static(b"opus"));
        pkt.header.set_flag(PacketFlags::Relay);
        let good = pkt.encode_authenticated(b"0123456789abcdef");
        let bad = pkt.encode_authenticated(b"wrong-secret-wrong-secret");
        let unauth = pkt.encode();

        assert!(relay
            .authenticate_inbound(&good, "127.0.0.1:45002".parse().unwrap())
            .is_err());
        assert!(relay.authenticate_inbound(&bad, peer).is_err());
        assert!(relay.authenticate_inbound(&unauth, peer).is_err());
        assert!(relay.authenticate_inbound(&good, peer).is_ok());
        assert!(
            relay.authenticate_inbound(&good, peer).is_err(),
            "replay must be rejected"
        );
        assert!(relay
            .add_peer(ChannelId::new(), "127.0.0.1:45002".parse().unwrap())
            .is_err());
    }
}
