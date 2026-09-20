//! Client address resolution behind reverse proxies, the SSRF guard shared by every
//! operator-supplied outbound URL (webhooks, live audio push streams), and dual-stack socket
//! binding for the media/TURN listeners.

use std::net::{IpAddr, SocketAddr};

use url::{Host, Url};

use crate::error::AurixError;

pub const MAX_OUTBOUND_URL_LEN: usize = 2048;

/// How a listener was bound: which address families it accepts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BoundFamily {
    V4,
    V6Only,
    /// IPv6 socket on `::` with `IPV6_V6ONLY` off: IPv4 peers appear as `::ffff:a.b.c.d`.
    DualStack,
}

impl BoundFamily {
    pub fn accepts_v4(self) -> bool {
        !matches!(self, Self::V6Only)
    }
    pub fn accepts_v6(self) -> bool {
        !matches!(self, Self::V4)
    }
    /// Whether `send_to` on this socket needs IPv4-mapped destinations (see [`crate::addr::wire`]).
    pub fn socket_is_v6(self) -> bool {
        !matches!(self, Self::V4)
    }
}

/// Parse `host:port` for a bind address. Host names other than `localhost` are refused: bind
/// addresses must be literal so the family is unambiguous.
pub fn parse_bind_addr(host: &str, port: u16) -> std::io::Result<SocketAddr> {
    let h = host.trim().trim_start_matches('[').trim_end_matches(']');
    let ip: IpAddr = match h {
        "" => IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED),
        "localhost" => IpAddr::V4(std::net::Ipv4Addr::LOCALHOST),
        other => other.parse().map_err(|_| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!(
                    "bind address `{host}` must be an IP literal (0.0.0.0, ::, 203.0.113.7, ...)"
                ),
            )
        })?,
    };
    Ok(SocketAddr::new(ip, port))
}

fn dual_stack_socket(
    addr: SocketAddr,
    ty: socket2::Type,
    dual_stack: bool,
) -> std::io::Result<(socket2::Socket, BoundFamily)> {
    let domain = if addr.is_ipv6() {
        socket2::Domain::IPV6
    } else {
        socket2::Domain::IPV4
    };
    let v6_hint = if addr.is_ipv6() {
        " (is IPv6 enabled on this host? bind to 0.0.0.0 for IPv4-only)"
    } else {
        ""
    };
    let socket = socket2::Socket::new(domain, ty, None)
        .map_err(|e| std::io::Error::new(e.kind(), format!("socket({addr}): {e}{v6_hint}")))?;
    let family = if addr.is_ipv6() {
        // Only a wildcard IPv6 bind can accept IPv4 peers; a specific IPv6 address is v6-only.
        let v6_only = !(dual_stack && addr.ip().is_unspecified());
        socket
            .set_only_v6(v6_only)
            .map_err(|e| std::io::Error::new(e.kind(), format!("IPV6_V6ONLY on {addr}: {e}")))?;
        if v6_only {
            BoundFamily::V6Only
        } else {
            BoundFamily::DualStack
        }
    } else {
        BoundFamily::V4
    };
    if ty == socket2::Type::STREAM {
        socket.set_reuse_address(true)?;
    }
    socket.set_nonblocking(true)?;
    socket
        .bind(&addr.into())
        .map_err(|e| std::io::Error::new(e.kind(), format!("bind {addr}: {e}{v6_hint}")))?;
    Ok((socket, family))
}

/// Bind a UDP socket. `::` with `dual_stack` accepts both families on one socket; the caller
/// canonicalises peer addresses with [`crate::addr::canonical`] and maps destinations with
/// [`crate::addr::wire`] using the returned family. `buffer_bytes` sizes SO_RCVBUF/SO_SNDBUF
/// (0 keeps the OS default).
pub fn bind_udp(
    addr: SocketAddr,
    dual_stack: bool,
    buffer_bytes: usize,
) -> std::io::Result<(tokio::net::UdpSocket, BoundFamily)> {
    let (socket, family) = dual_stack_socket(addr, socket2::Type::DGRAM, dual_stack)?;
    if buffer_bytes > 0 {
        // Best effort: the kernel clamps to its own maximum.
        let _ = socket.set_recv_buffer_size(buffer_bytes);
        let _ = socket.set_send_buffer_size(buffer_bytes);
    }
    let std_socket: std::net::UdpSocket = socket.into();
    Ok((tokio::net::UdpSocket::from_std(std_socket)?, family))
}

/// Bind a TCP listener with the same family semantics as [`bind_udp`].
pub fn bind_tcp(
    addr: SocketAddr,
    dual_stack: bool,
) -> std::io::Result<(tokio::net::TcpListener, BoundFamily)> {
    let (socket, family) = dual_stack_socket(addr, socket2::Type::STREAM, dual_stack)?;
    socket.listen(1024)?;
    let std_listener: std::net::TcpListener = socket.into();
    Ok((tokio::net::TcpListener::from_std(std_listener)?, family))
}

/// A UDP socket that hides its address family from callers: peers are reported in canonical
/// form (real IPv4 for IPv4-mapped addresses) and destinations are converted back to the
/// socket's own family on send, so protocol code never sees `::ffff:` addresses and can treat
/// IPv4-only, IPv6-only and dual-stack sockets alike.
pub struct FamilyUdpSocket {
    socket: tokio::net::UdpSocket,
    family: BoundFamily,
    local_addr: SocketAddr,
}

impl FamilyUdpSocket {
    /// Bind with [`bind_udp`] semantics (`dual_stack` applies to wildcard IPv6 binds).
    pub fn bind(
        bind_addr: SocketAddr,
        dual_stack: bool,
        buffer_bytes: usize,
    ) -> std::io::Result<std::sync::Arc<Self>> {
        let (socket, family) = bind_udp(bind_addr, dual_stack, buffer_bytes)?;
        Self::from_parts(socket, family)
    }

    /// Wrap an already bound socket whose family is known.
    pub fn from_parts(
        socket: tokio::net::UdpSocket,
        family: BoundFamily,
    ) -> std::io::Result<std::sync::Arc<Self>> {
        let local_addr = socket.local_addr()?;
        Ok(std::sync::Arc::new(Self {
            socket,
            family,
            local_addr,
        }))
    }

    pub fn family(&self) -> BoundFamily {
        self.family
    }

    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    /// Whether a canonical peer address can be reached from this socket at all.
    pub fn can_reach(&self, addr: SocketAddr) -> bool {
        if addr.is_ipv4() {
            self.family.accepts_v4()
        } else {
            self.family.accepts_v6()
        }
    }

    pub async fn recv_from(&self, buf: &mut [u8]) -> std::io::Result<(usize, SocketAddr)> {
        let (n, from) = self.socket.recv_from(buf).await?;
        Ok((n, crate::addr::canonical(from)))
    }

    pub async fn send_to(&self, buf: &[u8], target: SocketAddr) -> std::io::Result<usize> {
        self.socket
            .send_to(buf, crate::addr::wire(target, self.family.socket_is_v6()))
            .await
    }

    pub fn try_send_to(&self, buf: &[u8], target: SocketAddr) -> std::io::Result<usize> {
        self.socket
            .try_send_to(buf, crate::addr::wire(target, self.family.socket_is_v6()))
    }
}

/// Addresses an operator-supplied URL must never reach unless private targets are explicitly
/// allowed: RFC 1918, loopback, link-local, unspecified/broadcast, documentation, carrier NAT
/// and IPv4-mapped IPv6 forms of the same.
pub fn is_private_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            v4.is_private()
                || v4.is_loopback()
                || v4.is_link_local()
                || v4.is_unspecified()
                || v4.is_broadcast()
                || v4.is_documentation()
                // 100.64.0.0/10 (carrier NAT) and 169.254.169.254-style metadata ranges.
                || (v4.octets()[0] == 100 && (v4.octets()[1] & 0xC0) == 64)
                || v4.octets()[0] == 0
        }
        IpAddr::V6(v6) => {
            v6.is_loopback()
                || v6.is_unspecified()
                // fc00::/7 unique local, fe80::/10 link local
                || (v6.segments()[0] & 0xfe00) == 0xfc00
                || (v6.segments()[0] & 0xffc0) == 0xfe80
                || v6.to_ipv4_mapped().is_some_and(|v4| is_private_ip(IpAddr::V4(v4)))
        }
    }
}

/// Syntactic checks plus the SSRF guard on literal IP hosts for an outbound URL. `tls_scheme` /
/// `plain_scheme` are e.g. `("https", "http")` or `("wss", "ws")`; the plain one is refused when
/// `require_tls`. Host names are resolved (and pinned) by the caller at connect time via
/// [`resolve_outbound`], so a name that later points at a private range is refused then.
pub fn validate_outbound_url(
    raw: &str,
    tls_scheme: &str,
    plain_scheme: &str,
    require_tls: bool,
    allow_private: bool,
    setting: &str,
) -> Result<Url, AurixError> {
    if raw.len() > MAX_OUTBOUND_URL_LEN {
        return Err(AurixError::Validation("url is too long".into()));
    }
    let url = Url::parse(raw).map_err(|e| AurixError::Validation(format!("url: {e}")))?;
    let scheme = url.scheme();
    if scheme == plain_scheme {
        if require_tls {
            return Err(AurixError::Validation(format!(
                "url must use {tls_scheme} ({setting})"
            )));
        }
    } else if scheme != tls_scheme {
        return Err(AurixError::Validation(format!(
            "url scheme '{scheme}' is not supported"
        )));
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(AurixError::Validation(
            "url must not embed credentials".into(),
        ));
    }
    if url.fragment().is_some() {
        return Err(AurixError::Validation(
            "url must not have a fragment".into(),
        ));
    }
    let Some(host) = url.host() else {
        return Err(AurixError::Validation("url must have a host".into()));
    };
    if !allow_private {
        let literal = match host {
            Host::Ipv4(ip) => Some(IpAddr::V4(ip)),
            Host::Ipv6(ip) => Some(IpAddr::V6(ip)),
            Host::Domain(d) => {
                if d.eq_ignore_ascii_case("localhost") || d.ends_with(".localhost") {
                    return Err(AurixError::Validation(
                        "url must not point at a private or loopback address".into(),
                    ));
                }
                None
            }
        };
        if literal.is_some_and(is_private_ip) {
            return Err(AurixError::Validation(
                "url must not point at a private or loopback address".into(),
            ));
        }
    }
    Ok(url)
}

/// Resolve `host:port` right now and return the addresses a connection may be pinned to. When
/// private targets are forbidden, private results are dropped so DNS rebinding cannot redirect
/// the connection after the URL was validated; an empty result is an error.
pub async fn resolve_outbound(
    host: &str,
    port: u16,
    allow_private: bool,
    setting: &str,
) -> Result<Vec<SocketAddr>, AurixError> {
    let resolved = tokio::net::lookup_host((host, port))
        .await
        .map_err(|e| AurixError::Validation(format!("cannot resolve host: {e}")))?;
    let addrs: Vec<SocketAddr> = resolved
        .filter(|a| allow_private || !is_private_ip(a.ip()))
        .collect();
    if addrs.is_empty() {
        return Err(AurixError::Validation(format!(
            "host resolves only to private addresses; refusing ({setting})"
        )));
    }
    Ok(addrs)
}

/// Resolve the real client IP.
///
/// `X-Forwarded-For` / `X-Real-IP` are honoured only when the direct peer is inside one of the
/// `trusted_proxies` CIDRs; otherwise the socket peer address is authoritative. With a trusted
/// proxy chain, the right-most address not belonging to a trusted proxy is the client.
pub fn client_ip(
    peer: SocketAddr,
    forwarded_for: Option<&str>,
    real_ip: Option<&str>,
    trusted_proxies: &[ipnetwork::IpNetwork],
) -> IpAddr {
    let peer_ip = crate::addr::canonical(peer).ip();
    let is_trusted = |ip: &IpAddr| trusted_proxies.iter().any(|n| n.contains(*ip));
    if !is_trusted(&peer_ip) {
        return peer_ip;
    }
    if let Some(xff) = forwarded_for {
        let hops: Vec<IpAddr> = xff
            .split(',')
            .filter_map(|s| s.trim().parse::<IpAddr>().ok())
            .map(crate::addr::canonical_ip)
            .collect();
        for ip in hops.iter().rev() {
            if !is_trusted(ip) {
                return *ip;
            }
        }
        if let Some(first) = hops.first() {
            return *first;
        }
    }
    if let Some(ip) = real_ip
        .and_then(|s| s.trim().parse::<IpAddr>().ok())
        .map(crate::addr::canonical_ip)
    {
        return ip;
    }
    peer_ip
}

pub fn parse_trusted_proxies(cidrs: &[String]) -> Vec<ipnetwork::IpNetwork> {
    cidrs.iter().filter_map(|c| c.parse().ok()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{Ipv4Addr, Ipv6Addr};

    #[test]
    fn untrusted_peer_ignores_forwarded_headers() {
        let peer: SocketAddr = "203.0.113.5:1234".parse().unwrap();
        let ip = client_ip(
            peer,
            Some("10.0.0.1"),
            Some("10.0.0.2"),
            &parse_trusted_proxies(&["10.0.0.0/8".into()]),
        );
        assert_eq!(ip, "203.0.113.5".parse::<IpAddr>().unwrap());
    }

    #[test]
    fn trusted_proxy_uses_rightmost_untrusted_hop() {
        let peer: SocketAddr = "10.0.0.1:1234".parse().unwrap();
        let proxies = parse_trusted_proxies(&["10.0.0.0/8".into()]);
        let ip = client_ip(
            peer,
            Some("198.51.100.7, 203.0.113.9, 10.0.0.2"),
            None,
            &proxies,
        );
        assert_eq!(ip, "203.0.113.9".parse::<IpAddr>().unwrap());
        let ip = client_ip(peer, None, Some("198.51.100.7"), &proxies);
        assert_eq!(ip, "198.51.100.7".parse::<IpAddr>().unwrap());
    }

    #[test]
    fn mapped_ipv4_peer_is_canonicalised() {
        let peer: SocketAddr = "[::ffff:203.0.113.5]:1234".parse().unwrap();
        let proxies = parse_trusted_proxies(&["10.0.0.0/8".into()]);
        assert_eq!(
            client_ip(peer, None, None, &proxies),
            "203.0.113.5".parse::<IpAddr>().unwrap()
        );
        // A trusted proxy on a dual-stack listener still shows up as trusted.
        let peer: SocketAddr = "[::ffff:10.0.0.1]:1234".parse().unwrap();
        assert_eq!(
            client_ip(peer, Some("::ffff:198.51.100.7"), None, &proxies),
            "198.51.100.7".parse::<IpAddr>().unwrap()
        );
    }

    #[test]
    fn parse_bind_addr_accepts_literals_and_localhost() {
        assert_eq!(
            parse_bind_addr("0.0.0.0", 10).unwrap(),
            "0.0.0.0:10".parse::<SocketAddr>().unwrap()
        );
        assert_eq!(
            parse_bind_addr("::", 10).unwrap(),
            "[::]:10".parse::<SocketAddr>().unwrap()
        );
        assert_eq!(
            parse_bind_addr("[::1]", 10).unwrap(),
            "[::1]:10".parse::<SocketAddr>().unwrap()
        );
        assert!(parse_bind_addr("localhost", 10).is_ok());
        assert!(parse_bind_addr("media.example.com", 10).is_err());
    }

    #[tokio::test]
    async fn ipv4_bind_only_reaches_ipv4() {
        let (sock, family) = bind_udp("127.0.0.1:0".parse().unwrap(), true, 0).unwrap();
        assert_eq!(family, BoundFamily::V4);
        assert!(family.accepts_v4() && !family.accepts_v6());
        let peer = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        peer.send_to(b"hi", sock.local_addr().unwrap())
            .await
            .unwrap();
        let mut buf = [0u8; 8];
        let (n, from) = sock.recv_from(&mut buf).await.unwrap();
        assert_eq!(&buf[..n], b"hi");
        assert!(from.is_ipv4());
    }

    #[tokio::test]
    async fn dual_stack_bind_receives_both_families_and_answers_on_the_wire_family() {
        let (sock, family) = match bind_udp("[::]:0".parse().unwrap(), true, 0) {
            Ok(v) => v,
            Err(e) => {
                eprintln!("skipping: no IPv6 on this host ({e})");
                return;
            }
        };
        assert_eq!(family, BoundFamily::DualStack);
        assert!(family.accepts_v4() && family.accepts_v6() && family.socket_is_v6());
        let port = sock.local_addr().unwrap().port();

        let v4 = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        v4.send_to(b"v4", SocketAddr::new(Ipv4Addr::LOCALHOST.into(), port))
            .await
            .unwrap();
        let mut buf = [0u8; 8];
        let (n, from) = sock.recv_from(&mut buf).await.unwrap();
        assert_eq!(&buf[..n], b"v4");
        let from = crate::addr::canonical(from);
        assert_eq!(from, v4.local_addr().unwrap());
        sock.send_to(b"ok", crate::addr::wire(from, family.socket_is_v6()))
            .await
            .unwrap();
        let (n, _) = v4.recv_from(&mut buf).await.unwrap();
        assert_eq!(&buf[..n], b"ok");

        let v6 = tokio::net::UdpSocket::bind("[::1]:0").await.unwrap();
        v6.send_to(b"v6", SocketAddr::new(Ipv6Addr::LOCALHOST.into(), port))
            .await
            .unwrap();
        let (n, from) = sock.recv_from(&mut buf).await.unwrap();
        assert_eq!(&buf[..n], b"v6");
        assert_eq!(crate::addr::canonical(from), v6.local_addr().unwrap());
    }

    #[tokio::test]
    async fn specific_ipv6_bind_is_v6_only() {
        let (sock, family) = match bind_udp("[::1]:0".parse().unwrap(), true, 0) {
            Ok(v) => v,
            Err(e) => {
                eprintln!("skipping: no IPv6 on this host ({e})");
                return;
            }
        };
        assert_eq!(family, BoundFamily::V6Only);
        assert!(!family.accepts_v4());
        let port = sock.local_addr().unwrap().port();
        let v4 = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        // Unreachable from IPv4: either the send is refused or nothing arrives.
        let _ = v4
            .send_to(b"v4", SocketAddr::new(Ipv4Addr::LOCALHOST.into(), port))
            .await;
        let mut buf = [0u8; 8];
        let got = tokio::time::timeout(
            std::time::Duration::from_millis(200),
            sock.recv_from(&mut buf),
        )
        .await;
        assert!(got.is_err(), "IPv6-only socket must not see IPv4 datagrams");

        let (listener, family) = bind_tcp("[::1]:0".parse().unwrap(), true).unwrap();
        assert_eq!(family, BoundFamily::V6Only);
        let addr = listener.local_addr().unwrap();
        let accept = tokio::spawn(async move { listener.accept().await.map(|(_, a)| a) });
        tokio::net::TcpStream::connect(addr).await.unwrap();
        assert!(accept.await.unwrap().unwrap().is_ipv6());
    }
}
