// SPDX-FileCopyrightText: 2025 Aurix contributors
// SPDX-License-Identifier: AGPL-3.0-only

//! AURX over a dedicated TLS/TCP listener — the media path for native clients behind
//! firewalls that only let TCP 443 through.
//!
//! The listener is separate from the media UDP socket and from the control WebSocket: it
//! speaks TLS 1.3 (rustls, ALPN `aurix-tunnel/1`) with the same certificate as the QUIC
//! endpoint, so the pin a client already holds (`SessionInitAck.tls_tunnel.cert_sha256`)
//! covers both. Inside the stream every sealed AURX packet is one
//! `u16 big-endian length | packet` frame ([`aurix_common::framing`]); the AURX layer keeps
//! doing authentication, encryption, anti-replay and `SessionBind` ownership exactly as on
//! UDP — TLS only hides the traffic from middleboxes and gets it through port 443.
//!
//! A connection speaks for a session only after an authenticated `SessionBind` arrived on it
//! and never changes owner; connections that do not bind within the bind timeout, violate
//! framing (empty, oversized, truncated frame at EOF) or overflow their downlink queue are
//! closed.

use crate::cert::MediaCert;
use aurix_common::framing::{encode_frame, FrameDecoder, FrameError, MAX_FRAME};
use aurix_common::protocol::{TlsTunnelInfo, TLS_TUNNEL_ALPN};
use aurix_common::types::SessionId;
use aurix_common::{AurixError, Result};
use parking_lot::Mutex;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, Notify};
use tokio_rustls::TlsAcceptor;
use tracing::{debug, info, warn};

static NEXT_LINK_ID: AtomicU64 = AtomicU64::new(1);

/// TLS tunnel tunables taken from `MediaConfig::tls_tunnel_*`.
#[derive(Debug, Clone)]
pub struct TlsTunnelOptions {
    /// Whether the node runs the listener at all.
    pub enabled: bool,
    /// TCP port to listen on; meant for 443 or a TLS-passthrough proxy (0 lets the OS pick,
    /// which only makes sense for tests).
    pub port: u16,
    /// `host:port` endpoints advertised to clients (empty: derived from the media addresses).
    pub advertise: Vec<String>,
    /// Downlink queue depth per connection, in AURX packets.
    pub queue_packets: usize,
    /// Open connections accepted at once (0 = unlimited).
    pub max_connections: usize,
    /// How long a fresh connection may stay without an authenticated `SessionBind`.
    pub bind_timeout: Duration,
    /// Idle timeout: no uplink frame for this long closes the connection (clients heartbeat).
    pub idle_timeout: Duration,
}

impl Default for TlsTunnelOptions {
    fn default() -> Self {
        Self {
            enabled: false,
            port: 0,
            advertise: Vec::new(),
            queue_packets: 128,
            max_connections: 0,
            bind_timeout: Duration::from_secs(10),
            idle_timeout: Duration::from_secs(20),
        }
    }
}

/// One TLS tunnel connection from the SFU's point of view: the downlink handle a
/// [`MediaSession`] holds while bound through it plus the bookkeeping the router needs to
/// attribute uplink frames.
///
/// [`MediaSession`]: crate::session::MediaSession
#[derive(Debug)]
pub struct TlsLink {
    id: u64,
    remote: SocketAddr,
    /// Session that authenticated a `SessionBind` over this connection (`None` until then).
    session: Mutex<Option<SessionId>>,
    tx: mpsc::Sender<Vec<u8>>,
    closed: AtomicBool,
    close_signal: Notify,
    sent: AtomicU64,
    dropped: AtomicU64,
}

impl TlsLink {
    fn new(remote: SocketAddr, queue_packets: usize) -> (Arc<Self>, mpsc::Receiver<Vec<u8>>) {
        let (tx, rx) = mpsc::channel(queue_packets.max(8));
        let link = Arc::new(Self {
            id: NEXT_LINK_ID.fetch_add(1, Ordering::Relaxed),
            remote,
            session: Mutex::new(None),
            tx,
            closed: AtomicBool::new(false),
            close_signal: Notify::new(),
            sent: AtomicU64::new(0),
            dropped: AtomicU64::new(0),
        });
        (link, rx)
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

    pub fn remote_address(&self) -> SocketAddr {
        self.remote
    }

    /// False once the connection closed (either side, framing violation, timeout).
    pub fn is_open(&self) -> bool {
        !self.closed.load(Ordering::Acquire)
    }

    /// Queues one sealed packet for the writer task; returns `false` (and counts a drop) when
    /// the connection is gone or the bounded downlink queue is full. A full queue means the
    /// TCP path cannot keep up with real time; the packet is dropped rather than buffered
    /// into ever-growing latency.
    pub fn send(&self, packet: Vec<u8>) -> bool {
        if !self.is_open() || packet.is_empty() || packet.len() > MAX_FRAME {
            self.count_drop();
            return false;
        }
        match self.tx.try_send(packet) {
            Ok(()) => {
                self.sent.fetch_add(1, Ordering::Relaxed);
                aurix_metrics::TLS_TUNNEL_PACKETS
                    .with_label_values(&["downlink", "sent"])
                    .inc();
                true
            }
            Err(_) => {
                self.count_drop();
                false
            }
        }
    }

    fn count_drop(&self) {
        self.dropped.fetch_add(1, Ordering::Relaxed);
        aurix_metrics::TLS_TUNNEL_PACKETS
            .with_label_values(&["downlink", "dropped"])
            .inc();
    }

    /// Closes the connection (superseded, session gone, …); idempotent.
    pub fn close(&self, reason: &str) {
        if !self.closed.swap(true, Ordering::AcqRel) {
            debug!("TLS tunnel #{} closing: {reason}", self.id);
            self.close_signal.notify_waiters();
            self.close_signal.notify_one();
        }
    }

    pub fn packets_sent(&self) -> u64 {
        self.sent.load(Ordering::Relaxed)
    }

    pub fn packets_dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }
}

impl PartialEq for TlsLink {
    fn eq(&self, other: &Self) -> bool {
        self.id == other.id
    }
}

impl Eq for TlsLink {}

type PacketHandler = dyn Fn(Arc<TlsLink>, Vec<u8>) -> Pin<Box<dyn std::future::Future<Output = ()> + Send>>
    + Send
    + Sync;
type ClosedHandler = dyn Fn(Arc<TlsLink>) + Send + Sync;

/// The node's TLS tunnel listener.
pub struct TlsTunnelServer {
    listener: Mutex<Option<TcpListener>>,
    local_addr: SocketAddr,
    acceptor: TlsAcceptor,
    info: TlsTunnelInfo,
    options: TlsTunnelOptions,
    connections: AtomicUsize,
    shutdown: Notify,
    stopped: AtomicBool,
}

impl std::fmt::Debug for TlsTunnelServer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TlsTunnelServer")
            .field("local_addr", &self.local_addr)
            .field("cert_sha256", &self.info.cert_sha256)
            .field("connections", &self.connections.load(Ordering::Relaxed))
            .finish()
    }
}

impl TlsTunnelServer {
    /// Binds the listener on `bind` (the media bind address with `options.port`) and prepares
    /// the TLS acceptor with the node's media certificate. Clients are told to connect to
    /// `options.advertise`, or — when that is empty — to `fallback_hosts` (the node's
    /// advertised media addresses) with the port the listener actually got.
    pub async fn bind(
        bind: SocketAddr,
        fallback_hosts: &[SocketAddr],
        options: TlsTunnelOptions,
        cert: &MediaCert,
    ) -> Result<Arc<Self>> {
        let tls = cert.server_config(TLS_TUNNEL_ALPN)?;
        let listener = TcpListener::bind(bind)
            .await
            .map_err(|e| AurixError::Transport(format!("TLS tunnel listener {bind}: {e}")))?;
        let local_addr = listener
            .local_addr()
            .map_err(|e| AurixError::Transport(format!("TLS tunnel listener: {e}")))?;
        let advertised = if options.advertise.is_empty() {
            fallback_hosts
                .iter()
                .map(|a| SocketAddr::new(a.ip(), local_addr.port()).to_string())
                .collect()
        } else {
            options.advertise.clone()
        };
        let info = TlsTunnelInfo {
            addrs: advertised,
            cert_sha256: cert.fingerprint().to_string(),
            server_name: cert.server_name().to_string(),
        };
        info!(
            "TLS media tunnel listening on {} (advertised {:?}, cert sha256 {})",
            local_addr, info.addrs, info.cert_sha256
        );
        Ok(Arc::new(Self {
            listener: Mutex::new(Some(listener)),
            local_addr,
            acceptor: TlsAcceptor::from(Arc::new(tls)),
            info,
            options,
            connections: AtomicUsize::new(0),
            shutdown: Notify::new(),
            stopped: AtomicBool::new(false),
        }))
    }

    /// What clients need to connect: endpoints, certificate pin and server name.
    pub fn info(&self) -> &TlsTunnelInfo {
        &self.info
    }

    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    pub fn options(&self) -> &TlsTunnelOptions {
        &self.options
    }

    /// Open connections right now.
    pub fn connection_count(&self) -> usize {
        self.connections.load(Ordering::Relaxed)
    }

    /// Accepts connections until [`Self::shutdown`]. Every connection gets its own task that
    /// reads frames and hands each packet to `on_packet(link, bytes)`; `on_closed(link)` runs
    /// once the connection is gone (whatever the reason).
    pub fn run_accept_loop<P, C>(self: Arc<Self>, on_packet: P, on_closed: C)
    where
        P: Fn(Arc<TlsLink>, Vec<u8>) -> Pin<Box<dyn std::future::Future<Output = ()> + Send>>
            + Send
            + Sync
            + 'static,
        C: Fn(Arc<TlsLink>) + Send + Sync + 'static,
    {
        let Some(listener) = self.listener.lock().take() else {
            return;
        };
        let on_packet: Arc<PacketHandler> = Arc::new(on_packet);
        let on_closed: Arc<ClosedHandler> = Arc::new(on_closed);
        tokio::spawn(async move {
            loop {
                let accepted = tokio::select! {
                    res = listener.accept() => res,
                    _ = self.shutdown.notified() => break,
                };
                let (stream, remote) = match accepted {
                    Ok(pair) => pair,
                    Err(e) => {
                        warn!("TLS tunnel accept error: {e}");
                        tokio::time::sleep(Duration::from_millis(50)).await;
                        continue;
                    }
                };
                let max = self.options.max_connections;
                if max > 0 && self.connections.load(Ordering::Relaxed) >= max {
                    aurix_metrics::TLS_TUNNEL_HANDSHAKES
                        .with_label_values(&["refused"])
                        .inc();
                    drop(stream);
                    continue;
                }
                let server = self.clone();
                let on_packet = on_packet.clone();
                let on_closed = on_closed.clone();
                tokio::spawn(async move {
                    server.serve(stream, remote, on_packet, on_closed).await;
                });
            }
            debug!("TLS tunnel accept loop ended");
        });
    }

    async fn serve(
        &self,
        stream: TcpStream,
        remote: SocketAddr,
        on_packet: Arc<PacketHandler>,
        on_closed: Arc<ClosedHandler>,
    ) {
        let _ = stream.set_nodelay(true);
        let tls =
            match tokio::time::timeout(self.options.bind_timeout, self.acceptor.accept(stream))
                .await
            {
                Ok(Ok(tls)) => tls,
                Ok(Err(e)) => {
                    aurix_metrics::TLS_TUNNEL_HANDSHAKES
                        .with_label_values(&["failed"])
                        .inc();
                    debug!("TLS tunnel handshake with {remote} failed: {e}");
                    return;
                }
                Err(_) => {
                    aurix_metrics::TLS_TUNNEL_HANDSHAKES
                        .with_label_values(&["failed"])
                        .inc();
                    debug!("TLS tunnel handshake with {remote} timed out");
                    return;
                }
            };
        if tls.get_ref().1.alpn_protocol() != Some(TLS_TUNNEL_ALPN) {
            aurix_metrics::TLS_TUNNEL_HANDSHAKES
                .with_label_values(&["failed"])
                .inc();
            debug!("TLS tunnel connection from {remote} negotiated no/unexpected ALPN");
            return;
        }
        aurix_metrics::TLS_TUNNEL_HANDSHAKES
            .with_label_values(&["accepted"])
            .inc();
        self.connections.fetch_add(1, Ordering::Relaxed);
        aurix_metrics::TLS_TUNNEL_CONNECTIONS.inc();

        let (link, mut outbound) = TlsLink::new(remote, self.options.queue_packets);
        debug!("TLS tunnel connection #{} from {remote}", link.id());
        let (mut reader, mut writer) = tokio::io::split(tls);

        let writer_link = link.clone();
        let writer_task = tokio::spawn(async move {
            loop {
                let packet = tokio::select! {
                    p = outbound.recv() => p,
                    _ = writer_link.close_signal.notified() => None,
                };
                let Some(packet) = packet else {
                    break;
                };
                if writer_link.closed.load(Ordering::Acquire) {
                    break;
                }
                let Some(frame) = encode_frame(&packet) else {
                    continue;
                };
                if writer.write_all(&frame).await.is_err() {
                    break;
                }
            }
            let _ = writer.shutdown().await;
        });

        let mut decoder = FrameDecoder::new();
        let mut buf = vec![0u8; MAX_FRAME * 2];
        let bind_deadline = tokio::time::Instant::now() + self.options.bind_timeout;
        let mut last_uplink = tokio::time::Instant::now();
        let reason = loop {
            let deadline = if link.session_id().is_none() {
                bind_deadline.min(last_uplink + self.options.idle_timeout)
            } else {
                last_uplink + self.options.idle_timeout
            };
            let read = tokio::select! {
                r = reader.read(&mut buf) => r,
                _ = link.close_signal.notified() => break "closed",
                _ = tokio::time::sleep_until(deadline) => {
                    if link.session_id().is_none() {
                        aurix_metrics::TLS_TUNNEL_HANDSHAKES
                            .with_label_values(&["unbound"])
                            .inc();
                        break "no SessionBind within the bind timeout";
                    }
                    break "idle timeout";
                }
            };
            let n = match read {
                Ok(0) => {
                    if decoder.pending() > 0 {
                        aurix_metrics::TLS_TUNNEL_PACKETS
                            .with_label_values(&["uplink", "malformed"])
                            .inc();
                        break "EOF inside a frame";
                    }
                    break "EOF";
                }
                Ok(n) => n,
                Err(e) => {
                    debug!("TLS tunnel #{} read error: {e}", link.id());
                    break "read error";
                }
            };
            last_uplink = tokio::time::Instant::now();
            decoder.push(&buf[..n]);
            let mut violation = None;
            loop {
                match decoder.next_frame() {
                    Ok(Some(packet)) => on_packet(link.clone(), packet).await,
                    Ok(None) => break,
                    Err(FrameError::Empty) => {
                        violation = Some("empty frame");
                        break;
                    }
                    Err(FrameError::Oversized(len)) => {
                        debug!("TLS tunnel #{} oversized frame ({len} bytes)", link.id());
                        violation = Some("oversized frame");
                        break;
                    }
                }
            }
            if let Some(v) = violation {
                aurix_metrics::TLS_TUNNEL_PACKETS
                    .with_label_values(&["uplink", "malformed"])
                    .inc();
                break v;
            }
            if link.closed.load(Ordering::Acquire) {
                break "closed";
            }
        };
        debug!("TLS tunnel connection #{} ended: {reason}", link.id());
        link.close(reason);
        writer_task.abort();
        on_closed(link);
        self.connections.fetch_sub(1, Ordering::Relaxed);
        aurix_metrics::TLS_TUNNEL_CONNECTIONS.dec();
    }

    /// Stops accepting; open connections are closed by their sessions' teardown or their
    /// idle timeouts.
    pub fn shutdown(&self) {
        if !self.stopped.swap(true, Ordering::AcqRel) {
            self.shutdown.notify_waiters();
            self.shutdown.notify_one();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn link_send_respects_queue_bound_and_close() {
        let (link, mut rx) = TlsLink::new("127.0.0.1:443".parse().unwrap(), 8);
        for _ in 0..8 {
            assert!(link.send(vec![1, 2, 3]));
        }
        assert!(!link.send(vec![4]), "ninth packet must be dropped");
        assert_eq!(link.packets_sent(), 8);
        assert_eq!(link.packets_dropped(), 1);
        assert!(!link.send(Vec::new()), "empty packets never go out");
        assert!(
            !link.send(vec![0; MAX_FRAME + 1]),
            "oversized never goes out"
        );
        assert_eq!(rx.try_recv().unwrap(), vec![1, 2, 3]);
        link.close("test");
        assert!(!link.is_open());
        assert!(!link.send(vec![9]));
    }

    #[test]
    fn link_claim_is_sticky() {
        let (link, _rx) = TlsLink::new("127.0.0.1:443".parse().unwrap(), 8);
        let a = SessionId::new();
        let b = SessionId::new();
        assert!(link.session_id().is_none());
        assert!(link.claim(a));
        assert!(link.claim(a));
        assert!(!link.claim(b));
        assert_eq!(link.session_id(), Some(a));
    }
}
