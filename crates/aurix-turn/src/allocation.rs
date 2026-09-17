use chrono::{DateTime, Duration, Utc};
use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use tokio::net::UdpSocket;
use tokio::sync::Notify;
use uuid::Uuid;

/// RFC 8656 §9: permissions last 5 minutes and are refreshed by CreatePermission/ChannelBind.
pub const PERMISSION_LIFETIME_SECS: i64 = 300;
/// RFC 8656 §12: channel bindings last 10 minutes.
pub const CHANNEL_LIFETIME_SECS: i64 = 600;
pub const MAX_PERMISSIONS: usize = 64;
pub const MAX_CHANNELS: usize = 64;

/// Transport the client used to reach the TURN server.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ClientProtocol {
    Udp,
    Tcp,
}

/// Identifies one allocation: the client 5-tuple (server side is fixed).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ClientKey {
    pub addr: SocketAddr,
    pub proto: ClientProtocol,
}

#[derive(Debug)]
pub struct Allocation {
    pub id: Uuid,
    pub client: ClientKey,
    pub relay_addr: SocketAddr,
    pub relay_socket: Arc<UdpSocket>,
    pub username: String,
    pub realm: String,
    permissions: HashMap<IpAddr, DateTime<Utc>>,
    channel_bindings: Vec<ChannelBinding>,
    pub expires_at: DateTime<Utc>,
    pub created_at: DateTime<Utc>,
    /// Signalled when the allocation is removed so the relay task exits and frees the port.
    pub closer: Arc<Notify>,
    pub bytes_relayed_in: std::sync::atomic::AtomicU64,
    pub bytes_relayed_out: std::sync::atomic::AtomicU64,
}

impl Allocation {
    pub fn new(client: ClientKey, relay_addr: SocketAddr, relay_socket: Arc<UdpSocket>, username: String, realm: String, lifetime_secs: i64) -> Self {
        let now = Utc::now();
        Self {
            id: Uuid::now_v7(),
            client,
            relay_addr,
            relay_socket,
            username,
            realm,
            permissions: HashMap::new(),
            channel_bindings: Vec::new(),
            expires_at: now + Duration::seconds(lifetime_secs),
            created_at: now,
            closer: Arc::new(Notify::new()),
            bytes_relayed_in: std::sync::atomic::AtomicU64::new(0),
            bytes_relayed_out: std::sync::atomic::AtomicU64::new(0),
        }
    }

    pub fn is_expired(&self) -> bool {
        Utc::now() >= self.expires_at
    }

    pub fn refresh(&mut self, lifetime_secs: i64) {
        self.expires_at = Utc::now() + Duration::seconds(lifetime_secs);
    }

    /// Install or refresh a permission. Returns `false` when the permission table is full.
    pub fn add_permission(&mut self, ip: IpAddr) -> bool {
        self.prune();
        if !self.permissions.contains_key(&ip) && self.permissions.len() >= MAX_PERMISSIONS {
            return false;
        }
        self.permissions.insert(ip, Utc::now() + Duration::seconds(PERMISSION_LIFETIME_SECS));
        true
    }

    pub fn has_permission(&self, ip: &IpAddr) -> bool {
        self.permissions.get(ip).map(|exp| Utc::now() < *exp).unwrap_or(false)
    }

    /// Bind `number` to `peer`. Fails if the number or peer is already bound to something else
    /// (RFC 8656 §12.2) or the table is full.
    pub fn add_channel_binding(&mut self, number: u16, peer_addr: SocketAddr) -> std::result::Result<(), &'static str> {
        self.prune();
        for b in &self.channel_bindings {
            if b.number == number && b.peer_addr != peer_addr {
                return Err("channel number already bound to another peer");
            }
            if b.peer_addr == peer_addr && b.number != number {
                return Err("peer already bound to another channel");
            }
        }
        if let Some(existing) = self.channel_bindings.iter_mut().find(|b| b.number == number) {
            existing.expires_at = Utc::now() + Duration::seconds(CHANNEL_LIFETIME_SECS);
        } else {
            if self.channel_bindings.len() >= MAX_CHANNELS {
                return Err("channel table full");
            }
            self.channel_bindings.push(ChannelBinding { number, peer_addr, expires_at: Utc::now() + Duration::seconds(CHANNEL_LIFETIME_SECS) });
        }
        // A channel binding also installs a permission for the peer.
        self.permissions.insert(peer_addr.ip(), Utc::now() + Duration::seconds(PERMISSION_LIFETIME_SECS));
        Ok(())
    }

    pub fn get_channel_binding(&self, number: u16) -> Option<&ChannelBinding> {
        self.channel_bindings.iter().find(|b| b.number == number && !b.is_expired())
    }

    pub fn get_channel_for_peer(&self, peer: &SocketAddr) -> Option<u16> {
        self.channel_bindings.iter().find(|b| b.peer_addr == *peer && !b.is_expired()).map(|b| b.number)
    }

    fn prune(&mut self) {
        let now = Utc::now();
        self.permissions.retain(|_, exp| *exp > now);
        self.channel_bindings.retain(|b| b.expires_at > now);
    }
}

#[derive(Debug, Clone)]
pub struct ChannelBinding {
    pub number: u16,
    pub peer_addr: SocketAddr,
    pub expires_at: DateTime<Utc>,
}

impl ChannelBinding {
    pub fn is_expired(&self) -> bool {
        Utc::now() >= self.expires_at
    }
}
