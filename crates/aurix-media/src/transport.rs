//! The media UDP socket. One socket carries native AURX, WebRTC (ICE/DTLS/SRTP) and, on a
//! dual-stack node, both address families: peers are reported in canonical form (real IPv4 for
//! IPv4-mapped addresses) and destinations are converted back to the socket's own family on send,
//! so the rest of the SFU never sees `::ffff:` addresses.

use std::net::SocketAddr;
use std::sync::Arc;

use aurix_common::net::FamilyUdpSocket;

pub const MEDIA_SOCKET_BUFFER_BYTES: usize = 4 * 1024 * 1024;

/// The media socket: [`FamilyUdpSocket`] with the SFU's socket buffers and dual-stack wildcard
/// binds.
pub type MediaSocket = FamilyUdpSocket;

/// Bind the media socket. A wildcard IPv6 `bind_addr` (`[::]:port`) becomes dual-stack.
pub fn bind_media_socket(bind_addr: SocketAddr) -> std::io::Result<Arc<MediaSocket>> {
    FamilyUdpSocket::bind(bind_addr, true, MEDIA_SOCKET_BUFFER_BYTES)
}

#[cfg(test)]
mod tests {
    use super::*;
    use aurix_common::net::BoundFamily;
    use tokio::net::UdpSocket;

    #[tokio::test]
    async fn dual_stack_socket_hides_mapped_addresses() {
        let sock = match bind_media_socket("[::]:0".parse().unwrap()) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("skipping: no IPv6 ({e})");
                return;
            }
        };
        assert_eq!(sock.family(), BoundFamily::DualStack);
        let port = sock.local_addr().port();
        let v4 = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        v4.send_to(b"ping", ("127.0.0.1", port)).await.unwrap();
        let mut buf = [0u8; 16];
        let (n, from) = sock.recv_from(&mut buf).await.unwrap();
        assert_eq!(&buf[..n], b"ping");
        assert!(from.is_ipv4(), "peer must be canonical IPv4, got {from}");
        assert!(sock.can_reach(from));
        sock.send_to(b"pong", from).await.unwrap();
        let (n, _) = v4.recv_from(&mut buf).await.unwrap();
        assert_eq!(&buf[..n], b"pong");
    }

    #[tokio::test]
    async fn ipv4_socket_cannot_reach_ipv6() {
        let sock = bind_media_socket("127.0.0.1:0".parse().unwrap()).unwrap();
        assert_eq!(sock.family(), BoundFamily::V4);
        assert!(!sock.can_reach("[::1]:1".parse().unwrap()));
        assert!(sock.can_reach("127.0.0.1:1".parse().unwrap()));
    }
}
