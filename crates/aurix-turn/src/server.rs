use crate::stun::StunMessage;
use crate::turn::TurnHandler;
use aurix_common::config::TurnConfig;
use aurix_common::error::{AurixError, Result};
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, UdpSocket};
use tracing::{error, info, warn};

pub struct TurnServer {
    handler: Arc<TurnHandler>,
    config: TurnConfig,
}

impl TurnServer {
    pub fn new(config: &TurnConfig) -> Self {
        let external_ip = config.host.parse::<std::net::IpAddr>().ok()
            .filter(|ip| !ip.is_unspecified());
        let handler = Arc::new(TurnHandler::new(
            config.realm.clone(), config.auth_secret.clone(),
            config.min_port, config.max_port, config.max_allocations,
            config.allocation_lifetime_secs as i64, external_ip,
        ));
        Self { handler, config: config.clone() }
    }

    pub fn handler(&self) -> &Arc<TurnHandler> { &self.handler }

    pub async fn run(&self) -> Result<()> {
        let udp_addr = format!("{}:{}", self.config.host, self.config.udp_port);
        let socket = UdpSocket::bind(&udp_addr).await
            .map_err(|e| AurixError::StunTurn(format!("Failed to bind TURN UDP: {e}")))?;
        let socket = Arc::new(socket);
        info!("TURN server listening on UDP {}", udp_addr);
        self.handler.set_server_socket(socket.clone());

        // ── Spawn TCP listener ──
        let tcp_addr = format!("{}:{}", self.config.host, self.config.tcp_port);
        let handler_tcp = self.handler.clone();
        tokio::spawn(async move {
            match TcpListener::bind(&tcp_addr).await {
                Ok(listener) => {
                    info!("TURN server listening on TCP {}", tcp_addr);
                    loop {
                        match listener.accept().await {
                            Ok((stream, peer)) => {
                                let h = handler_tcp.clone();
                                tokio::spawn(Self::handle_tcp_client(stream, peer, h));
                            }
                            Err(e) => { error!("TCP accept error: {e}"); }
                        }
                    }
                }
                Err(e) => { error!("Failed to bind TURN TCP {}: {e}", tcp_addr); }
            }
        });

        // ── Periodic cleanup ──
        let handler_cleanup = self.handler.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(30));
            loop { interval.tick().await; handler_cleanup.cleanup_expired(); }
        });

        // ── UDP receive loop ──
        let mut buf = vec![0u8; 4096];
        loop {
            match socket.recv_from(&mut buf).await {
                Ok((len, src)) => {
                    let data = &buf[..len];
                    if len >= 4 && (data[0] & 0xC0) == 0x40 {
                        self.handle_channel_data(data, src).await;
                        continue;
                    }
                    if !StunMessage::is_stun(data) { continue; }
                    match StunMessage::decode(data) {
                        Ok(msg) => {
                            if let Err(e) = self.handler.handle_stun_message(&msg, src, &socket, data).await {
                                warn!("STUN handler error from {}: {}", src, e);
                            }
                        }
                        Err(e) => warn!("STUN decode error from {}: {}", src, e),
                    }
                }
                Err(e) => {
                    error!("TURN UDP recv error: {}", e);
                    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                }
            }
        }
    }

    /// Handle a single TCP TURN client using RFC 4571 framing:
    /// each STUN message is preceded by a 2-byte big-endian length prefix.
    async fn handle_tcp_client(
        mut stream: tokio::net::TcpStream,
        peer: std::net::SocketAddr,
        handler: Arc<TurnHandler>,
    ) {
        info!("TCP TURN client connected: {}", peer);
        let mut buf = vec![0u8; 4096];
        loop {
            // Read 2-byte length prefix
            if stream.read_exact(&mut buf[..2]).await.is_err() { break; }
            let msg_len = u16::from_be_bytes([buf[0], buf[1]]) as usize;
            if msg_len > 4096 || msg_len == 0 { break; }
            if stream.read_exact(&mut buf[..msg_len]).await.is_err() { break; }

            let data = &buf[..msg_len];
            if !StunMessage::is_stun(data) { continue; }
            match StunMessage::decode(data) {
                Ok(msg) => {
                    // For TCP responses, we encode and send back with length prefix
                    let response = match msg.msg_type {
                        crate::stun::StunMessageType::BindingRequest => {
                            let mut resp = StunMessage::new(crate::stun::StunMessageType::BindingResponse, msg.transaction_id);
                            resp.add_xor_mapped_address(peer);
                            resp.add_software("Aurix TURN/1.0");
                            Some(resp.encode())
                        }
                        // Other request types would need the full handler,
                        // but TCP TURN allocations work differently (relay via TCP, not UDP).
                        // For binding and connectivity checks, this suffices.
                        _ => {
                            // Create a temporary UDP-like socket for the handler
                            // In production, TURN-TCP relay uses TCP connections, not UDP.
                            // Send error for unsupported TCP TURN methods.
                            let mut err = StunMessage::new(crate::stun::StunMessageType::AllocateErrorResponse, msg.transaction_id);
                            err.add_error_code(400, "TCP allocations use TCP relay");
                            Some(err.encode())
                        }
                    };
                    if let Some(resp_bytes) = response {
                        let len = (resp_bytes.len() as u16).to_be_bytes();
                        if stream.write_all(&len).await.is_err() { break; }
                        if stream.write_all(&resp_bytes).await.is_err() { break; }
                    }
                }
                Err(e) => warn!("TCP STUN decode error from {}: {e}", peer),
            }
        }
        info!("TCP TURN client disconnected: {}", peer);
    }

    async fn handle_channel_data(&self, data: &[u8], src: std::net::SocketAddr) {
        if data.len() < 4 { return; }
        let channel_number = u16::from_be_bytes([data[0], data[1]]);
        let data_len = u16::from_be_bytes([data[2], data[3]]) as usize;
        if data.len() < 4 + data_len { return; }
        let payload = &data[4..4 + data_len];
        if let Some(alloc) = self.handler.allocations().get(&src) {
            if let Some(binding) = alloc.get_channel_binding(channel_number) {
                let _ = alloc.relay_socket.send_to(payload, binding.peer_addr).await;
            }
        }
    }
}