//! Client address resolution behind reverse proxies.

use std::net::{IpAddr, SocketAddr};

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
    let peer_ip = peer.ip();
    let is_trusted = |ip: &IpAddr| trusted_proxies.iter().any(|n| n.contains(*ip));
    if !is_trusted(&peer_ip) {
        return peer_ip;
    }
    if let Some(xff) = forwarded_for {
        let hops: Vec<IpAddr> = xff
            .split(',')
            .filter_map(|s| s.trim().parse::<IpAddr>().ok())
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
    if let Some(ip) = real_ip.and_then(|s| s.trim().parse::<IpAddr>().ok()) {
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
}
