//! AURX over QUIC datagrams on the media port: the low-latency alternative to raw UDP for
//! native clients that want 0-RTT reconnects and connection migration (Wi-Fi ↔ cellular,
//! NAT rebinding) without losing their media path.
//!
//! Design:
//! * **One socket.** QUIC shares the media UDP socket with raw AURX and WebRTC. The receive
//!   workers classify by first byte (`is_aurix_packet` / `is_webrtc_packet` / else QUIC) and
//!   hand QUIC datagrams to a [`quinn::Endpoint`] through [`SharedQuicSocket`]; server-chosen
//!   connection IDs always start with a byte ≥ 0x80, so a QUIC short-header packet can never
//!   spell the AURX magic (`0x41 0x55 …`). Clients therefore reach QUIC at exactly the address
//!   they already use for UDP — no extra port, no extra firewall rule.
//! * **Datagrams only.** Every QUIC DATAGRAM frame carries exactly one sealed AURX packet;
//!   streams are disabled (`max_concurrent_*_streams = 0`), so a lost datagram never blocks
//!   the next one (no head-of-line blocking, unlike the WebSocket tunnel).
//! * **Wire format unchanged.** Authentication (per-session HMAC + AEAD), replay windows,
//!   sequence numbering, E2EE and PCMU are exactly the UDP ones. TLS only hides the packets
//!   from on-path observers and gives the connection its identity; it is *not* trusted for
//!   session ownership: a connection speaks for a session only after an authenticated
//!   `SessionBind` arrived on it, and later datagrams are attributed by the connection the
//!   session is bound to — never by source address (which migration changes).
//! * **0-RTT.** Early data may be replayed by an attacker. Everything a client may send is
//!   already replay-safe at the AURX layer (`SessionBind` has a strictly increasing timestamp,
//!   media/heartbeats sit in the per-session anti-replay window), so early datagrams are fed
//!   to the same router path as 1-RTT ones and nothing else is derived from them.
//! * **Certificates.** The node uses an operator-provided PEM pair or generates a self-signed
//!   one at start; the SHA-256 of the DER certificate is advertised over the authenticated
//!   control channel (`SessionInitAck.quic.cert_sha256`) and pinned by the client, so no PKI
//!   is needed and no CA can impersonate the node.

use crate::cert::MediaCert;
use crate::transport::MediaSocket;
use aurix_common::protocol::{QuicInfo, QUIC_ALPN};
use aurix_common::quic::{endpoint_config, transport_config};
use aurix_common::types::SessionId;
use aurix_common::{AurixError, Result};
use parking_lot::Mutex;
use quinn::rustls;
use std::io::IoSliceMut;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;
use tokio::sync::mpsc;
use tracing::{debug, info};

static NEXT_LINK_ID: AtomicU64 = AtomicU64::new(1);

/// Media-plane QUIC tunables taken from `MediaConfig`.
#[derive(Debug, Clone)]
pub struct QuicOptions {
    /// Accept AURX over QUIC on the media socket at all.
    pub enabled: bool,
    /// Connection idle timeout; clients heartbeat, so ≥ 3 heartbeat intervals.
    pub idle_timeout: Duration,
    /// Accept early (0-RTT) datagrams from resuming clients.
    pub zero_rtt: bool,
    /// Let a connection follow its peer to a new address.
    pub migration: bool,
    /// Downlink datagram queue depth per connection, in AURX packets.
    pub queue_packets: usize,
    /// Open connections accepted at once (0 = unlimited).
    pub max_connections: usize,
    /// Operator-provided PEM certificate chain and private key (`None`: self-signed at start).
    pub cert: Option<(PathBuf, PathBuf)>,
    /// TLS server name of the certificate (SAN of the generated one, SNI the client sends).
    pub server_name: String,
}

impl Default for QuicOptions {
    fn default() -> Self {
        Self {
            enabled: true,
            idle_timeout: Duration::from_secs(20),
            zero_rtt: true,
            migration: true,
            queue_packets: 128,
            max_connections: 0,
            cert: None,
            server_name: "aurix-media".into(),
        }
    }
}

/// One QUIC connection of the media endpoint, from the SFU's point of view: the downlink
/// handle a [`MediaSession`] holds while bound through it plus the bookkeeping the router
/// needs to attribute uplink datagrams.
///
/// [`MediaSession`]: crate::session::MediaSession
#[derive(Debug)]
pub struct QuicLink {
    id: u64,
    conn: quinn::Connection,
    /// Session that authenticated a `SessionBind` over this connection (`None` until then).
    session: Mutex<Option<SessionId>>,
    /// Peer address at the last packet, to notice migrations.
    path: Mutex<SocketAddr>,
    sent: AtomicU64,
    dropped: AtomicU64,
    migrations: AtomicU64,
}

impl QuicLink {
    pub fn new(conn: quinn::Connection) -> Arc<Self> {
        Arc::new(Self {
            id: NEXT_LINK_ID.fetch_add(1, Ordering::Relaxed),
            path: Mutex::new(conn.remote_address()),
            conn,
            session: Mutex::new(None),
            sent: AtomicU64::new(0),
            dropped: AtomicU64::new(0),
            migrations: AtomicU64::new(0),
        })
    }

    /// Node-unique identity (every connection gets its own link).
    pub fn id(&self) -> u64 {
        self.id
    }

    /// Session this connection proved (with an authenticated `SessionBind`) to speak for.
    pub fn session_id(&self) -> Option<SessionId> {
        *self.session.lock()
    }

    /// Records the authenticated owner; a connection never changes owner once it has one.
    /// Returns `false` when it already belongs to a different session.
    pub fn claim(&self, session_id: SessionId) -> bool {
        let mut slot = self.session.lock();
        match *slot {
            None => {
                *slot = Some(session_id);
                true
            }
            Some(current) => current == session_id,
        }
    }

    /// Current peer address (changes on migration).
    pub fn remote_address(&self) -> SocketAddr {
        self.conn.remote_address()
    }

    /// Compares the peer address with the one seen last; returns the previous address when
    /// the peer moved (connection migration or NAT rebinding).
    pub fn observe_path(&self) -> Option<SocketAddr> {
        let now = self.conn.remote_address();
        let mut path = self.path.lock();
        if *path == now {
            return None;
        }
        let old = std::mem::replace(&mut *path, now);
        self.migrations.fetch_add(1, Ordering::Relaxed);
        Some(old)
    }

    pub fn migrations(&self) -> u64 {
        self.migrations.load(Ordering::Relaxed)
    }

    /// False once the connection closed (either side, idle timeout, error).
    pub fn is_open(&self) -> bool {
        self.conn.close_reason().is_none()
    }

    /// Largest AURX packet that fits one datagram on the current path, if datagrams are usable.
    pub fn max_datagram_size(&self) -> Option<usize> {
        self.conn.max_datagram_size()
    }

    /// Queues one sealed packet as a datagram; returns `false` (and counts a drop) when the
    /// connection is gone, the packet does not fit, or the send queue had to evict.
    pub fn send(&self, packet: bytes::Bytes) -> bool {
        let evicts = self.conn.datagram_send_buffer_space() < packet.len();
        match self.conn.send_datagram(packet) {
            Ok(()) if !evicts => {
                self.sent.fetch_add(1, Ordering::Relaxed);
                aurix_metrics::QUIC_PACKETS
                    .with_label_values(&["downlink", "sent"])
                    .inc();
                true
            }
            _ => {
                self.dropped.fetch_add(1, Ordering::Relaxed);
                aurix_metrics::QUIC_PACKETS
                    .with_label_values(&["downlink", "dropped"])
                    .inc();
                false
            }
        }
    }

    /// Closes the connection with an application reason (superseded, session gone, …).
    pub fn close(&self, reason: &str) {
        self.conn
            .close(quinn::VarInt::from_u32(0), reason.as_bytes());
    }

    pub fn packets_sent(&self) -> u64 {
        self.sent.load(Ordering::Relaxed)
    }

    pub fn packets_dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }
}

impl PartialEq for QuicLink {
    fn eq(&self, other: &Self) -> bool {
        self.id == other.id
    }
}

impl Eq for QuicLink {}

/// [`quinn::AsyncUdpSocket`] over the shared media socket: sends go straight to the socket,
/// receives are the QUIC-classified datagrams the receive workers push in.
pub struct SharedQuicSocket {
    socket: Arc<MediaSocket>,
    inbound: Mutex<mpsc::Receiver<(SocketAddr, Vec<u8>)>>,
}

impl std::fmt::Debug for SharedQuicSocket {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SharedQuicSocket")
            .field("local_addr", &self.socket.local_addr())
            .finish()
    }
}

struct SharedPoller {
    socket: Arc<MediaSocket>,
}

impl std::fmt::Debug for SharedPoller {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SharedPoller").finish()
    }
}

impl quinn::UdpPoller for SharedPoller {
    fn poll_writable(self: Pin<&mut Self>, cx: &mut Context) -> Poll<std::io::Result<()>> {
        self.socket.poll_send_ready(cx)
    }
}

impl quinn::AsyncUdpSocket for SharedQuicSocket {
    fn create_io_poller(self: Arc<Self>) -> Pin<Box<dyn quinn::UdpPoller>> {
        Box::pin(SharedPoller {
            socket: self.socket.clone(),
        })
    }

    fn try_send(&self, transmit: &quinn::udp::Transmit) -> std::io::Result<()> {
        self.socket
            .try_send_to(transmit.contents, transmit.destination)
            .map(|_| ())
    }

    fn poll_recv(
        &self,
        cx: &mut Context,
        bufs: &mut [IoSliceMut<'_>],
        meta: &mut [quinn::udp::RecvMeta],
    ) -> Poll<std::io::Result<usize>> {
        let mut inbound = self.inbound.lock();
        match inbound.poll_recv(cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(None) => Poll::Ready(Err(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "media socket workers stopped",
            ))),
            Poll::Ready(Some((addr, data))) => {
                let (Some(buf), Some(meta)) = (bufs.first_mut(), meta.first_mut()) else {
                    return Poll::Ready(Ok(0));
                };
                let len = data.len().min(buf.len());
                buf[..len].copy_from_slice(&data[..len]);
                *meta = quinn::udp::RecvMeta {
                    addr,
                    len,
                    stride: len,
                    ecn: None,
                    dst_ip: None,
                };
                Poll::Ready(Ok(1))
            }
        }
    }

    fn local_addr(&self) -> std::io::Result<SocketAddr> {
        Ok(self.socket.local_addr())
    }
}

/// The node's QUIC endpoint on the media socket.
pub struct QuicServer {
    endpoint: quinn::Endpoint,
    inbound: mpsc::Sender<(SocketAddr, Vec<u8>)>,
    info: QuicInfo,
    connections: AtomicUsize,
    options: QuicOptions,
}

impl std::fmt::Debug for QuicServer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("QuicServer")
            .field("cert_sha256", &self.info.cert_sha256)
            .field("connections", &self.connections.load(Ordering::Relaxed))
            .finish()
    }
}

impl QuicServer {
    /// Creates the endpoint on `socket` with the node's media certificate (shared with the
    /// TLS tunnel, so one pin covers both).
    pub fn start(
        socket: Arc<MediaSocket>,
        options: QuicOptions,
        cert: &MediaCert,
    ) -> Result<Arc<Self>> {
        let info = QuicInfo {
            cert_sha256: cert.fingerprint().to_string(),
            server_name: cert.server_name().to_string(),
        };

        let mut tls = cert.server_config(QUIC_ALPN)?;
        // 0-RTT needs resumption state; keep enough for the connection cap (or a sane default).
        let tickets = if options.max_connections > 0 {
            options.max_connections.saturating_mul(2).max(256)
        } else {
            8192
        };
        tls.session_storage = rustls::server::ServerSessionMemoryCache::new(tickets);
        tls.max_early_data_size = if options.zero_rtt { u32::MAX } else { 0 };
        let crypto = quinn::crypto::rustls::QuicServerConfig::try_from(tls)
            .map_err(|e| AurixError::Internal(format!("QUIC crypto config: {e}")))?;
        let mut server_config = quinn::ServerConfig::with_crypto(Arc::new(crypto));
        server_config.transport_config(Arc::new(transport_config(
            options.idle_timeout,
            options.queue_packets,
            None,
        )));
        server_config.migration(options.migration);
        if options.max_connections > 0 {
            server_config.max_incoming(options.max_connections);
        }

        let (inbound, rx) = mpsc::channel(options.queue_packets.max(64) * 16);
        let shared = Arc::new(SharedQuicSocket {
            socket,
            inbound: Mutex::new(rx),
        });
        let endpoint = quinn::Endpoint::new_with_abstract_socket(
            endpoint_config(),
            Some(server_config),
            shared,
            Arc::new(quinn::TokioRuntime),
        )
        .map_err(|e| AurixError::Transport(format!("QUIC endpoint: {e}")))?;
        info!(
            "QUIC media endpoint ready (cert sha256 {}, server name {}, 0-RTT {}, migration {})",
            info.cert_sha256, info.server_name, options.zero_rtt, options.migration
        );
        Ok(Arc::new(Self {
            endpoint,
            inbound,
            info,
            connections: AtomicUsize::new(0),
            options,
        }))
    }

    /// What clients need to connect: certificate pin and server name.
    pub fn info(&self) -> &QuicInfo {
        &self.info
    }

    pub fn options(&self) -> &QuicOptions {
        &self.options
    }

    /// Open connections right now.
    pub fn connection_count(&self) -> usize {
        self.connections.load(Ordering::Relaxed)
    }

    /// Hands one QUIC-classified datagram from the media socket to the endpoint. Drops (and
    /// counts) when the ingress queue is full: a stalled endpoint must never stall the raw
    /// AURX and WebRTC traffic sharing the socket.
    pub fn feed(&self, src: SocketAddr, data: &[u8]) {
        if self.inbound.try_send((src, data.to_vec())).is_err() {
            aurix_metrics::QUIC_PACKETS
                .with_label_values(&["uplink", "dropped"])
                .inc();
        }
    }

    /// Accepts connections until the endpoint closes. Every connection gets its own task
    /// that reads datagrams and hands them to `on_datagram(link, bytes)`; `on_closed(link)`
    /// runs once the connection is gone (whatever the reason).
    pub fn run_accept_loop<D, C>(self: Arc<Self>, on_datagram: D, on_closed: C)
    where
        D: Fn(Arc<QuicLink>, bytes::Bytes) -> Pin<Box<dyn std::future::Future<Output = ()> + Send>>
            + Send
            + Sync
            + 'static,
        C: Fn(Arc<QuicLink>) + Send + Sync + 'static,
    {
        let on_datagram = Arc::new(on_datagram);
        let on_closed = Arc::new(on_closed);
        tokio::spawn(async move {
            while let Some(incoming) = self.endpoint.accept().await {
                let max = self.options.max_connections;
                if max > 0 && self.connections.load(Ordering::Relaxed) >= max {
                    aurix_metrics::QUIC_HANDSHAKES
                        .with_label_values(&["refused"])
                        .inc();
                    incoming.refuse();
                    continue;
                }
                let server = self.clone();
                let on_datagram = on_datagram.clone();
                let on_closed = on_closed.clone();
                tokio::spawn(async move {
                    server.serve(incoming, on_datagram, on_closed).await;
                });
            }
            debug!("QUIC accept loop ended");
        });
    }

    async fn serve<D, C>(&self, incoming: quinn::Incoming, on_datagram: Arc<D>, on_closed: Arc<C>)
    where
        D: Fn(Arc<QuicLink>, bytes::Bytes) -> Pin<Box<dyn std::future::Future<Output = ()> + Send>>
            + Send
            + Sync
            + 'static,
        C: Fn(Arc<QuicLink>) + Send + Sync + 'static,
    {
        let remote = incoming.remote_address();
        let connecting = match incoming.accept() {
            Ok(c) => c,
            Err(e) => {
                aurix_metrics::QUIC_HANDSHAKES
                    .with_label_values(&["failed"])
                    .inc();
                debug!("QUIC connection from {remote} not accepted: {e}");
                return;
            }
        };
        // 0-RTT: the connection becomes usable before the handshake finishes; datagrams read
        // then may be replayed, which the AURX layer tolerates (see module docs). With 0-RTT
        // off, `max_early_data_size = 0` made the peer's early data unusable and we only start
        // reading after the handshake.
        let (conn, early) = if self.options.zero_rtt {
            match connecting.into_0rtt() {
                Ok((conn, accepted)) => (conn, Some(accepted)),
                Err(connecting) => match connecting.await {
                    Ok(conn) => (conn, None),
                    Err(e) => {
                        aurix_metrics::QUIC_HANDSHAKES
                            .with_label_values(&["failed"])
                            .inc();
                        debug!("QUIC handshake with {remote} failed: {e}");
                        return;
                    }
                },
            }
        } else {
            match connecting.await {
                Ok(conn) => (conn, None),
                Err(e) => {
                    aurix_metrics::QUIC_HANDSHAKES
                        .with_label_values(&["failed"])
                        .inc();
                    debug!("QUIC handshake with {remote} failed: {e}");
                    return;
                }
            }
        };
        aurix_metrics::QUIC_HANDSHAKES
            .with_label_values(&[if early.is_some() {
                "accepted_0rtt"
            } else {
                "accepted"
            }])
            .inc();
        self.connections.fetch_add(1, Ordering::Relaxed);
        aurix_metrics::QUIC_CONNECTIONS.inc();
        let link = QuicLink::new(conn.clone());
        debug!("QUIC connection #{} from {remote}", link.id());

        loop {
            match conn.read_datagram().await {
                Ok(data) => on_datagram(link.clone(), data).await,
                Err(e) => {
                    debug!("QUIC connection #{} closed: {e}", link.id());
                    break;
                }
            }
        }
        on_closed(link);
        self.connections.fetch_sub(1, Ordering::Relaxed);
        aurix_metrics::QUIC_CONNECTIONS.dec();
    }

    /// Closes every connection and stops accepting.
    pub fn shutdown(&self) {
        self.endpoint
            .close(quinn::VarInt::from_u32(0), b"node shutting down");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn options_default_to_self_signed_media_identity() {
        let options = QuicOptions::default();
        assert!(options.cert.is_none());
        let cert = MediaCert::self_signed(&options.server_name).unwrap();
        assert_eq!(cert.fingerprint().len(), 64);
    }
}
