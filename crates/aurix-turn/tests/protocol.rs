//! End-to-end TURN protocol tests over real loopback sockets (UDP and TCP clients).

use aurix_common::config::TurnConfig;
use aurix_common::crypto::{generate_turn_credentials, stun_long_term_key};
use aurix_turn::stun::{StunAttributeType, StunMessage, StunMessageType};
use aurix_turn::TurnServer;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpStream, UdpSocket};

const SECRET: &str = "integration-turn-secret-0123456789";
const REALM: &str = "test.aurix";

async fn start_server() -> (Arc<TurnServer>, SocketAddr, SocketAddr) {
    let cfg = TurnConfig {
        enabled: true,
        host: "127.0.0.1".into(),
        udp_port: 0,
        tcp_port: 0,
        realm: REALM.into(),
        auth_secret: SECRET.into(),
        min_port: 40000,
        max_port: 40100,
        allocation_lifetime_secs: 600,
        max_allocations: 100,
        external_ip: None,
        credential_ttl_secs: 3600,
    };
    let server = Arc::new(TurnServer::new(&cfg));
    let (udp, tcp_addr) = server.bind().await.unwrap();
    let udp_addr = udp.local_addr().unwrap();
    let s = server.clone();
    tokio::spawn(async move {
        let _ = s.serve_udp(udp).await;
    });
    (server, udp_addr, tcp_addr)
}

fn txid() -> [u8; 12] {
    rand::random()
}

struct Creds {
    username: String,
    key: [u8; 16],
}

fn creds(user: &str, ttl: i64) -> Creds {
    let c = generate_turn_credentials(SECRET, user, ttl, chrono::Utc::now().timestamp());
    let key = stun_long_term_key(&c.username, REALM, &c.password);
    Creds { username: c.username, key }
}

/// Minimal TURN client over UDP.
struct UdpClient {
    sock: UdpSocket,
    server: SocketAddr,
    nonce: Option<String>,
}

impl UdpClient {
    async fn new(server: SocketAddr) -> Self {
        let sock = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        Self { sock, server, nonce: None }
    }

    async fn recv(&self) -> Option<Vec<u8>> {
        let mut buf = vec![0u8; 4096];
        match tokio::time::timeout(Duration::from_millis(700), self.sock.recv_from(&mut buf)).await {
            Ok(Ok((n, _))) => Some(buf[..n].to_vec()),
            _ => None,
        }
    }

    async fn send_raw(&self, data: &[u8]) {
        self.sock.send_to(data, self.server).await.unwrap();
    }

    async fn roundtrip(&self, data: &[u8]) -> StunMessage {
        self.send_raw(data).await;
        let raw = self.recv().await.expect("server did not respond");
        StunMessage::decode(&raw).unwrap()
    }

    /// Fetch a nonce via an unauthenticated request (expects 401).
    async fn challenge(&mut self) {
        let mut req = StunMessage::new(StunMessageType::AllocateRequest, txid());
        req.add_attribute(StunAttributeType::RequestedTransport, vec![17, 0, 0, 0]);
        let resp = self.roundtrip(&req.encode()).await;
        assert_eq!(resp.msg_type, StunMessageType::AllocateErrorResponse);
        assert_eq!(error_code(&resp), Some(401));
        assert_eq!(resp.get_string(StunAttributeType::Realm).as_deref(), Some(REALM));
        self.nonce = Some(resp.get_string(StunAttributeType::Nonce).unwrap());
    }

    fn authed(&self, ty: StunMessageType, c: &Creds) -> StunMessage {
        let mut m = StunMessage::new(ty, txid());
        m.add_attribute(StunAttributeType::Username, c.username.as_bytes().to_vec());
        m.add_attribute(StunAttributeType::Realm, REALM.as_bytes().to_vec());
        m.add_attribute(StunAttributeType::Nonce, self.nonce.clone().unwrap().into_bytes());
        m
    }

    async fn allocate(&self, c: &Creds) -> StunMessage {
        let mut req = self.authed(StunMessageType::AllocateRequest, c);
        req.add_attribute(StunAttributeType::RequestedTransport, vec![17, 0, 0, 0]);
        self.roundtrip(&req.encode_with_integrity(&c.key)).await
    }
}

fn error_code(m: &StunMessage) -> Option<u16> {
    m.get_attribute(StunAttributeType::ErrorCode).map(|a| a.value[2] as u16 * 100 + a.value[3] as u16)
}

#[tokio::test]
async fn full_udp_flow_with_permissions_channels_and_refresh() {
    let (server, udp_addr, _) = start_server().await;
    let c = creds("alice", 600);
    let mut client = UdpClient::new(udp_addr).await;
    client.challenge().await;

    // Allocate (authenticated, signed).
    let resp = client.allocate(&c).await;
    assert_eq!(resp.msg_type, StunMessageType::AllocateResponse, "{:?}", error_code(&resp));
    let relay = resp.get_xor_address(StunAttributeType::XorRelayedAddress).unwrap();
    let mapped = resp.get_xor_address(StunAttributeType::XorMappedAddress).unwrap();
    assert_eq!(mapped, client.sock.local_addr().unwrap());
    assert!((40000..=40100).contains(&relay.port()));
    assert_eq!(resp.get_u32(StunAttributeType::Lifetime), Some(600));
    assert_eq!(server.handler().allocation_count(), 1);
    // Server responses carry MESSAGE-INTEGRITY the client can verify.
    {
        let mut req = client.authed(StunMessageType::RefreshRequest, &c);
        req.add_attribute(StunAttributeType::Lifetime, 700u32.to_be_bytes().to_vec());
        client.send_raw(&req.encode_with_integrity(&c.key)).await;
        let raw = client.recv().await.unwrap();
        assert!(StunMessage::verify_integrity(&raw, &c.key), "refresh response must be signed");
        let m = StunMessage::decode(&raw).unwrap();
        assert_eq!(m.msg_type, StunMessageType::RefreshResponse);
        assert_eq!(m.get_u32(StunAttributeType::Lifetime), Some(700));
    }

    // A second Allocate on the same 5-tuple is a mismatch.
    let dup = client.allocate(&c).await;
    assert_eq!(error_code(&dup), Some(437));

    // Peer sends to the relay before any permission: must be dropped (no open relay).
    let peer = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    peer.send_to(b"unsolicited", relay).await.unwrap();
    assert!(client.recv().await.is_none(), "data without permission must not reach client");

    // Send indication to the peer before permission: dropped.
    {
        let mut ind = StunMessage::new(StunMessageType::SendIndication, txid());
        ind.add_xor_address(StunAttributeType::XorPeerAddress, peer.local_addr().unwrap());
        ind.add_attribute(StunAttributeType::Data, b"nope".to_vec());
        client.send_raw(&ind.encode()).await;
        let mut buf = [0u8; 64];
        assert!(tokio::time::timeout(Duration::from_millis(300), peer.recv_from(&mut buf)).await.is_err());
    }

    // CreatePermission for the peer.
    {
        let mut req = client.authed(StunMessageType::CreatePermissionRequest, &c);
        req.add_xor_address(StunAttributeType::XorPeerAddress, peer.local_addr().unwrap());
        let resp = client.roundtrip(&req.encode_with_integrity(&c.key)).await;
        assert_eq!(resp.msg_type, StunMessageType::CreatePermissionResponse, "{:?}", error_code(&resp));
    }

    // Client -> peer via Send indication.
    {
        let mut ind = StunMessage::new(StunMessageType::SendIndication, txid());
        ind.add_xor_address(StunAttributeType::XorPeerAddress, peer.local_addr().unwrap());
        ind.add_attribute(StunAttributeType::Data, b"hello peer".to_vec());
        client.send_raw(&ind.encode()).await;
        let mut buf = [0u8; 64];
        let (n, from) = tokio::time::timeout(Duration::from_secs(1), peer.recv_from(&mut buf)).await.unwrap().unwrap();
        assert_eq!(&buf[..n], b"hello peer");
        assert_eq!(from, relay);
    }

    // Peer -> client arrives as a Data indication.
    {
        peer.send_to(b"hi client", relay).await.unwrap();
        let raw = client.recv().await.expect("data indication");
        let m = StunMessage::decode(&raw).unwrap();
        assert_eq!(m.msg_type, StunMessageType::DataIndication);
        assert_eq!(m.get_attribute(StunAttributeType::Data).unwrap().value, b"hi client");
        assert_eq!(m.get_xor_address(StunAttributeType::XorPeerAddress), Some(peer.local_addr().unwrap()));
    }

    // ChannelBind and ChannelData in both directions.
    {
        let mut req = client.authed(StunMessageType::ChannelBindRequest, &c);
        req.add_attribute(StunAttributeType::ChannelNumber, vec![0x40, 0x01, 0, 0]);
        req.add_xor_address(StunAttributeType::XorPeerAddress, peer.local_addr().unwrap());
        let resp = client.roundtrip(&req.encode_with_integrity(&c.key)).await;
        assert_eq!(resp.msg_type, StunMessageType::ChannelBindResponse, "{:?}", error_code(&resp));

        // Bad channel number is rejected.
        let mut bad = client.authed(StunMessageType::ChannelBindRequest, &c);
        bad.add_attribute(StunAttributeType::ChannelNumber, vec![0x30, 0x01, 0, 0]);
        bad.add_xor_address(StunAttributeType::XorPeerAddress, peer.local_addr().unwrap());
        let resp = client.roundtrip(&bad.encode_with_integrity(&c.key)).await;
        assert_eq!(resp.msg_type, StunMessageType::ChannelBindErrorResponse);
        assert_eq!(error_code(&resp), Some(400));

        let mut cd = vec![0x40, 0x01, 0, 5];
        cd.extend_from_slice(b"chan!");
        client.send_raw(&cd).await;
        let mut buf = [0u8; 64];
        let (n, _) = tokio::time::timeout(Duration::from_secs(1), peer.recv_from(&mut buf)).await.unwrap().unwrap();
        assert_eq!(&buf[..n], b"chan!");

        peer.send_to(b"back", relay).await.unwrap();
        let raw = client.recv().await.unwrap();
        assert_eq!(&raw[..4], &[0x40, 0x01, 0, 4]);
        assert_eq!(&raw[4..], b"back");
    }

    // Wrong user cannot refresh/steal someone else's allocation from a spoofed message.
    {
        let mallory = creds("mallory", 600);
        let mut req = client.authed(StunMessageType::RefreshRequest, &mallory);
        req.add_attribute(StunAttributeType::Lifetime, 0u32.to_be_bytes().to_vec());
        let resp = client.roundtrip(&req.encode_with_integrity(&mallory.key)).await;
        assert_eq!(error_code(&resp), Some(441));
        assert_eq!(server.handler().allocation_count(), 1);
    }

    // Refresh with lifetime 0 releases the allocation and frees the relay port.
    {
        let mut req = client.authed(StunMessageType::RefreshRequest, &c);
        req.add_attribute(StunAttributeType::Lifetime, 0u32.to_be_bytes().to_vec());
        let resp = client.roundtrip(&req.encode_with_integrity(&c.key)).await;
        assert_eq!(resp.msg_type, StunMessageType::RefreshResponse);
        assert_eq!(resp.get_u32(StunAttributeType::Lifetime), Some(0));
        assert_eq!(server.handler().allocation_count(), 0);
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(UdpSocket::bind(relay).await.is_ok(), "relay port must be released");
        peer.send_to(b"ghost", relay).await.unwrap();
        assert!(client.recv().await.is_none());
    }
}

#[tokio::test]
async fn rejects_unauthenticated_forged_and_stale_requests() {
    let (server, udp_addr, _) = start_server().await;
    let mut client = UdpClient::new(udp_addr).await;
    client.challenge().await;

    // Valid username but no MESSAGE-INTEGRITY: rejected even with a valid nonce.
    let c = creds("alice", 600);
    let mut req = client.authed(StunMessageType::AllocateRequest, &c);
    req.add_attribute(StunAttributeType::RequestedTransport, vec![17, 0, 0, 0]);
    let resp = client.roundtrip(&req.encode()).await;
    assert_eq!(error_code(&resp), Some(401));

    // Wrong password (forged key).
    let forged = Creds { username: c.username.clone(), key: [7u8; 16] };
    let resp = client.allocate(&forged).await;
    assert_eq!(error_code(&resp), Some(401));

    // Expired credentials with a correct HMAC.
    let expired = creds("alice", -5);
    let resp = client.allocate(&expired).await;
    assert_eq!(error_code(&resp), Some(401));

    // Wrong realm.
    {
        let mut m = StunMessage::new(StunMessageType::AllocateRequest, txid());
        m.add_attribute(StunAttributeType::Username, c.username.as_bytes().to_vec());
        m.add_attribute(StunAttributeType::Realm, b"other".to_vec());
        m.add_attribute(StunAttributeType::Nonce, client.nonce.clone().unwrap().into_bytes());
        m.add_attribute(StunAttributeType::RequestedTransport, vec![17, 0, 0, 0]);
        let key = stun_long_term_key(&c.username, "other", "x");
        let resp = client.roundtrip(&m.encode_with_integrity(&key)).await;
        assert_eq!(error_code(&resp), Some(401));
    }

    // Bogus nonce -> 438 Stale Nonce with a fresh nonce.
    {
        let saved = client.nonce.replace("0.deadbeef".into());
        let resp = client.allocate(&c).await;
        assert_eq!(error_code(&resp), Some(438));
        assert!(resp.get_string(StunAttributeType::Nonce).is_some());
        client.nonce = saved;
    }

    // Tampered message body fails integrity.
    {
        let mut req = client.authed(StunMessageType::AllocateRequest, &c);
        req.add_attribute(StunAttributeType::RequestedTransport, vec![17, 0, 0, 0]);
        let mut raw = req.encode_with_integrity(&c.key).to_vec();
        raw[28] ^= 0x01;
        // Fix the fingerprint so only the HMAC is wrong.
        let fp_off = aurix_turn::stun::find_attribute_offset(&raw, StunAttributeType::Fingerprint.to_u16()).unwrap();
        let mut head = raw[..fp_off].to_vec();
        head[2..4].copy_from_slice(&((fp_off - 20 + 8) as u16).to_be_bytes());
        let crc = crc32fast::hash(&head) ^ 0x5354554E;
        raw[fp_off + 4..fp_off + 8].copy_from_slice(&crc.to_be_bytes());
        let resp = client.roundtrip(&raw).await;
        assert_eq!(error_code(&resp), Some(401));
    }

    // TCP transport in REQUESTED-TRANSPORT is refused.
    {
        let mut req = client.authed(StunMessageType::AllocateRequest, &c);
        req.add_attribute(StunAttributeType::RequestedTransport, vec![6, 0, 0, 0]);
        let resp = client.roundtrip(&req.encode_with_integrity(&c.key)).await;
        assert_eq!(error_code(&resp), Some(442));
    }
    assert_eq!(server.handler().allocation_count(), 0);

    // Requests on non-existent allocations.
    let mut req = client.authed(StunMessageType::CreatePermissionRequest, &c);
    req.add_xor_address(StunAttributeType::XorPeerAddress, "127.0.0.1:9".parse().unwrap());
    let resp = client.roundtrip(&req.encode_with_integrity(&c.key)).await;
    assert_eq!(error_code(&resp), Some(437));

    // Plain STUN binding still works without credentials.
    let bind = StunMessage::new(StunMessageType::BindingRequest, txid());
    let resp = client.roundtrip(&bind.encode()).await;
    assert_eq!(resp.msg_type, StunMessageType::BindingResponse);
    assert_eq!(resp.get_xor_address(StunAttributeType::XorMappedAddress), Some(client.sock.local_addr().unwrap()));
}

/// Read one self-delimiting frame (STUN or ChannelData) from a TCP stream.
async fn read_tcp_frame(stream: &mut TcpStream) -> Vec<u8> {
    let mut head = [0u8; 4];
    stream.read_exact(&mut head).await.unwrap();
    let total = if head[0] & 0xC0 == 0x40 {
        let len = u16::from_be_bytes([head[2], head[3]]) as usize;
        4 + len + (4 - (len % 4)) % 4
    } else {
        20 + u16::from_be_bytes([head[2], head[3]]) as usize
    };
    let mut frame = head.to_vec();
    frame.resize(total, 0);
    stream.read_exact(&mut frame[4..]).await.unwrap();
    frame
}

#[tokio::test]
async fn tcp_client_allocates_and_relays_udp() {
    let (server, _udp, tcp_addr) = start_server().await;
    let mut stream = TcpStream::connect(tcp_addr).await.unwrap();
    let c = creds("bob", 600);

    // Challenge.
    let mut req = StunMessage::new(StunMessageType::AllocateRequest, txid());
    req.add_attribute(StunAttributeType::RequestedTransport, vec![17, 0, 0, 0]);
    stream.write_all(&req.encode()).await.unwrap();
    let resp = StunMessage::decode(&read_tcp_frame(&mut stream).await).unwrap();
    assert_eq!(error_code(&resp), Some(401));
    let nonce = resp.get_string(StunAttributeType::Nonce).unwrap();

    let authed = |ty: StunMessageType| {
        let mut m = StunMessage::new(ty, txid());
        m.add_attribute(StunAttributeType::Username, c.username.as_bytes().to_vec());
        m.add_attribute(StunAttributeType::Realm, REALM.as_bytes().to_vec());
        m.add_attribute(StunAttributeType::Nonce, nonce.clone().into_bytes());
        m
    };

    // Allocate; send the frame in two TCP writes to exercise stream reassembly.
    let mut req = authed(StunMessageType::AllocateRequest);
    req.add_attribute(StunAttributeType::RequestedTransport, vec![17, 0, 0, 0]);
    let raw = req.encode_with_integrity(&c.key);
    stream.write_all(&raw[..10]).await.unwrap();
    tokio::time::sleep(Duration::from_millis(20)).await;
    stream.write_all(&raw[10..]).await.unwrap();
    let raw_resp = read_tcp_frame(&mut stream).await;
    assert!(StunMessage::verify_integrity(&raw_resp, &c.key));
    let resp = StunMessage::decode(&raw_resp).unwrap();
    assert_eq!(resp.msg_type, StunMessageType::AllocateResponse, "{:?}", error_code(&resp));
    let relay = resp.get_xor_address(StunAttributeType::XorRelayedAddress).unwrap();
    assert_eq!(server.handler().allocation_count(), 1);

    // Permission + channel bind, then relay both ways with 4-byte padded ChannelData.
    let peer = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let mut req = authed(StunMessageType::ChannelBindRequest);
    req.add_attribute(StunAttributeType::ChannelNumber, vec![0x40, 0x10, 0, 0]);
    req.add_xor_address(StunAttributeType::XorPeerAddress, peer.local_addr().unwrap());
    stream.write_all(&req.encode_with_integrity(&c.key)).await.unwrap();
    let resp = StunMessage::decode(&read_tcp_frame(&mut stream).await).unwrap();
    assert_eq!(resp.msg_type, StunMessageType::ChannelBindResponse, "{:?}", error_code(&resp));

    let mut cd = vec![0x40, 0x10, 0, 5];
    cd.extend_from_slice(b"tcp!!");
    cd.extend_from_slice(&[0, 0, 0]);
    stream.write_all(&cd).await.unwrap();
    let mut buf = [0u8; 64];
    let (n, _) = tokio::time::timeout(Duration::from_secs(1), peer.recv_from(&mut buf)).await.unwrap().unwrap();
    assert_eq!(&buf[..n], b"tcp!!");

    peer.send_to(b"abc", relay).await.unwrap();
    let frame = tokio::time::timeout(Duration::from_secs(1), read_tcp_frame(&mut stream)).await.unwrap();
    assert_eq!(frame, vec![0x40, 0x10, 0, 3, b'a', b'b', b'c', 0]);

    // Closing the TCP connection tears down the allocation.
    drop(stream);
    for _ in 0..20 {
        if server.handler().allocation_count() == 0 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(server.handler().allocation_count(), 0);
}
