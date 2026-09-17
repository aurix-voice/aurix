use std::net::SocketAddr;
use std::collections::HashSet;
use chrono::{DateTime, Utc, Duration};
use uuid::Uuid;
use tokio::net::UdpSocket;
use std::sync::Arc;

#[derive(Debug)]
pub struct Allocation {
    pub id: Uuid,
    pub client_addr: SocketAddr,
    pub relay_addr: SocketAddr,
    pub relay_socket: Arc<UdpSocket>,
    pub username: String,
    pub realm: String,
    pub permissions: HashSet<std::net::IpAddr>,
    pub channel_bindings: Vec<ChannelBinding>,
    pub expires_at: DateTime<Utc>,
    pub created_at: DateTime<Utc>,
}

impl Allocation {
    pub fn new(
        client_addr: SocketAddr,
        relay_addr: SocketAddr,
        relay_socket: Arc<UdpSocket>,
        username: String,
        realm: String,
        lifetime_secs: i64,
    ) -> Self {
        Self {
            id: Uuid::now_v7(),
            client_addr,
            relay_addr,
            relay_socket,
            username,
            realm,
            permissions: HashSet::new(),
            channel_bindings: Vec::new(),
            expires_at: Utc::now() + Duration::seconds(lifetime_secs),
            created_at: Utc::now(),
        }
    }

    pub fn is_expired(&self) -> bool {
        Utc::now() >= self.expires_at
    }

    pub fn refresh(&mut self, lifetime_secs: i64) {
        self.expires_at = Utc::now() + Duration::seconds(lifetime_secs);
    }

    pub fn add_permission(&mut self, ip: std::net::IpAddr) {
        self.permissions.insert(ip);
    }

    pub fn has_permission(&self, ip: &std::net::IpAddr) -> bool {
        self.permissions.contains(ip)
    }

    pub fn add_channel_binding(&mut self, number: u16, peer_addr: SocketAddr) {
        self.channel_bindings.retain(|b| b.number != number);
        self.channel_bindings.push(ChannelBinding {
            number,
            peer_addr,
            expires_at: Utc::now() + Duration::seconds(600),
        });
    }

    pub fn get_channel_binding(&self, number: u16) -> Option<&ChannelBinding> {
        self.channel_bindings.iter().find(|b| b.number == number && !b.is_expired())
    }

    pub fn get_channel_for_peer(&self, peer: &SocketAddr) -> Option<u16> {
        self.channel_bindings
            .iter()
            .find(|b| b.peer_addr == *peer && !b.is_expired())
            .map(|b| b.number)
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