use aurix_common::error::{AurixError, Result};
use aurix_common::protocol::{AurixPacket, PacketFlags, HEADER_SIZE};
use aurix_common::types::*;
use dashmap::DashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::net::UdpSocket;
use tracing::{error, info, warn};

/// Manages forwarding audio between SFU nodes for cross-region channels.
pub struct CascadeRelay {
    /// channel_id -> set of remote node addresses that have participants in that channel
    channel_peers: Arc<DashMap<ChannelId, Vec<SocketAddr>>>,
    socket: Arc<UdpSocket>,
    #[allow(dead_code)]
    local_node_id: MediaNodeId,
}

impl CascadeRelay {
    pub async fn new(bind_addr: &str, local_node_id: MediaNodeId) -> Result<Self> {
        let socket = UdpSocket::bind(bind_addr).await
            .map_err(|e| AurixError::Transport(format!("Cascade bind failed: {e}")))?;
        info!("Cascade relay listening on {}", bind_addr);
        Ok(Self {
            channel_peers: Arc::new(DashMap::new()),
            socket: Arc::new(socket),
            local_node_id,
        })
    }

    /// Register a remote node as having participants in a given channel.
    pub fn add_peer(&self, channel_id: ChannelId, peer_addr: SocketAddr) {
        self.channel_peers.entry(channel_id).or_default().push(peer_addr);
        // Deduplicate
        if let Some(mut entry) = self.channel_peers.get_mut(&channel_id) {
            entry.sort();
            entry.dedup();
        }
    }

    /// Remove a remote node from a channel.
    pub fn remove_peer(&self, channel_id: ChannelId, peer_addr: &SocketAddr) {
        if let Some(mut entry) = self.channel_peers.get_mut(&channel_id) {
            entry.retain(|a| a != peer_addr);
        }
    }

    /// Forward an audio packet to all remote nodes that have participants in this channel.
    /// Sets the Relay flag to prevent infinite forwarding loops.
    pub async fn forward_to_peers(&self, channel_id: &ChannelId, packet: &AurixPacket) {
        // Don't re-forward relayed packets
        if packet.header.has_flag(PacketFlags::Relay) { return; }

        if let Some(peers) = self.channel_peers.get(channel_id) {
            let mut relay_packet = packet.clone();
            relay_packet.header.set_flag(PacketFlags::Relay);
            let encoded = relay_packet.encode();
            let data = encoded.freeze();

            for peer_addr in peers.value().iter() {
                if let Err(e) = self.socket.send_to(&data, peer_addr).await {
                    warn!("Cascade forward to {} failed: {}", peer_addr, e);
                }
            }
        }
    }

    /// Start receiving relayed packets from other nodes and inject them into the local SFU router.
    pub fn start_receiver(
        self: Arc<Self>,
        router_fn: Arc<dyn Fn(&[u8], SocketAddr) + Send + Sync>,
    ) {
        let socket = self.socket.clone();
        tokio::spawn(async move {
            let mut buf = vec![0u8; 2048];
            loop {
                match socket.recv_from(&mut buf).await {
                    Ok((len, src)) => {
                        if len < HEADER_SIZE { continue; }
                        // The packet has the Relay flag set; the router will route locally
                        // without re-forwarding (because forward_to_peers checks the flag).
                        router_fn(&buf[..len], src);
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
        self.channel_peers.get(channel_id).map_or(false, |p| !p.is_empty())
    }
}