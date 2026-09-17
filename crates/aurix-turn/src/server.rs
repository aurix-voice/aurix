use crate::allocation::ClientProtocol;
use crate::stun::{StunMessage, STUN_HEADER_SIZE};
use crate::turn::{ClientSink, TurnHandler};
use aurix_common::config::TurnConfig;
use aurix_common::error::{AurixError, Result};
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, UdpSocket};
use tokio::sync::mpsc;
use tracing::{debug, error, info, warn};

const TCP_MAX_FRAME: usize = 64 * 1024;
const TCP_IDLE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(600);

pub struct TurnServer {
    handler: Arc<TurnHandler>,
    config: TurnConfig,
}

impl TurnServer {
    pub fn new(config: &TurnConfig) -> Self {
        let external_ip = config
            .external_ip
            .as_deref()
            .and_then(|s| s.parse::<std::net::IpAddr>().ok())
            .or_else(|| config.host.parse::<std::net::IpAddr>().ok())
            .filter(|ip| !ip.is_unspecified());
        let mut handler = TurnHandler::new(
            config.realm.clone(),
            config.auth_secret.clone(),
            config.min_port,
            config.max_port,
            config.max_allocations,
            config.allocation_lifetime_secs as i64,
            external_ip,
        );
        if let Ok(ip) = config.host.parse::<std::net::IpAddr>() {
            if ip.is_loopback() {
                handler.set_relay_bind_ip(ip);
            }
        }
        Self {
            handler: Arc::new(handler),
            config: config.clone(),
        }
    }

    pub fn handler(&self) -> &Arc<TurnHandler> {
        &self.handler
    }

    /// Bind the UDP and TCP listeners and return the bound UDP address; the receive loops run
    /// in background tasks until `run` completes.
    pub async fn run(&self) -> Result<()> {
        let (socket, _tcp) = self.bind().await?;
        self.serve_udp(socket).await
    }

    /// Bind sockets without entering the receive loop (used by tests and by `run`).
    pub async fn bind(&self) -> Result<(Arc<UdpSocket>, SocketAddr)> {
        let udp_addr = format!("{}:{}", self.config.host, self.config.udp_port);
        let socket = UdpSocket::bind(&udp_addr).await.map_err(|e| {
            AurixError::StunTurn(format!("Failed to bind TURN UDP {udp_addr}: {e}"))
        })?;
        let socket = Arc::new(socket);
        info!(
            "TURN server listening on UDP {}",
            socket
                .local_addr()
                .map(|a| a.to_string())
                .unwrap_or(udp_addr)
        );

        let tcp_addr = format!("{}:{}", self.config.host, self.config.tcp_port);
        let listener = TcpListener::bind(&tcp_addr).await.map_err(|e| {
            AurixError::StunTurn(format!("Failed to bind TURN TCP {tcp_addr}: {e}"))
        })?;
        let tcp_local = listener
            .local_addr()
            .map_err(|e| AurixError::StunTurn(e.to_string()))?;
        info!("TURN server listening on TCP {}", tcp_local);
        let handler_tcp = self.handler.clone();
        tokio::spawn(async move {
            loop {
                match listener.accept().await {
                    Ok((stream, peer)) => {
                        let h = handler_tcp.clone();
                        tokio::spawn(Self::handle_tcp_client(stream, peer, h));
                    }
                    Err(e) => {
                        error!("TURN TCP accept error: {e}");
                        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                    }
                }
            }
        });

        let handler_cleanup = self.handler.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(30));
            loop {
                interval.tick().await;
                handler_cleanup.cleanup_expired();
            }
        });
        Ok((socket, tcp_local))
    }

    pub async fn serve_udp(&self, socket: Arc<UdpSocket>) -> Result<()> {
        let sink = ClientSink::Udp(socket.clone());
        let mut buf = vec![0u8; 4096];
        loop {
            match socket.recv_from(&mut buf).await {
                Ok((len, src)) => {
                    let data = &buf[..len];
                    if len >= 4 && (data[0] & 0xC0) == 0x40 {
                        self.handler
                            .handle_channel_data(data, src, ClientProtocol::Udp)
                            .await;
                        continue;
                    }
                    if !StunMessage::is_stun(data) {
                        continue;
                    }
                    match StunMessage::decode(data) {
                        Ok(msg) => {
                            if let Err(e) = self
                                .handler
                                .handle_stun_message(&msg, src, &sink, data)
                                .await
                            {
                                warn!("STUN handler error from {}: {}", src, e);
                            }
                        }
                        Err(e) => debug!("STUN decode error from {}: {}", src, e),
                    }
                }
                Err(e) => {
                    error!("TURN UDP recv error: {}", e);
                    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                }
            }
        }
    }

    /// TURN over TCP (RFC 8656 §3.1): STUN messages and ChannelData frames are self-delimiting;
    /// ChannelData is padded to a 4-byte boundary on stream transports.
    async fn handle_tcp_client(
        stream: tokio::net::TcpStream,
        peer: SocketAddr,
        handler: Arc<TurnHandler>,
    ) {
        info!("TURN TCP client connected: {}", peer);
        let _ = stream.set_nodelay(true);
        let (mut rd, mut wr) = stream.into_split();
        let (tx, mut rx) = mpsc::channel::<Vec<u8>>(256);
        let writer = tokio::spawn(async move {
            while let Some(frame) = rx.recv().await {
                if wr.write_all(&frame).await.is_err() {
                    break;
                }
            }
        });
        let sink = ClientSink::Tcp(tx);

        let mut buf: Vec<u8> = Vec::with_capacity(8192);
        let mut chunk = vec![0u8; 4096];
        loop {
            let n = match tokio::time::timeout(TCP_IDLE_TIMEOUT, rd.read(&mut chunk)).await {
                Ok(Ok(0)) | Err(_) => break,
                Ok(Ok(n)) => n,
                Ok(Err(_)) => break,
            };
            buf.extend_from_slice(&chunk[..n]);
            if buf.len() > TCP_MAX_FRAME {
                warn!("TURN TCP client {} exceeded frame limit", peer);
                break;
            }
            while let Some(frame_len) = Self::tcp_frame_len(&buf) {
                if frame_len > TCP_MAX_FRAME {
                    buf.clear();
                    break;
                }
                if buf.len() < frame_len {
                    break;
                }
                let frame: Vec<u8> = buf.drain(..frame_len).collect();
                if (frame[0] & 0xC0) == 0x40 {
                    handler
                        .handle_channel_data(&frame, peer, ClientProtocol::Tcp)
                        .await;
                } else if let Ok(msg) = StunMessage::decode(&frame) {
                    if let Err(e) = handler.handle_stun_message(&msg, peer, &sink, &frame).await {
                        warn!("TURN TCP handler error from {}: {}", peer, e);
                    }
                }
            }
        }
        handler.remove_client(peer, ClientProtocol::Tcp);
        writer.abort();
        info!("TURN TCP client disconnected: {}", peer);
    }

    /// Length of the next complete frame at the head of `buf`, or `None` if more bytes are needed.
    fn tcp_frame_len(buf: &[u8]) -> Option<usize> {
        if buf.len() < 4 {
            return None;
        }
        match buf[0] & 0xC0 {
            0x40 => {
                let len = u16::from_be_bytes([buf[2], buf[3]]) as usize;
                Some(4 + len + (4 - (len % 4)) % 4)
            }
            0x00 => {
                if buf.len() < STUN_HEADER_SIZE {
                    return None;
                }
                StunMessage::stun_frame_len(buf).or(Some(usize::MAX))
            }
            _ => Some(usize::MAX),
        }
    }
}
