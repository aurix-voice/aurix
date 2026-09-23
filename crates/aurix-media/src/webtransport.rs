// SPDX-FileCopyrightText: 2025 Aurix contributors
// SPDX-License-Identifier: AGPL-3.0-only

//! AURX over WebTransport — the browser equivalent of the native QUIC path.
//!
//! A dedicated HTTP/3 endpoint (normally UDP/443, separate from the media UDP socket that
//! carries native AURX + native QUIC + WebRTC) accepts one WebTransport session per browser
//! at [`WEBTRANSPORT_PATH`]. Inside the session every QUIC datagram is one sealed AURX
//! packet, byte-for-byte what a native client sends over UDP or QUIC, so the AURX layer keeps
//! doing authentication, encryption, anti-replay and `SessionBind` ownership; the browser
//! only gains a datagram pipe with full Opus control and no SDP/ICE. A session speaks for an
//! Aurix media session only after an authenticated `SessionBind` arrived on it and never
//! changes owner; sessions that do not bind within the bind timeout or go idle are closed.
//!
//! Browsers cannot pin an arbitrary certificate the way native clients do. The endpoint runs
//! either the operator's publicly trusted certificate (Web PKI, DNS name in
//! `webtransport_advertise`) or — the default, which works for bare IPs — a node-generated
//! short-lived ECDSA certificate whose SHA-256 the browser passes as
//! `serverCertificateHashes`. Browsers only accept such certificates for ≤ 14 days, so the
//! node keeps two (current and next), advertises both hashes in
//! `SessionInitAck.webtransport.cert_sha256` and swaps to the next one before the current
//! expires; nothing here talks to a CA (no ACME).

use crate::cert::MediaCert;
use crate::transport::MEDIA_SOCKET_BUFFER_BYTES;
use aurix_common::protocol::{WebTransportInfo, MAX_PACKET_SIZE, WEBTRANSPORT_PATH};
use aurix_common::types::SessionId;
use aurix_common::{AurixError, Result};
use parking_lot::{Mutex, RwLock};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Notify;
use tracing::{debug, info, warn};
use wtransport::endpoint::endpoint_side::Server;
use wtransport::endpoint::IncomingSession;
use wtransport::error::SendDatagramError;
use wtransport::{Endpoint, ServerConfig, VarInt};

static NEXT_LINK_ID: AtomicU64 = AtomicU64::new(1);

/// WebTransport tunables taken from `MediaConfig::webtransport_*`.
#[derive(Debug, Clone)]
pub struct WebTransportOptions {
    /// Whether the node runs the endpoint at all.
    pub enabled: bool,
    /// UDP port to listen on; meant for 443 (0 lets the OS pick, which only makes sense for
    /// tests).
    pub port: u16,
    /// `host:port` endpoints advertised to browsers (empty: derived from the media
    /// addresses).
    pub advertise: Vec<String>,
    /// Operator PEM pair (publicly trusted); `None` runs the node-generated short-lived
    /// certificate.
    pub cert: Option<(PathBuf, PathBuf)>,
    /// Validity of the node-generated certificate.
    pub cert_validity: Duration,
    /// CN of the node-generated certificate (the QUIC server name).
    pub server_name: String,
    /// Downlink datagrams buffered per session, in AURX packets.
    pub queue_packets: usize,
    /// Open sessions accepted at once (0 = unlimited).
    pub max_connections: usize,
    /// How long a fresh session may stay without an authenticated `SessionBind`.
    pub bind_timeout: Duration,
    /// Idle timeout: no uplink datagram for this long closes the session (browsers
    /// heartbeat).
    pub idle_timeout: Duration,
}

impl Default for WebTransportOptions {
    fn default() -> Self {
        Self {
            enabled: false,
            port: 0,
            advertise: Vec::new(),
            cert: None,
            cert_validity: Duration::from_secs(13 * 86_400),
            server_name: "aurix-media".into(),
            queue_packets: 128,
            max_connections: 0,
            bind_timeout: Duration::from_secs(10),
            idle_timeout: Duration::from_secs(20),
        }
    }
}

/// One WebTransport session from the SFU's point of view: the downlink handle a
/// [`MediaSession`] holds while bound through it plus the bookkeeping the router needs to
/// attribute uplink datagrams.
///
/// [`MediaSession`]: crate::session::MediaSession
pub struct WebTransportLink {
    id: u64,
    conn: wtransport::Connection,
    /// Session that authenticated a `SessionBind` over this connection (`None` until then).
    session: Mutex<Option<SessionId>>,
    closed: AtomicBool,
    close_signal: Notify,
    sent: AtomicU64,
    dropped: AtomicU64,
}

impl std::fmt::Debug for WebTransportLink {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WebTransportLink")
            .field("id", &self.id)
            .field("remote", &self.conn.remote_address())
            .field("session", &*self.session.lock())
            .field("closed", &self.closed.load(Ordering::Relaxed))
            .finish()
    }
}

impl WebTransportLink {
    fn new(conn: wtransport::Connection) -> Arc<Self> {
        Arc::new(Self {
            id: NEXT_LINK_ID.fetch_add(1, Ordering::Relaxed),
            conn,
            session: Mutex::new(None),
            closed: AtomicBool::new(false),
            close_signal: Notify::new(),
            sent: AtomicU64::new(0),
            dropped: AtomicU64::new(0),
        })
    }

    /// Node-unique identity (every session gets its own link).
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
        self.conn.remote_address()
    }

    /// Smoothed QUIC round-trip estimate of the session.
    pub fn rtt(&self) -> Duration {
        self.conn.rtt()
    }

    /// False once the session closed (either side, timeout, superseded).
    pub fn is_open(&self) -> bool {
        !self.closed.load(Ordering::Acquire)
    }

    /// Sends one sealed packet as a datagram; returns `false` (and counts a drop) when the
    /// session is gone, the packet does not fit the path MTU or the bounded datagram buffer
    /// is exhausted. Datagrams are never queued into growing latency: QUIC drops the oldest
    /// buffered one when the buffer (`queue_packets` worth) is full.
    pub fn send(&self, packet: &[u8]) -> bool {
        if !self.is_open() || packet.is_empty() || packet.len() > MAX_PACKET_SIZE {
            self.count_drop();
            return false;
        }
        match self.conn.send_datagram(packet) {
            Ok(()) => {
                self.sent.fetch_add(1, Ordering::Relaxed);
                aurix_metrics::WEBTRANSPORT_PACKETS
                    .with_label_values(&["downlink", "sent"])
                    .inc();
                true
            }
            Err(SendDatagramError::NotConnected) => {
                self.closed.store(true, Ordering::Release);
                self.count_drop();
                false
            }
            Err(SendDatagramError::TooLarge | SendDatagramError::UnsupportedByPeer) => {
                self.count_drop();
                false
            }
        }
    }

    fn count_drop(&self) {
        self.dropped.fetch_add(1, Ordering::Relaxed);
        aurix_metrics::WEBTRANSPORT_PACKETS
            .with_label_values(&["downlink", "dropped"])
            .inc();
    }

    /// Closes the session (superseded, media session gone, …); idempotent.
    pub fn close(&self, reason: &str) {
        if !self.closed.swap(true, Ordering::AcqRel) {
            debug!("WebTransport #{} closing: {reason}", self.id);
            self.conn.close(VarInt::from_u32(0), reason.as_bytes());
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

impl PartialEq for WebTransportLink {
    fn eq(&self, other: &Self) -> bool {
        self.id == other.id
    }
}

impl Eq for WebTransportLink {}

type PacketHandler = dyn Fn(Arc<WebTransportLink>, bytes::Bytes) -> Pin<Box<dyn std::future::Future<Output = ()> + Send>>
    + Send
    + Sync;
type ClosedHandler = dyn Fn(Arc<WebTransportLink>) + Send + Sync;

/// The certificates the endpoint presents: the one in use and, for node-generated
/// short-lived certificates, the one it will rotate to.
struct Identity {
    current: MediaCert,
    next: Option<MediaCert>,
}

/// The node's WebTransport endpoint.
pub struct WebTransportServer {
    endpoint: Endpoint<Server>,
    local_addr: SocketAddr,
    info: RwLock<WebTransportInfo>,
    identity: Mutex<Identity>,
    san_names: Vec<String>,
    options: WebTransportOptions,
    connections: AtomicUsize,
    shutdown: Notify,
    stopped: AtomicBool,
}

impl std::fmt::Debug for WebTransportServer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WebTransportServer")
            .field("local_addr", &self.local_addr)
            .field("info", &*self.info.read())
            .field("connections", &self.connections.load(Ordering::Relaxed))
            .finish()
    }
}

impl WebTransportServer {
    /// Binds the endpoint on `bind` (the media bind address with `options.port`). Browsers are
    /// told to connect to `options.advertise`, or — when that is empty — to `fallback_hosts`
    /// (the node's advertised media addresses) with the port the endpoint actually got. The
    /// socket gets the media socket's buffers: one endpoint receives every browser's datagrams
    /// and ACKs, and the OS default (~200 KiB) overflows in the hundreds of sessions.
    pub fn bind(
        bind: SocketAddr,
        fallback_hosts: &[SocketAddr],
        options: WebTransportOptions,
    ) -> Result<Arc<Self>> {
        let (socket, _family) =
            aurix_common::net::bind_udp_std(bind, true, MEDIA_SOCKET_BUFFER_BYTES)
                .map_err(|e| AurixError::Transport(format!("WebTransport endpoint {bind}: {e}")))?;
        let local_addr = socket
            .local_addr()
            .map_err(|e| AurixError::Transport(format!("WebTransport endpoint: {e}")))?;
        let advertised: Vec<String> = if options.advertise.is_empty() {
            fallback_hosts
                .iter()
                .map(|a| SocketAddr::new(a.ip(), local_addr.port()).to_string())
                .collect()
        } else {
            options.advertise.clone()
        };
        let mut san_names: Vec<String> = advertised
            .iter()
            .filter_map(|e| aurix_common::addr::split_host_port(e))
            .map(|(host, _)| host.trim_matches(['[', ']']).to_string())
            .collect();
        if !san_names.iter().any(|n| n == &options.server_name) {
            san_names.push(options.server_name.clone());
        }
        san_names.dedup();

        let identity = match &options.cert {
            Some((cert, key)) => Identity {
                current: MediaCert::load_named(
                    cert,
                    key,
                    &options.server_name,
                    "media.webtransport",
                )?,
                next: None,
            },
            None => Identity {
                current: Self::short_lived(&options, &san_names)?,
                next: Some(Self::short_lived(&options, &san_names)?),
            },
        };
        let config = Self::build_config(Some(socket), &identity.current, &options)?;
        let endpoint = Endpoint::server(config)
            .map_err(|e| AurixError::Transport(format!("WebTransport endpoint {bind}: {e}")))?;
        let info = Self::info_for(&advertised, &identity, options.cert.is_none());
        info!(
            "WebTransport media endpoint listening on {} (advertised {:?}, cert sha256 {:?})",
            local_addr, info.urls, info.cert_sha256
        );
        Ok(Arc::new(Self {
            endpoint,
            local_addr,
            info: RwLock::new(info),
            identity: Mutex::new(identity),
            san_names,
            options,
            connections: AtomicUsize::new(0),
            shutdown: Notify::new(),
            stopped: AtomicBool::new(false),
        }))
    }

    fn short_lived(options: &WebTransportOptions, san_names: &[String]) -> Result<MediaCert> {
        MediaCert::short_lived(&options.server_name, san_names, options.cert_validity)
    }

    /// Endpoint configuration presenting `cert`: TLS 1.3 with the HTTP/3 ALPN, QUIC transport
    /// sized for one control connection plus datagrams (`queue_packets` worth of buffer each
    /// way), no keep-alives (browsers heartbeat at the AURX layer). `socket` binds a fresh
    /// endpoint; `None` builds a configuration for [`Endpoint::reload_config`].
    fn build_config(
        socket: Option<std::net::UdpSocket>,
        cert: &MediaCert,
        options: &WebTransportOptions,
    ) -> Result<ServerConfig> {
        let tls = cert.server_config(wtransport::tls::WEBTRANSPORT_ALPN)?;
        let mut transport = quinn::TransportConfig::default();
        let buffer = options.queue_packets.max(8) * MAX_PACKET_SIZE;
        transport
            .max_concurrent_bidi_streams(quinn::VarInt::from_u32(8))
            .max_concurrent_uni_streams(quinn::VarInt::from_u32(8))
            .datagram_receive_buffer_size(Some(buffer))
            .datagram_send_buffer_size(buffer)
            .keep_alive_interval(None);
        let builder = ServerConfig::builder();
        let builder = match socket {
            Some(socket) => builder.with_bind_socket(socket),
            None => builder.with_bind_default(0),
        };
        builder
            .with_custom_tls_and_transport(tls, transport)
            .max_idle_timeout(Some(options.idle_timeout.max(Duration::from_secs(1))))
            .map(|b| b.build())
            .map_err(|_| {
                AurixError::InvalidConfiguration(
                    "media.webtransport idle timeout out of range".into(),
                )
            })
    }

    fn info_for(advertised: &[String], identity: &Identity, pinned: bool) -> WebTransportInfo {
        let cert_sha256 = if pinned {
            std::iter::once(&identity.current)
                .chain(identity.next.as_ref())
                .map(|c| c.fingerprint().to_string())
                .collect()
        } else {
            Vec::new()
        };
        WebTransportInfo {
            urls: advertised
                .iter()
                .map(|e| format!("https://{e}{WEBTRANSPORT_PATH}"))
                .collect(),
            cert_sha256,
        }
    }

    /// What browsers need to connect: URLs and, for node-generated certificates, the hashes
    /// to pin (current and next).
    pub fn info(&self) -> WebTransportInfo {
        self.info.read().clone()
    }

    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    pub fn options(&self) -> &WebTransportOptions {
        &self.options
    }

    /// Open sessions right now.
    pub fn connection_count(&self) -> usize {
        self.connections.load(Ordering::Relaxed)
    }

    /// Interval at which the node-generated certificate is swapped for the next one: well
    /// inside the validity, so the "next" hash a browser cached from `SessionInitAck` is
    /// still valid when it becomes current and the current one never gets near expiry.
    pub fn rotation_interval(&self) -> Duration {
        self.options.cert_validity.mul_f64(0.45)
    }

    /// Makes `next` the presented certificate, generates a fresh `next` and re-advertises.
    /// Existing sessions are unaffected (the certificate only matters at the handshake).
    /// No-op for operator certificates.
    pub fn rotate_certificate(&self) -> Result<bool> {
        let mut identity = self.identity.lock();
        let Some(next) = identity.next.take() else {
            return Ok(false);
        };
        let result = Self::build_config(None, &next, &self.options).and_then(|config| {
            self.endpoint.reload_config(config, false).map_err(|e| {
                AurixError::Internal(format!("WebTransport certificate rotation: {e}"))
            })
        });
        if let Err(e) = result {
            identity.next = Some(next);
            aurix_metrics::WEBTRANSPORT_CERT_ROTATIONS
                .with_label_values(&["failed"])
                .inc();
            return Err(e);
        }
        identity.current = next;
        identity.next = Some(Self::short_lived(&self.options, &self.san_names)?);
        let mut info = self.info.write();
        let urls = std::mem::take(&mut info.urls);
        let mut refreshed = Self::info_for(&[], &identity, true);
        refreshed.urls = urls;
        *info = refreshed;
        aurix_metrics::WEBTRANSPORT_CERT_ROTATIONS
            .with_label_values(&["rotated"])
            .inc();
        info!(
            "WebTransport certificate rotated (cert sha256 {:?})",
            info.cert_sha256
        );
        Ok(true)
    }

    /// Accepts sessions until [`Self::shutdown`]. Every session gets its own task that reads
    /// datagrams and hands each to `on_packet(link, bytes)`; `on_closed(link)` runs once the
    /// session is gone (whatever the reason). Node-generated certificates are rotated on a
    /// timer from here as well.
    pub fn run_accept_loop<P, C>(self: Arc<Self>, on_packet: P, on_closed: C)
    where
        P: Fn(
                Arc<WebTransportLink>,
                bytes::Bytes,
            ) -> Pin<Box<dyn std::future::Future<Output = ()> + Send>>
            + Send
            + Sync
            + 'static,
        C: Fn(Arc<WebTransportLink>) + Send + Sync + 'static,
    {
        let on_packet: Arc<PacketHandler> = Arc::new(on_packet);
        let on_closed: Arc<ClosedHandler> = Arc::new(on_closed);
        if self.options.cert.is_none() {
            let server = self.clone();
            tokio::spawn(async move {
                let period = server.rotation_interval();
                loop {
                    tokio::select! {
                        _ = tokio::time::sleep(period) => {}
                        _ = server.shutdown.notified() => break,
                    }
                    if server.stopped.load(Ordering::Acquire) {
                        break;
                    }
                    if let Err(e) = server.rotate_certificate() {
                        warn!("WebTransport certificate rotation failed: {e}");
                    }
                }
            });
        }
        tokio::spawn(async move {
            loop {
                let incoming = tokio::select! {
                    incoming = self.endpoint.accept() => incoming,
                    _ = self.shutdown.notified() => break,
                };
                let max = self.options.max_connections;
                if max > 0 && self.connections.load(Ordering::Relaxed) >= max {
                    aurix_metrics::WEBTRANSPORT_HANDSHAKES
                        .with_label_values(&["refused"])
                        .inc();
                    incoming.refuse();
                    continue;
                }
                let server = self.clone();
                let on_packet = on_packet.clone();
                let on_closed = on_closed.clone();
                tokio::spawn(async move {
                    server.serve(incoming, on_packet, on_closed).await;
                });
            }
            debug!("WebTransport accept loop ended");
        });
    }

    async fn serve(
        &self,
        incoming: IncomingSession,
        on_packet: Arc<PacketHandler>,
        on_closed: Arc<ClosedHandler>,
    ) {
        let remote = incoming.remote_address();
        let failed = |what: &str| {
            aurix_metrics::WEBTRANSPORT_HANDSHAKES
                .with_label_values(&["failed"])
                .inc();
            debug!("WebTransport handshake with {remote} {what}");
        };
        let request = match tokio::time::timeout(self.options.bind_timeout, incoming).await {
            Ok(Ok(request)) => request,
            Ok(Err(e)) => return failed(&format!("failed: {e}")),
            Err(_) => return failed("timed out"),
        };
        let path = request.path();
        let path = path.split('?').next().unwrap_or(path);
        if path != WEBTRANSPORT_PATH {
            aurix_metrics::WEBTRANSPORT_HANDSHAKES
                .with_label_values(&["not_found"])
                .inc();
            debug!("WebTransport request from {remote} for unknown path {path:?}");
            request.not_found().await;
            return;
        }
        let conn = match tokio::time::timeout(self.options.bind_timeout, request.accept()).await {
            Ok(Ok(conn)) => conn,
            Ok(Err(e)) => return failed(&format!("failed: {e}")),
            Err(_) => return failed("timed out"),
        };
        aurix_metrics::WEBTRANSPORT_HANDSHAKES
            .with_label_values(&["accepted"])
            .inc();
        self.connections.fetch_add(1, Ordering::Relaxed);
        aurix_metrics::WEBTRANSPORT_CONNECTIONS.inc();

        let link = WebTransportLink::new(conn);
        debug!("WebTransport session #{} from {remote}", link.id());
        let bind_deadline = tokio::time::Instant::now() + self.options.bind_timeout;
        let mut last_uplink = tokio::time::Instant::now();
        let reason = loop {
            let deadline = if link.session_id().is_none() {
                bind_deadline.min(last_uplink + self.options.idle_timeout)
            } else {
                last_uplink + self.options.idle_timeout
            };
            let datagram = tokio::select! {
                d = link.conn.receive_datagram() => d,
                _ = link.close_signal.notified() => break "closed",
                _ = tokio::time::sleep_until(deadline) => {
                    if link.session_id().is_none() {
                        aurix_metrics::WEBTRANSPORT_HANDSHAKES
                            .with_label_values(&["unbound"])
                            .inc();
                        break "no SessionBind within the bind timeout";
                    }
                    break "idle timeout";
                }
            };
            let datagram = match datagram {
                Ok(d) => d,
                Err(e) => {
                    debug!("WebTransport #{} ended by peer: {e}", link.id());
                    break "connection closed";
                }
            };
            last_uplink = tokio::time::Instant::now();
            let payload = datagram.payload();
            if payload.is_empty() || payload.len() > MAX_PACKET_SIZE {
                aurix_metrics::WEBTRANSPORT_PACKETS
                    .with_label_values(&["uplink", "malformed"])
                    .inc();
                continue;
            }
            on_packet(link.clone(), payload).await;
            if link.closed.load(Ordering::Acquire) {
                break "closed";
            }
        };
        debug!("WebTransport session #{} ended: {reason}", link.id());
        link.close(reason);
        on_closed(link);
        self.connections.fetch_sub(1, Ordering::Relaxed);
        aurix_metrics::WEBTRANSPORT_CONNECTIONS.dec();
    }

    /// Stops accepting and closes the endpoint; open sessions end with it.
    pub fn shutdown(&self) {
        if !self.stopped.swap(true, Ordering::AcqRel) {
            self.shutdown.notify_waiters();
            self.shutdown.notify_one();
            self.endpoint.close(VarInt::from_u32(0), b"shutdown");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn options() -> WebTransportOptions {
        WebTransportOptions {
            enabled: true,
            cert_validity: Duration::from_secs(2 * 86_400),
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn binds_with_two_pinned_hashes_and_rotates() {
        let server = WebTransportServer::bind(
            "127.0.0.1:0".parse().unwrap(),
            &["127.0.0.1:0".parse().unwrap()],
            options(),
        )
        .unwrap();
        let info = server.info();
        assert_eq!(
            info.urls,
            vec![format!(
                "https://127.0.0.1:{}/aurix",
                server.local_addr().port()
            )]
        );
        assert_eq!(info.cert_sha256.len(), 2);
        assert_ne!(info.cert_sha256[0], info.cert_sha256[1]);
        let previous_next = info.cert_sha256[1].clone();
        assert!(server.rotate_certificate().unwrap());
        let rotated = server.info();
        assert_eq!(rotated.urls, info.urls);
        assert_eq!(rotated.cert_sha256[0], previous_next);
        assert_ne!(rotated.cert_sha256[1], previous_next);
        assert!(server.rotation_interval() < server.options().cert_validity / 2);
        server.shutdown();
    }

    #[tokio::test]
    async fn operator_certificate_is_not_pinned_and_not_rotated() {
        let generated = rcgen::generate_simple_self_signed(vec!["voice.example".into()]).unwrap();
        let dir = std::env::temp_dir().join(format!("aurix-wt-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let cert_path = dir.join("cert.pem");
        let key_path = dir.join("key.pem");
        std::fs::write(&cert_path, generated.cert.pem()).unwrap();
        std::fs::write(&key_path, generated.signing_key.serialize_pem()).unwrap();
        let server = WebTransportServer::bind(
            "127.0.0.1:0".parse().unwrap(),
            &[],
            WebTransportOptions {
                cert: Some((cert_path, key_path)),
                advertise: vec!["voice.example:443".into()],
                ..options()
            },
        )
        .unwrap();
        let info = server.info();
        assert_eq!(
            info.urls,
            vec!["https://voice.example:443/aurix".to_string()]
        );
        assert!(info.cert_sha256.is_empty());
        assert!(!server.rotate_certificate().unwrap());
        server.shutdown();
        let _ = std::fs::remove_dir_all(dir);
    }
}
