//! Address-family helpers shared by the server and the client core.
//!
//! A dual-stack socket (bound to `::` with `IPV6_V6ONLY` off) reports IPv4 peers as IPv4-mapped
//! IPv6 addresses (`::ffff:203.0.113.7`). Aurix keeps every address in *canonical* form (a real
//! IPv4 address for mapped peers) so session tables, ICE candidates, STUN attributes, rate-limit
//! keys and logs agree with each other, and converts back to the socket's own family only at the
//! moment a datagram is sent.

use std::net::{IpAddr, Ipv6Addr, SocketAddr};

/// Canonical form of a peer address: IPv4-mapped IPv6 becomes plain IPv4, everything else is
/// returned unchanged.
pub fn canonical(addr: SocketAddr) -> SocketAddr {
    SocketAddr::new(canonical_ip(addr.ip()), addr.port())
}

/// [`canonical`] for a bare IP.
pub fn canonical_ip(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V6(v6) => match v6.to_ipv4_mapped() {
            Some(v4) => IpAddr::V4(v4),
            None => ip,
        },
        v4 => v4,
    }
}

/// The address to hand to `send_to` on a socket whose local address is `socket_is_v6`: an IPv4
/// destination becomes IPv4-mapped IPv6 on an IPv6 (dual-stack) socket, since not every
/// platform accepts an `AF_INET` sockaddr on an `AF_INET6` socket.
pub fn wire(addr: SocketAddr, socket_is_v6: bool) -> SocketAddr {
    match addr {
        SocketAddr::V4(v4) if socket_is_v6 => {
            SocketAddr::new(IpAddr::V6(v4.ip().to_ipv6_mapped()), v4.port())
        }
        other => other,
    }
}

/// `host:port` for clients: IP literals are formatted as socket addresses (IPv6 in brackets),
/// host names as `name:port`.
pub fn host_port(host: &str, port: u16) -> String {
    match host
        .trim()
        .trim_start_matches('[')
        .trim_end_matches(']')
        .parse::<IpAddr>()
    {
        Ok(ip) => SocketAddr::new(ip, port).to_string(),
        Err(_) => format!("{}:{}", host.trim(), port),
    }
}

/// Split `host:port` / `[v6]:port` into its parts (brackets removed).
pub fn split_host_port(s: &str) -> Option<(&str, u16)> {
    let colon = s.rfind(':')?;
    let host = s[..colon].trim_start_matches('[').trim_end_matches(']');
    let port = s[colon + 1..].parse().ok()?;
    Some((host, port))
}

/// Whether `host` is a wildcard bind address (`0.0.0.0`, `::`, `[::]`, empty).
pub fn is_unspecified_host(host: &str) -> bool {
    let h = host.trim().trim_start_matches('[').trim_end_matches(']');
    h.is_empty()
        || h.parse::<IpAddr>()
            .map(|ip| ip.is_unspecified())
            .unwrap_or(false)
}

/// Whether a bind host selects an IPv6 socket (`::`, `::1`, any IPv6 literal). A dual-stack
/// listener is an IPv6 socket bound to the unspecified address with `IPV6_V6ONLY` off.
pub fn bind_host_is_v6(host: &str) -> bool {
    host.trim()
        .trim_start_matches('[')
        .trim_end_matches(']')
        .parse::<Ipv6Addr>()
        .is_ok()
}

/// Rate-limit / abuse key for a client IP: IPv4 as-is, IPv6 truncated to `v6_prefix` bits so a
/// client rotating through its delegated /64 shares one bucket. `0` or `>= 128` keeps the full
/// address.
pub fn client_key(ip: IpAddr, v6_prefix: u8) -> String {
    match ip {
        IpAddr::V4(v4) => v4.to_string(),
        IpAddr::V6(v6) => {
            if let Some(v4) = v6.to_ipv4_mapped() {
                return v4.to_string();
            }
            if v6_prefix == 0 || v6_prefix >= 128 {
                return v6.to_string();
            }
            let bits = u128::from(v6);
            let mask = u128::MAX << (128 - v6_prefix as u32);
            format!("{}/{}", Ipv6Addr::from(bits & mask), v6_prefix)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;

    #[test]
    fn canonical_unmaps_ipv4_mapped_peers() {
        let mapped: SocketAddr = "[::ffff:203.0.113.7]:5000".parse().unwrap();
        assert_eq!(canonical(mapped), "203.0.113.7:5000".parse().unwrap());
        let v6: SocketAddr = "[2001:db8::1]:5000".parse().unwrap();
        assert_eq!(canonical(v6), v6);
        let v4: SocketAddr = "10.0.0.1:1".parse().unwrap();
        assert_eq!(canonical(v4), v4);
    }

    #[test]
    fn wire_maps_ipv4_only_on_ipv6_sockets() {
        let v4: SocketAddr = "203.0.113.7:5000".parse().unwrap();
        assert_eq!(wire(v4, false), v4);
        assert_eq!(wire(v4, true), "[::ffff:203.0.113.7]:5000".parse().unwrap());
        let v6: SocketAddr = "[2001:db8::1]:5000".parse().unwrap();
        assert_eq!(wire(v6, true), v6);
        assert_eq!(canonical(wire(v4, true)), v4);
    }

    #[test]
    fn host_port_brackets_ipv6_literals() {
        assert_eq!(host_port("203.0.113.7", 10000), "203.0.113.7:10000");
        assert_eq!(host_port("2001:db8::1", 10000), "[2001:db8::1]:10000");
        assert_eq!(host_port("[2001:db8::1]", 10000), "[2001:db8::1]:10000");
        assert_eq!(host_port("::", 10000), "[::]:10000");
        assert_eq!(
            host_port("voice.example.com", 10000),
            "voice.example.com:10000"
        );
        assert_eq!(
            split_host_port("[2001:db8::1]:10000"),
            Some(("2001:db8::1", 10000))
        );
        assert_eq!(split_host_port("[::]:10000"), Some(("::", 10000)));
        assert_eq!(split_host_port("host:1"), Some(("host", 1)));
        assert_eq!(split_host_port("nonsense"), None);
    }

    #[test]
    fn bind_host_classification() {
        assert!(is_unspecified_host("0.0.0.0"));
        assert!(is_unspecified_host("::"));
        assert!(is_unspecified_host("[::]"));
        assert!(is_unspecified_host(""));
        assert!(!is_unspecified_host("127.0.0.1"));
        assert!(!is_unspecified_host("::1"));
        assert!(bind_host_is_v6("::"));
        assert!(bind_host_is_v6("::1"));
        assert!(bind_host_is_v6("[2001:db8::1]"));
        assert!(!bind_host_is_v6("0.0.0.0"));
        assert!(!bind_host_is_v6("localhost"));
    }

    #[test]
    fn client_key_truncates_ipv6_to_prefix() {
        assert_eq!(
            client_key(IpAddr::V4(Ipv4Addr::new(198, 51, 100, 9)), 64),
            "198.51.100.9"
        );
        let a: IpAddr = "2001:db8:1:2:3:4:5:6".parse().unwrap();
        let b: IpAddr = "2001:db8:1:2:ffff::1".parse().unwrap();
        let c: IpAddr = "2001:db8:1:3::1".parse().unwrap();
        assert_eq!(client_key(a, 64), "2001:db8:1:2::/64");
        assert_eq!(client_key(a, 64), client_key(b, 64));
        assert_ne!(client_key(a, 64), client_key(c, 64));
        assert_eq!(client_key(a, 0), a.to_string());
        assert_eq!(client_key(a, 128), a.to_string());
        let mapped: IpAddr = "::ffff:198.51.100.9".parse().unwrap();
        assert_eq!(client_key(mapped, 64), "198.51.100.9");
    }
}
