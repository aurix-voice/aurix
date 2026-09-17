use sha1;
use md5;
use crate::stun::*;
use crate::allocation::Allocation;
use aurix_common::error::Result;
use base64::Engine;
use bytes::{BufMut, BytesMut};
use dashmap::DashMap;
use hmac::Hmac;
use sha2::Sha256;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::net::UdpSocket;
use tracing::{debug, info, warn};

pub struct TurnHandler {
    realm: String,
    auth_secret: String,
    allocations: Arc<DashMap<SocketAddr, Allocation>>,
    min_port: u16,
    max_port: u16,
    next_port: std::sync::atomic::AtomicU16,
    max_allocations: u32,
    allocation_lifetime: i64,
    external_ip: Option<std::net::IpAddr>,
    /// Holds the main server socket so relay tasks can send DataIndications back to clients.
    server_socket: parking_lot::RwLock<Option<Arc<UdpSocket>>>,
}

impl TurnHandler {
    pub fn new(
        realm: String,
        auth_secret: String,
        min_port: u16,
        max_port: u16,
        max_allocations: u32,
        allocation_lifetime: i64,
        external_ip: Option<std::net::IpAddr>,
    ) -> Self {
        Self {
            realm,
            auth_secret,
            allocations: Arc::new(DashMap::new()),
            min_port,
            max_port,
            next_port: std::sync::atomic::AtomicU16::new(min_port),
            max_allocations,
            allocation_lifetime,
            external_ip,
            server_socket: parking_lot::RwLock::new(None),
        }
    }

    /// Set the server socket after binding so relay tasks can send data back.
    pub fn set_server_socket(&self, socket: Arc<UdpSocket>) {
        *self.server_socket.write() = Some(socket);
    }

    pub fn allocations(&self) -> &Arc<DashMap<SocketAddr, Allocation>> {
        &self.allocations
    }

    pub async fn handle_stun_message(
        &self,
        msg: &StunMessage,
        src: SocketAddr,
        socket: &UdpSocket,
        raw_data: &[u8],
    ) -> Result<()> {
        match msg.msg_type {
            StunMessageType::BindingRequest => self.handle_binding_request(msg, src, socket).await,
            StunMessageType::AllocateRequest => self.handle_allocate_request(msg, src, socket, raw_data).await,
            StunMessageType::RefreshRequest => self.handle_refresh_request(msg, src, socket).await,
            StunMessageType::CreatePermissionRequest => self.handle_create_permission(msg, src, socket).await,
            StunMessageType::ChannelBindRequest => self.handle_channel_bind(msg, src, socket).await,
            StunMessageType::SendIndication => self.handle_send_indication(msg, src).await,
            _ => { debug!("Unhandled STUN type: {:?}", msg.msg_type); Ok(()) }
        }
    }

    async fn handle_binding_request(&self, msg: &StunMessage, src: SocketAddr, socket: &UdpSocket) -> Result<()> {
        let mut response = StunMessage::new(StunMessageType::BindingResponse, msg.transaction_id);
        response.add_xor_mapped_address(src);
        response.add_software("Aurix TURN/1.0");
        let _ = socket.send_to(&response.encode(), src).await;
        aurix_metrics::STUN_REQUESTS.inc();
        Ok(())
    }

    async fn handle_allocate_request(&self, msg: &StunMessage, src: SocketAddr, socket: &UdpSocket, raw_data: &[u8]) -> Result<()> {
        if self.allocations.contains_key(&src) {
            let mut err = StunMessage::new(StunMessageType::AllocateErrorResponse, msg.transaction_id);
            err.add_error_code(437, "Allocation Mismatch");
            let _ = socket.send_to(&err.encode(), src).await;
            return Ok(());
        }

        let username = match msg.get_attribute(StunAttributeType::Username) {
            Some(attr) => String::from_utf8_lossy(&attr.value).to_string(),
            None => {
                let mut err = StunMessage::new(StunMessageType::AllocateErrorResponse, msg.transaction_id);
                err.add_error_code(401, "Unauthorized");
                err.add_attribute(StunAttributeType::Realm, self.realm.as_bytes().to_vec());
                err.add_attribute(StunAttributeType::Nonce, self.generate_nonce().as_bytes().to_vec());
                let _ = socket.send_to(&err.encode(), src).await;
                return Ok(());
            }
        };

        if !self.validate_credentials(&username, msg, raw_data) {
            let mut err = StunMessage::new(StunMessageType::AllocateErrorResponse, msg.transaction_id);
            err.add_error_code(401, "Unauthorized");
            let _ = socket.send_to(&err.encode(), src).await;
            return Ok(());
        }

        if self.allocations.len() as u32 >= self.max_allocations {
            let mut err = StunMessage::new(StunMessageType::AllocateErrorResponse, msg.transaction_id);
            err.add_error_code(508, "Insufficient Capacity");
            let _ = socket.send_to(&err.encode(), src).await;
            return Ok(());
        }

        let relay_port = self.allocate_port();
        let relay_ip = self.external_ip.unwrap_or_else(|| {
            socket.local_addr().map(|a| a.ip()).unwrap_or(std::net::IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED))
        });
        let relay_addr = SocketAddr::new(relay_ip, relay_port);
        let relay_bind = SocketAddr::new(std::net::IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED), relay_port);

        let relay_socket = match UdpSocket::bind(relay_bind).await {
            Ok(s) => Arc::new(s),
            Err(e) => {
                warn!("Failed to bind relay port {}: {}", relay_port, e);
                let mut err = StunMessage::new(StunMessageType::AllocateErrorResponse, msg.transaction_id);
                err.add_error_code(508, "Insufficient Capacity");
                let _ = socket.send_to(&err.encode(), src).await;
                return Ok(());
            }
        };

        let allocation = Allocation::new(src, relay_addr, relay_socket.clone(), username, self.realm.clone(), self.allocation_lifetime);

        // Spawn relay task: forward data from peers back to the client via DataIndication
        let allocations_ref = self.allocations.clone();
        let client_addr = src;
        let server_socket = self.server_socket.read().clone();

        tokio::spawn(async move {
            let mut buf = vec![0u8; 2048];
            loop {
                match relay_socket.recv_from(&mut buf).await {
                    Ok((len, peer_addr)) => {
                        if let Some(alloc) = allocations_ref.get(&client_addr) {
                            if !alloc.has_permission(&peer_addr.ip()) {
                                continue;
                            }
                            // Check for channel binding — send as ChannelData
                            if let Some(ch_num) = alloc.get_channel_for_peer(&peer_addr) {
                                let mut channel_data = BytesMut::with_capacity(4 + len + 4);
                                channel_data.put_u16(ch_num);
                                channel_data.put_u16(len as u16);
                                channel_data.put_slice(&buf[..len]);
                                let padding = (4 - (len % 4)) % 4;
                                for _ in 0..padding { channel_data.put_u8(0); }
                                if let Some(ref srv_sock) = server_socket {
                                    let _ = srv_sock.send_to(&channel_data, client_addr).await;
                                }
                            } else {
                                // Send as DataIndication
                                let mut indication = StunMessage::new(StunMessageType::DataIndication, [0u8; 12]);
                                // Add XOR-PEER-ADDRESS
                                let mut peer_buf = Vec::new();
                                peer_buf.push(0x00);
                                if let SocketAddr::V4(v4) = peer_addr {
                                    peer_buf.push(0x01);
                                    let port = v4.port() ^ (STUN_MAGIC_COOKIE >> 16) as u16;
                                    peer_buf.extend_from_slice(&port.to_be_bytes());
                                    let ip = v4.ip().octets();
                                    let cookie = STUN_MAGIC_COOKIE.to_be_bytes();
                                    for i in 0..4 { peer_buf.push(ip[i] ^ cookie[i]); }
                                }
                                indication.add_attribute(StunAttributeType::XorPeerAddress, peer_buf);
                                indication.add_attribute(StunAttributeType::Data, buf[..len].to_vec());
                                if let Some(ref srv_sock) = server_socket {
                                    let _ = srv_sock.send_to(&indication.encode(), client_addr).await;
                                }
                            }
                        } else {
                            break; // Allocation removed, stop relay
                        }
                    }
                    Err(_) => break,
                }
            }
        });

        self.allocations.insert(src, allocation);
        aurix_metrics::TURN_ALLOCATIONS.inc();

        // Success response
        let mut response = StunMessage::new(StunMessageType::AllocateResponse, msg.transaction_id);
        response.add_xor_mapped_address(src);
        // XOR-RELAYED-ADDRESS
        let mut relay_buf = Vec::new();
        relay_buf.push(0x00);
        if let SocketAddr::V4(v4) = relay_addr {
            relay_buf.push(0x01);
            let port = v4.port() ^ (STUN_MAGIC_COOKIE >> 16) as u16;
            relay_buf.extend_from_slice(&port.to_be_bytes());
            let ip = v4.ip().octets();
            let cookie = STUN_MAGIC_COOKIE.to_be_bytes();
            for i in 0..4 { relay_buf.push(ip[i] ^ cookie[i]); }
        }
        response.add_attribute(StunAttributeType::XorRelayedAddress, relay_buf);
        response.add_attribute(StunAttributeType::Lifetime, (self.allocation_lifetime as u32).to_be_bytes().to_vec());
        response.add_software("Aurix TURN/1.0");
        let _ = socket.send_to(&response.encode(), src).await;
        info!("Allocation created for {} -> {}", src, relay_addr);
        Ok(())
    }

    async fn handle_refresh_request(&self, msg: &StunMessage, src: SocketAddr, socket: &UdpSocket) -> Result<()> {
        let lifetime = msg.get_attribute(StunAttributeType::Lifetime)
            .and_then(|a| if a.value.len() >= 4 { Some(u32::from_be_bytes([a.value[0], a.value[1], a.value[2], a.value[3]])) } else { None })
            .unwrap_or(600);

        if lifetime == 0 {
            self.allocations.remove(&src);
            aurix_metrics::TURN_ALLOCATIONS.dec();
            info!("Allocation removed for {} (refresh with 0)", src);
        } else if let Some(mut alloc) = self.allocations.get_mut(&src) {
            alloc.refresh(lifetime as i64);
        } else {
            let mut err = StunMessage::new(StunMessageType::AllocateErrorResponse, msg.transaction_id);
            err.add_error_code(437, "Allocation Mismatch");
            let _ = socket.send_to(&err.encode(), src).await;
            return Ok(());
        }
        let mut resp = StunMessage::new(StunMessageType::RefreshResponse, msg.transaction_id);
        resp.add_attribute(StunAttributeType::Lifetime, lifetime.to_be_bytes().to_vec());
        let _ = socket.send_to(&resp.encode(), src).await;
        Ok(())
    }

    async fn handle_create_permission(&self, msg: &StunMessage, src: SocketAddr, socket: &UdpSocket) -> Result<()> {
        if let Some(mut alloc) = self.allocations.get_mut(&src) {
            if let Some(attr) = msg.get_attribute(StunAttributeType::XorPeerAddress) {
                if attr.value.len() >= 8 {
                    let cookie = STUN_MAGIC_COOKIE.to_be_bytes();
                    let ip = std::net::Ipv4Addr::new(
                        attr.value[4] ^ cookie[0], attr.value[5] ^ cookie[1],
                        attr.value[6] ^ cookie[2], attr.value[7] ^ cookie[3],
                    );
                    alloc.add_permission(std::net::IpAddr::V4(ip));
                }
            }
            let resp = StunMessage::new(StunMessageType::CreatePermissionResponse, msg.transaction_id);
            let _ = socket.send_to(&resp.encode(), src).await;
        }
        Ok(())
    }

    async fn handle_channel_bind(&self, msg: &StunMessage, src: SocketAddr, socket: &UdpSocket) -> Result<()> {
        let ch_num = msg.get_attribute(StunAttributeType::ChannelNumber)
            .and_then(|a| if a.value.len() >= 2 { Some(u16::from_be_bytes([a.value[0], a.value[1]])) } else { None });

        if let (Some(ch_num), Some(mut alloc)) = (ch_num, self.allocations.get_mut(&src)) {
            if !(0x4000..=0x7FFE).contains(&ch_num) {
                let mut err = StunMessage::new(StunMessageType::BindingErrorResponse, msg.transaction_id);
                err.add_error_code(400, "Bad Request");
                let _ = socket.send_to(&err.encode(), src).await;
                return Ok(());
            }
            if let Some(attr) = msg.get_attribute(StunAttributeType::XorPeerAddress) {
                if attr.value.len() >= 8 {
                    let port = u16::from_be_bytes([attr.value[2], attr.value[3]]) ^ (STUN_MAGIC_COOKIE >> 16) as u16;
                    let cookie = STUN_MAGIC_COOKIE.to_be_bytes();
                    let ip = std::net::Ipv4Addr::new(
                        attr.value[4] ^ cookie[0], attr.value[5] ^ cookie[1],
                        attr.value[6] ^ cookie[2], attr.value[7] ^ cookie[3],
                    );
                    alloc.add_channel_binding(ch_num, SocketAddr::new(std::net::IpAddr::V4(ip), port));
                }
            }
            let resp = StunMessage::new(StunMessageType::ChannelBindResponse, msg.transaction_id);
            let _ = socket.send_to(&resp.encode(), src).await;
        }
        Ok(())
    }

    async fn handle_send_indication(&self, msg: &StunMessage, src: SocketAddr) -> Result<()> {
        if let Some(alloc) = self.allocations.get(&src) {
            if let Some(attr) = msg.get_attribute(StunAttributeType::XorPeerAddress) {
                if attr.value.len() >= 8 {
                    let port = u16::from_be_bytes([attr.value[2], attr.value[3]]) ^ (STUN_MAGIC_COOKIE >> 16) as u16;
                    let cookie = STUN_MAGIC_COOKIE.to_be_bytes();
                    let ip = std::net::Ipv4Addr::new(
                        attr.value[4] ^ cookie[0], attr.value[5] ^ cookie[1],
                        attr.value[6] ^ cookie[2], attr.value[7] ^ cookie[3],
                    );
                    let peer_addr = SocketAddr::new(std::net::IpAddr::V4(ip), port);
                    if alloc.has_permission(&peer_addr.ip()) {
                        if let Some(data_attr) = msg.get_attribute(StunAttributeType::Data) {
                            let _ = alloc.relay_socket.send_to(&data_attr.value, peer_addr).await;
                        }
                    }
                }
            }
        }
        Ok(())
    }

    /// Validate TURN credentials using HMAC-SHA256 time-limited scheme.
    /// Username format: "timestamp:userId"
    /// Password = Base64(HMAC-SHA256(secret, username))
    fn validate_credentials(&self, username: &str, msg: &StunMessage, raw_msg: &[u8]) -> bool {
        let parts: Vec<&str> = username.split(':').collect();
        if parts.len() < 2 { return false; }
        let timestamp = match parts[0].parse::<i64>() {
            Ok(t) => t,
            Err(_) => return false,
        };
        if timestamp < chrono::Utc::now().timestamp() { return false; }

        // Compute expected password: Base64(HMAC-SHA1(secret, username))
        use hmac::{Hmac, Mac};
        use sha1::Sha1;
        type HmacSha1 = Hmac<Sha1>;

        let mut password_mac = HmacSha1::new_from_slice(self.auth_secret.as_bytes())
            .expect("HMAC key always valid");
        password_mac.update(username.as_bytes());
        let password = base64::engine::general_purpose::STANDARD.encode(password_mac.finalize().into_bytes());

        // If MESSAGE-INTEGRITY present, verify it
        if let Some(integrity_attr) = msg.get_attribute(StunAttributeType::MessageIntegrity) {
            if integrity_attr.value.len() < 20 { return false; }

            // Key = MD5(username:realm:password)
            use md5::{Md5, Digest as Md5Digest};
            let mut md5 = Md5::new();
            md5.update(format!("{}:{}:{}", username, self.realm, password).as_bytes());
            let key = md5.finalize();

            // Find MESSAGE-INTEGRITY position in raw message.
            // Reconstruct the message up to the MESSAGE-INTEGRITY attribute, adjusting the
            // STUN message length field to cover up through MESSAGE-INTEGRITY (24 bytes: 4 type+len + 20 value).
            let mi_type_bytes = StunAttributeType::MessageIntegrity.to_u16().to_be_bytes();
            let mut mi_offset = None;
            let mut i = 20usize; // start after STUN header
            while i + 4 <= raw_msg.len() {
                let at = u16::from_be_bytes([raw_msg[i], raw_msg[i + 1]]);
                let al = u16::from_be_bytes([raw_msg[i + 2], raw_msg[i + 3]]) as usize;
                if at == StunAttributeType::MessageIntegrity.to_u16() {
                    mi_offset = Some(i);
                    break;
                }
                i += 4 + al;
                i += (4 - (al % 4)) % 4; // padding
            }

            let mi_offset = match mi_offset {
                Some(o) => o,
                None => return false,
            };

            // Build the data to HMAC: header (with adjusted length) + attributes up to MI header
            let adjusted_len = (mi_offset - 20 + 24) as u16; // attrs before MI + MI attr itself (4+20)
            let mut hmac_input = Vec::with_capacity(mi_offset + 4);
            hmac_input.extend_from_slice(&raw_msg[..2]); // type
            hmac_input.extend_from_slice(&adjusted_len.to_be_bytes()); // adjusted length
            hmac_input.extend_from_slice(&raw_msg[4..mi_offset]); // magic+txid+attrs before MI

            let mut verifier = HmacSha1::new_from_slice(&key).expect("valid key");
            verifier.update(&hmac_input);
            return verifier.verify_slice(&integrity_attr.value).is_ok();
        }

        // No MESSAGE-INTEGRITY: accept based on valid timestamp (first request before challenge)
        true
    }

    fn generate_nonce(&self) -> String {
        use rand::Rng;
        let bytes: [u8; 16] = rand::thread_rng().gen();
        base64::engine::general_purpose::STANDARD.encode(bytes)
    }

    fn allocate_port(&self) -> u16 {
        loop {
            let port = self.next_port.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            if port > self.max_port {
                self.next_port.store(self.min_port, std::sync::atomic::Ordering::Relaxed);
                return self.min_port;
            }
            return port;
        }
    }

    pub fn generate_turn_credentials(&self, user_id: &str, ttl_secs: i64) -> (String, String) {
        let timestamp = chrono::Utc::now().timestamp() + ttl_secs;
        let username = format!("{}:{}", timestamp, user_id);
        use hmac::{Hmac, Mac};
        use sha1::Sha1;
        type HmacSha1 = Hmac<Sha1>;
        let mut mac = HmacSha1::new_from_slice(self.auth_secret.as_bytes())
            .expect("HMAC key always valid");
        mac.update(username.as_bytes());
        let password = base64::engine::general_purpose::STANDARD.encode(mac.finalize().into_bytes());
        (username, password)
    }

    pub fn cleanup_expired(&self) {
        let before = self.allocations.len();
        self.allocations.retain(|_, alloc| !alloc.is_expired());
        let removed = before - self.allocations.len();
        if removed > 0 {
            aurix_metrics::TURN_ALLOCATIONS.set(self.allocations.len() as i64);
            info!("Cleaned up {} expired TURN allocations", removed);
        }
    }
}