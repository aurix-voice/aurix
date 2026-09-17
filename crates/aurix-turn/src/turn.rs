//! TURN request handling (RFC 8656) with long-term credentials (RFC 8489 §9.2) using the
//! coturn-compatible time-limited username/password scheme.

use crate::allocation::{Allocation, ClientKey, ClientProtocol};
use crate::stun::*;
use aurix_common::crypto::{constant_time_eq, hmac_sha256, parse_turn_username, stun_long_term_key, turn_password_for_username};
use aurix_common::error::Result;
use base64::Engine;
use bytes::{BufMut, BytesMut};
use dashmap::DashMap;
use std::collections::HashSet;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::atomic::{AtomicU16, Ordering};
use std::sync::Arc;
use tokio::net::UdpSocket;
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

const SOFTWARE: &str = "Aurix TURN/1.0";
const NONCE_LIFETIME_SECS: i64 = 3600;
const DEFAULT_LIFETIME_SECS: u32 = 600;
const MAX_LIFETIME_SECS: u32 = 3600;
const MAX_ALLOCATIONS_PER_USER: usize = 16;
const MAX_RELAY_PAYLOAD: usize = 1500;

/// How a response reaches the client: directly via the UDP server socket, or through the
/// per-connection writer task for TCP clients.
#[derive(Clone)]
pub enum ClientSink {
    Udp(Arc<UdpSocket>),
    Tcp(mpsc::Sender<Vec<u8>>),
}

impl ClientSink {
    pub async fn send(&self, data: &[u8], addr: SocketAddr) {
        match self {
            ClientSink::Udp(sock) => {
                let _ = sock.send_to(data, addr).await;
            }
            ClientSink::Tcp(tx) => {
                let _ = tx.send(data.to_vec()).await;
            }
        }
    }

    fn proto(&self) -> ClientProtocol {
        match self {
            ClientSink::Udp(_) => ClientProtocol::Udp,
            ClientSink::Tcp(_) => ClientProtocol::Tcp,
        }
    }
}

struct Authenticated {
    username: String,
    key: [u8; 16],
}

enum AuthFailure {
    /// 401 with fresh REALM/NONCE.
    Unauthorized,
    /// 438 Stale Nonce.
    StaleNonce,
    /// 400 Bad Request (malformed credentials).
    BadRequest,
}

pub struct TurnHandler {
    realm: String,
    auth_secret: String,
    allocations: Arc<DashMap<ClientKey, Allocation>>,
    ports_in_use: Arc<parking_lot::Mutex<HashSet<u16>>>,
    min_port: u16,
    max_port: u16,
    next_port: AtomicU16,
    max_allocations: u32,
    allocation_lifetime: i64,
    external_ip: Option<IpAddr>,
    relay_bind_ip: IpAddr,
}

impl TurnHandler {
    pub fn new(realm: String, auth_secret: String, min_port: u16, max_port: u16, max_allocations: u32, allocation_lifetime: i64, external_ip: Option<IpAddr>) -> Self {
        Self {
            realm,
            auth_secret,
            allocations: Arc::new(DashMap::new()),
            ports_in_use: Arc::new(parking_lot::Mutex::new(HashSet::new())),
            min_port,
            max_port,
            next_port: AtomicU16::new(min_port),
            max_allocations,
            allocation_lifetime: allocation_lifetime.clamp(60, MAX_LIFETIME_SECS as i64),
            external_ip,
            relay_bind_ip: IpAddr::V4(Ipv4Addr::UNSPECIFIED),
        }
    }

    /// IP the relay sockets bind to (defaults to 0.0.0.0; tests use loopback).
    pub fn set_relay_bind_ip(&mut self, ip: IpAddr) {
        self.relay_bind_ip = ip;
    }

    pub fn allocations(&self) -> &Arc<DashMap<ClientKey, Allocation>> {
        &self.allocations
    }

    pub fn allocation_count(&self) -> usize {
        self.allocations.len()
    }

    pub async fn handle_stun_message(&self, msg: &StunMessage, src: SocketAddr, sink: &ClientSink, raw: &[u8]) -> Result<()> {
        if !StunMessage::verify_fingerprint(raw) {
            debug!("Dropping STUN message with bad FINGERPRINT from {}", src);
            return Ok(());
        }
        let client = ClientKey { addr: src, proto: sink.proto() };
        match msg.msg_type {
            StunMessageType::BindingRequest => self.handle_binding_request(msg, src, sink).await,
            StunMessageType::AllocateRequest => self.handle_allocate_request(msg, client, sink, raw).await,
            StunMessageType::RefreshRequest => self.handle_refresh_request(msg, client, sink, raw).await,
            StunMessageType::CreatePermissionRequest => self.handle_create_permission(msg, client, sink, raw).await,
            StunMessageType::ChannelBindRequest => self.handle_channel_bind(msg, client, sink, raw).await,
            StunMessageType::SendIndication => self.handle_send_indication(msg, client).await,
            other => {
                debug!("Unhandled STUN type {:?} from {}", other, src);
                Ok(())
            }
        }
    }

    /// Client-to-server ChannelData (RFC 8656 §12.6).
    pub async fn handle_channel_data(&self, data: &[u8], src: SocketAddr, proto: ClientProtocol) {
        if data.len() < 4 {
            return;
        }
        let number = u16::from_be_bytes([data[0], data[1]]);
        let len = u16::from_be_bytes([data[2], data[3]]) as usize;
        if data.len() < 4 + len || !(0x4000..=0x4FFF).contains(&number) {
            return;
        }
        let payload = &data[4..4 + len];
        let target = {
            let Some(alloc) = self.allocations.get(&ClientKey { addr: src, proto }) else { return };
            let Some(binding) = alloc.get_channel_binding(number) else { return };
            (alloc.relay_socket.clone(), binding.peer_addr, alloc.bytes_relayed_out.fetch_add(len as u64, Ordering::Relaxed))
        };
        let _ = target.0.send_to(payload, target.1).await;
    }

    async fn handle_binding_request(&self, msg: &StunMessage, src: SocketAddr, sink: &ClientSink) -> Result<()> {
        let mut response = StunMessage::new(StunMessageType::BindingResponse, msg.transaction_id);
        response.add_xor_mapped_address(src);
        response.add_software(SOFTWARE);
        sink.send(&response.encode(), src).await;
        aurix_metrics::STUN_REQUESTS.inc();
        Ok(())
    }

    // ── Authentication ──

    fn nonce(&self, now_unix: i64) -> String {
        let bucket = now_unix / NONCE_LIFETIME_SECS;
        let tag = hmac_sha256(self.auth_secret.as_bytes(), &[b"turn-nonce", &bucket.to_be_bytes()]);
        format!("{:x}.{}", bucket, base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(&tag[..12]))
    }

    fn nonce_is_valid(&self, nonce: &str, now_unix: i64) -> bool {
        let Some((bucket_hex, _)) = nonce.split_once('.') else { return false };
        let Ok(bucket) = i64::from_str_radix(bucket_hex, 16) else { return false };
        let current = now_unix / NONCE_LIFETIME_SECS;
        // Accept the current and the previous window so a nonce issued just before the
        // boundary is still usable.
        if bucket != current && bucket != current - 1 {
            return false;
        }
        constant_time_eq(self.nonce(bucket * NONCE_LIFETIME_SECS).as_bytes(), nonce.as_bytes())
    }

    fn authenticate(&self, msg: &StunMessage, raw: &[u8]) -> std::result::Result<Authenticated, AuthFailure> {
        let now = chrono::Utc::now().timestamp();
        if msg.get_attribute(StunAttributeType::MessageIntegrity).is_none() {
            return Err(AuthFailure::Unauthorized);
        }
        let username = msg.get_string(StunAttributeType::Username).ok_or(AuthFailure::BadRequest)?;
        let realm = msg.get_string(StunAttributeType::Realm).ok_or(AuthFailure::BadRequest)?;
        let nonce = msg.get_string(StunAttributeType::Nonce).ok_or(AuthFailure::BadRequest)?;
        if username.len() > 512 || realm != self.realm {
            return Err(AuthFailure::Unauthorized);
        }
        if !self.nonce_is_valid(&nonce, now) {
            return Err(AuthFailure::StaleNonce);
        }
        let (expiry, user) = parse_turn_username(&username).ok_or(AuthFailure::Unauthorized)?;
        if user.is_empty() || expiry < now {
            return Err(AuthFailure::Unauthorized);
        }
        let password = turn_password_for_username(&self.auth_secret, &username);
        let key = stun_long_term_key(&username, &self.realm, &password);
        if !StunMessage::verify_integrity(raw, &key) {
            return Err(AuthFailure::Unauthorized);
        }
        Ok(Authenticated { username, key })
    }

    async fn send_auth_error(&self, msg: &StunMessage, client: ClientKey, sink: &ClientSink, failure: AuthFailure) {
        let mut err = StunMessage::new(msg.msg_type.error_response(), msg.transaction_id);
        match failure {
            AuthFailure::Unauthorized => {
                err.add_error_code(401, "Unauthorized");
                err.add_attribute(StunAttributeType::Realm, self.realm.as_bytes().to_vec());
                err.add_attribute(StunAttributeType::Nonce, self.nonce(chrono::Utc::now().timestamp()).as_bytes().to_vec());
            }
            AuthFailure::StaleNonce => {
                err.add_error_code(438, "Stale Nonce");
                err.add_attribute(StunAttributeType::Realm, self.realm.as_bytes().to_vec());
                err.add_attribute(StunAttributeType::Nonce, self.nonce(chrono::Utc::now().timestamp()).as_bytes().to_vec());
            }
            AuthFailure::BadRequest => err.add_error_code(400, "Bad Request"),
        }
        err.add_software(SOFTWARE);
        sink.send(&err.encode(), client.addr).await;
    }

    async fn send_error(&self, msg: &StunMessage, client: ClientKey, sink: &ClientSink, key: &[u8], code: u16, reason: &str) {
        let mut err = StunMessage::new(msg.msg_type.error_response(), msg.transaction_id);
        err.add_error_code(code, reason);
        err.add_software(SOFTWARE);
        sink.send(&err.encode_with_integrity(key), client.addr).await;
    }

    // ── Allocate ──

    async fn handle_allocate_request(&self, msg: &StunMessage, client: ClientKey, sink: &ClientSink, raw: &[u8]) -> Result<()> {
        let auth = match self.authenticate(msg, raw) {
            Ok(a) => a,
            Err(f) => {
                self.send_auth_error(msg, client, sink, f).await;
                return Ok(());
            }
        };

        if self.allocations.contains_key(&client) {
            self.send_error(msg, client, sink, &auth.key, 437, "Allocation Mismatch").await;
            return Ok(());
        }
        // REQUESTED-TRANSPORT is mandatory and only UDP (17) is relayed.
        match msg.get_attribute(StunAttributeType::RequestedTransport).map(|a| a.value.first().copied()) {
            Some(Some(17)) => {}
            Some(_) => {
                self.send_error(msg, client, sink, &auth.key, 442, "Unsupported Transport Protocol").await;
                return Ok(());
            }
            None => {
                self.send_error(msg, client, sink, &auth.key, 400, "Bad Request").await;
                return Ok(());
            }
        }
        if msg.get_attribute(StunAttributeType::ReservationToken).is_some() || msg.get_attribute(StunAttributeType::EvenPort).is_some() {
            self.send_error(msg, client, sink, &auth.key, 508, "Insufficient Capacity").await;
            return Ok(());
        }
        let family = msg.get_attribute(StunAttributeType::RequestedAddressFamily).and_then(|a| a.value.first().copied()).unwrap_or(0x01);
        let relay_ip_family_v6 = match family {
            0x01 => false,
            0x02 => true,
            _ => {
                self.send_error(msg, client, sink, &auth.key, 440, "Address Family not Supported").await;
                return Ok(());
            }
        };
        if relay_ip_family_v6 && !self.relay_bind_ip.is_ipv6() && self.external_ip.map(|ip| !ip.is_ipv6()).unwrap_or(true) {
            self.send_error(msg, client, sink, &auth.key, 440, "Address Family not Supported").await;
            return Ok(());
        }

        if self.allocations.len() as u32 >= self.max_allocations {
            self.send_error(msg, client, sink, &auth.key, 508, "Insufficient Capacity").await;
            return Ok(());
        }
        let per_user = self.allocations.iter().filter(|a| a.username == auth.username).count();
        if per_user >= MAX_ALLOCATIONS_PER_USER {
            self.send_error(msg, client, sink, &auth.key, 486, "Allocation Quota Reached").await;
            return Ok(());
        }

        let requested = msg.get_u32(StunAttributeType::Lifetime).unwrap_or(self.allocation_lifetime as u32);
        let lifetime = requested.clamp(DEFAULT_LIFETIME_SECS.min(self.allocation_lifetime as u32), MAX_LIFETIME_SECS).max(1);

        let Some((relay_socket, relay_port)) = self.bind_relay_socket().await else {
            self.send_error(msg, client, sink, &auth.key, 508, "Insufficient Capacity").await;
            return Ok(());
        };
        let relay_ip = self.external_ip.unwrap_or(match self.relay_bind_ip {
            IpAddr::V4(ip) if ip.is_unspecified() => match sink {
                ClientSink::Udp(s) => s.local_addr().map(|a| a.ip()).unwrap_or(IpAddr::V4(Ipv4Addr::UNSPECIFIED)),
                ClientSink::Tcp(_) => IpAddr::V4(Ipv4Addr::UNSPECIFIED),
            },
            ip => ip,
        });
        let relay_addr = SocketAddr::new(relay_ip, relay_port);

        let allocation = Allocation::new(client, relay_addr, relay_socket.clone(), auth.username.clone(), self.realm.clone(), lifetime as i64);
        let closer = allocation.closer.clone();
        self.spawn_relay_task(client, relay_socket, relay_port, closer, sink.clone());
        self.allocations.insert(client, allocation);
        aurix_metrics::TURN_ALLOCATIONS.set(self.allocations.len() as i64);

        let mut response = StunMessage::new(StunMessageType::AllocateResponse, msg.transaction_id);
        response.add_xor_address(StunAttributeType::XorRelayedAddress, relay_addr);
        response.add_attribute(StunAttributeType::Lifetime, lifetime.to_be_bytes().to_vec());
        response.add_xor_mapped_address(client.addr);
        response.add_software(SOFTWARE);
        sink.send(&response.encode_with_integrity(&auth.key), client.addr).await;
        info!("TURN allocation {} -> relay {} (user {}, {}s)", client.addr, relay_addr, auth.username, lifetime);
        Ok(())
    }

    /// Reads peer data from the relay socket and delivers it to the client as ChannelData or a
    /// Data indication until the allocation is closed.
    fn spawn_relay_task(&self, client: ClientKey, relay_socket: Arc<UdpSocket>, relay_port: u16, closer: Arc<tokio::sync::Notify>, sink: ClientSink) {
        let allocations = self.allocations.clone();
        let ports = self.ports_in_use.clone();
        tokio::spawn(async move {
            let mut buf = vec![0u8; 2048];
            loop {
                let recv = tokio::select! {
                    _ = closer.notified() => break,
                    r = relay_socket.recv_from(&mut buf) => r,
                };
                let (len, peer) = match recv {
                    Ok(v) => v,
                    Err(e) => {
                        warn!("Relay socket error on port {}: {}", relay_port, e);
                        break;
                    }
                };
                if len > MAX_RELAY_PAYLOAD {
                    continue;
                }
                let out = {
                    let Some(alloc) = allocations.get(&client) else { break };
                    if alloc.is_expired() || !alloc.has_permission(&peer.ip()) {
                        continue;
                    }
                    alloc.bytes_relayed_in.fetch_add(len as u64, Ordering::Relaxed);
                    match alloc.get_channel_for_peer(&peer) {
                        Some(ch) => {
                            let mut cd = BytesMut::with_capacity(4 + len + 3);
                            cd.put_u16(ch);
                            cd.put_u16(len as u16);
                            cd.put_slice(&buf[..len]);
                            if client.proto == ClientProtocol::Tcp {
                                for _ in 0..((4 - (len % 4)) % 4) {
                                    cd.put_u8(0);
                                }
                            }
                            cd
                        }
                        None => {
                            let mut txid = [0u8; 12];
                            rand::Rng::fill(&mut rand::thread_rng(), &mut txid);
                            let mut ind = StunMessage::new(StunMessageType::DataIndication, txid);
                            ind.add_xor_address(StunAttributeType::XorPeerAddress, peer);
                            ind.add_attribute(StunAttributeType::Data, buf[..len].to_vec());
                            ind.encode()
                        }
                    }
                };
                sink.send(&out, client.addr).await;
            }
            ports.lock().remove(&relay_port);
            debug!("Relay task for {} on port {} exited", client.addr, relay_port);
        });
    }

    async fn bind_relay_socket(&self) -> Option<(Arc<UdpSocket>, u16)> {
        let span = (self.max_port - self.min_port) as usize + 1;
        for _ in 0..span.min(256) {
            let port = {
                let mut p = self.next_port.fetch_add(1, Ordering::Relaxed);
                if p > self.max_port || p < self.min_port {
                    self.next_port.store(self.min_port.wrapping_add(1), Ordering::Relaxed);
                    p = self.min_port;
                }
                p
            };
            if !self.ports_in_use.lock().insert(port) {
                continue;
            }
            match UdpSocket::bind(SocketAddr::new(self.relay_bind_ip, port)).await {
                Ok(sock) => return Some((Arc::new(sock), port)),
                Err(_) => {
                    self.ports_in_use.lock().remove(&port);
                }
            }
        }
        None
    }

    // ── Refresh ──

    async fn handle_refresh_request(&self, msg: &StunMessage, client: ClientKey, sink: &ClientSink, raw: &[u8]) -> Result<()> {
        let auth = match self.authenticate(msg, raw) {
            Ok(a) => a,
            Err(f) => {
                self.send_auth_error(msg, client, sink, f).await;
                return Ok(());
            }
        };
        let requested = msg.get_u32(StunAttributeType::Lifetime);
        let lifetime = match requested {
            Some(0) => 0,
            Some(l) => l.clamp(DEFAULT_LIFETIME_SECS.min(self.allocation_lifetime as u32), MAX_LIFETIME_SECS),
            None => self.allocation_lifetime as u32,
        };

        let outcome = match self.allocations.get_mut(&client) {
            None => Err((437, "Allocation Mismatch")),
            Some(alloc) if alloc.username != auth.username => Err((441, "Wrong Credentials")),
            Some(mut alloc) => {
                if lifetime > 0 {
                    alloc.refresh(lifetime as i64);
                }
                Ok(())
            }
        };
        if let Err((code, reason)) = outcome {
            self.send_error(msg, client, sink, &auth.key, code, reason).await;
            return Ok(());
        }
        if lifetime == 0 {
            self.remove_allocation(&client);
            info!("TURN allocation {} released by client", client.addr);
        }
        let mut resp = StunMessage::new(StunMessageType::RefreshResponse, msg.transaction_id);
        resp.add_attribute(StunAttributeType::Lifetime, lifetime.to_be_bytes().to_vec());
        resp.add_software(SOFTWARE);
        sink.send(&resp.encode_with_integrity(&auth.key), client.addr).await;
        Ok(())
    }

    pub fn remove_allocation(&self, client: &ClientKey) -> bool {
        let removed = self.allocations.remove(client);
        if let Some((_, alloc)) = removed {
            alloc.closer.notify_one();
            aurix_metrics::TURN_ALLOCATIONS.set(self.allocations.len() as i64);
            true
        } else {
            false
        }
    }

    /// Drop every allocation owned by a TCP connection that went away.
    pub fn remove_client(&self, addr: SocketAddr, proto: ClientProtocol) {
        self.remove_allocation(&ClientKey { addr, proto });
    }

    // ── CreatePermission ──

    async fn handle_create_permission(&self, msg: &StunMessage, client: ClientKey, sink: &ClientSink, raw: &[u8]) -> Result<()> {
        let auth = match self.authenticate(msg, raw) {
            Ok(a) => a,
            Err(f) => {
                self.send_auth_error(msg, client, sink, f).await;
                return Ok(());
            }
        };
        let peers = msg.get_all_xor_addresses(StunAttributeType::XorPeerAddress);
        if peers.is_empty() {
            self.send_error(msg, client, sink, &auth.key, 400, "Bad Request").await;
            return Ok(());
        }
        let outcome = match self.allocations.get_mut(&client) {
            None => Err((437, "Allocation Mismatch")),
            Some(alloc) if alloc.username != auth.username => Err((441, "Wrong Credentials")),
            Some(mut alloc) => {
                if peers.iter().any(|p| p.is_ipv4() != alloc.relay_addr.is_ipv4()) {
                    Err((443, "Peer Address Family Mismatch"))
                } else if peers.iter().all(|p| alloc.add_permission(p.ip())) {
                    Ok(())
                } else {
                    Err((508, "Insufficient Capacity"))
                }
            }
        };
        match outcome {
            Ok(()) => {
                let mut resp = StunMessage::new(StunMessageType::CreatePermissionResponse, msg.transaction_id);
                resp.add_software(SOFTWARE);
                sink.send(&resp.encode_with_integrity(&auth.key), client.addr).await;
            }
            Err((code, reason)) => self.send_error(msg, client, sink, &auth.key, code, reason).await,
        }
        Ok(())
    }

    // ── ChannelBind ──

    async fn handle_channel_bind(&self, msg: &StunMessage, client: ClientKey, sink: &ClientSink, raw: &[u8]) -> Result<()> {
        let auth = match self.authenticate(msg, raw) {
            Ok(a) => a,
            Err(f) => {
                self.send_auth_error(msg, client, sink, f).await;
                return Ok(());
            }
        };
        let number = msg.get_attribute(StunAttributeType::ChannelNumber).and_then(|a| a.value.get(..2)).map(|v| u16::from_be_bytes([v[0], v[1]]));
        let peer = msg.get_xor_address(StunAttributeType::XorPeerAddress);
        let (Some(number), Some(peer)) = (number, peer) else {
            self.send_error(msg, client, sink, &auth.key, 400, "Bad Request").await;
            return Ok(());
        };
        if !(0x4000..=0x4FFF).contains(&number) {
            self.send_error(msg, client, sink, &auth.key, 400, "Bad Request").await;
            return Ok(());
        }
        let outcome = match self.allocations.get_mut(&client) {
            None => Err((437, "Allocation Mismatch")),
            Some(alloc) if alloc.username != auth.username => Err((441, "Wrong Credentials")),
            Some(mut alloc) => {
                if peer.is_ipv4() != alloc.relay_addr.is_ipv4() {
                    Err((443, "Peer Address Family Mismatch"))
                } else {
                    alloc.add_channel_binding(number, peer).map_err(|_| (400, "Bad Request"))
                }
            }
        };
        match outcome {
            Ok(()) => {
                let mut resp = StunMessage::new(StunMessageType::ChannelBindResponse, msg.transaction_id);
                resp.add_software(SOFTWARE);
                sink.send(&resp.encode_with_integrity(&auth.key), client.addr).await;
            }
            Err((code, reason)) => self.send_error(msg, client, sink, &auth.key, code, reason).await,
        }
        Ok(())
    }

    // ── Send indication ──

    async fn handle_send_indication(&self, msg: &StunMessage, client: ClientKey) -> Result<()> {
        let (Some(peer), Some(data)) = (msg.get_xor_address(StunAttributeType::XorPeerAddress), msg.get_attribute(StunAttributeType::Data)) else {
            return Ok(());
        };
        if data.value.len() > MAX_RELAY_PAYLOAD {
            return Ok(());
        }
        let target = {
            let Some(alloc) = self.allocations.get(&client) else { return Ok(()) };
            if alloc.is_expired() || !alloc.has_permission(&peer.ip()) {
                debug!("Send indication to {} without permission from {}", peer, client.addr);
                return Ok(());
            }
            alloc.bytes_relayed_out.fetch_add(data.value.len() as u64, Ordering::Relaxed);
            alloc.relay_socket.clone()
        };
        let _ = target.send_to(&data.value, peer).await;
        Ok(())
    }

    pub fn generate_turn_credentials(&self, user_id: &str, ttl_secs: i64) -> (String, String) {
        let c = aurix_common::crypto::generate_turn_credentials(&self.auth_secret, user_id, ttl_secs, chrono::Utc::now().timestamp());
        (c.username, c.password)
    }

    pub fn cleanup_expired(&self) {
        let expired: Vec<ClientKey> = self.allocations.iter().filter(|a| a.is_expired()).map(|a| a.client).collect();
        for key in &expired {
            self.remove_allocation(key);
        }
        if !expired.is_empty() {
            info!("Cleaned up {} expired TURN allocations", expired.len());
        }
    }

    pub fn realm(&self) -> &str {
        &self.realm
    }

    /// Whether a loopback/unspecified relay IP is in use (only sensible for tests).
    pub fn relay_bind_ip(&self) -> IpAddr {
        self.relay_bind_ip
    }
}

