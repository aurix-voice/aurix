//! Client address resolution behind reverse proxies, and the SSRF guard shared by every
//! operator-supplied outbound URL (webhooks, live audio push streams).

use std::net::{IpAddr, SocketAddr};

use url::{Host, Url};

use crate::error::AurixError;

pub const MAX_OUTBOUND_URL_LEN: usize = 2048;

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
