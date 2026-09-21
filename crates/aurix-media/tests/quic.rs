//! Raw QUIC clients (quinn, no `aurix-client`) against a real `SfuNode`: the node's
//! datagram media path is only as trusting as UDP — authenticated `SessionBind` first,
//! replayed early data refused, a connection speaks for exactly one session, a superseded
//! connection is closed and its media dropped, the connection cap refuses handshakes, and a
//! node with QUIC off (or an old UDP-only client) keeps working.

use aurix_common::protocol::*;
use aurix_common::quic::{client_config, client_tls_config};
use aurix_common::types::*;
use aurix_media::quic::QuicOptions;
use aurix_media::{MediaSession, SfuNode, SfuOptions};
use bytes::Bytes;
use quinn::rustls;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio::net::UdpSocket;

const IDLE: Duration = Duration::from_secs(5);

async fn start_sfu(quic: QuicOptions) -> (SfuNode, SocketAddr) {
    let mut sfu = SfuNode::new(
        MediaNodeId::new(),
        Region::EuWest,
        SfuOptions {
            max_participants: 4,
            quic,
            ..SfuOptions::default()
        },
    );
    sfu.start("127.0.0.1:0".parse().unwrap()).await.unwrap();
    let addr = sfu.local_addr().unwrap();
    (sfu, addr)
}

fn session(sfu: &SfuNode, app: AppId, channel: ChannelId, name: &str) -> Arc<MediaSession> {
    let s = sfu
        .create_session(SessionId::new(), UserId::new(), app, name.into())
        .unwrap();
    sfu.join_channel(
        &s.session_id,
        channel,
        ChannelConfig::default(),
        ChannelRole::Speaker,
    )
    .unwrap();
    s
}

/// A bare quinn client: one endpoint per connection, pinned to the node's certificate.
struct Raw {
    endpoint: quinn::Endpoint,
    conn: quinn::Connection,
    zero_rtt: bool,
    seq: u32,
}

impl Raw {
    async fn connect(server: SocketAddr, info: &QuicInfo, tls: &Arc<rustls::ClientConfig>) -> Raw {
        let socket = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        let mut endpoint = quinn::Endpoint::new(
            quinn::EndpointConfig::default(),
            None,
            socket,
            Arc::new(quinn::TokioRuntime),
        )
        .unwrap();
        endpoint.set_default_client_config(
            client_config(tls.clone(), IDLE, 128, Some(Duration::from_secs(1))).unwrap(),
        );
        let connecting = endpoint.connect(server, &info.server_name).unwrap();
        let (conn, zero_rtt) = match connecting.into_0rtt() {
            Ok((conn, _accepted)) => (conn, true),
            Err(connecting) => (
                tokio::time::timeout(Duration::from_secs(2), connecting)
                    .await
                    .expect("handshake")
                    .expect("handshake"),
                false,
            ),
        };
        Raw {
            endpoint,
            conn,
            zero_rtt,
            seq: 1,
        }
    }

    fn next_seq(&mut self) -> u32 {
        let s = self.seq;
        self.seq += 1;
        s
    }

    fn bind_bytes(s: &MediaSession, nonce: u64) -> Bytes {
        let now = chrono::Utc::now().timestamp_millis();
        AurixPacket::session_bind(&s.session_id, s.ssrc, now, nonce)
            .encode_authenticated(&s.keys)
            .freeze()
    }

    async fn recv(&self) -> Option<AurixPacket> {
        tokio::time::timeout(Duration::from_millis(400), self.conn.read_datagram())
            .await
            .ok()
            .and_then(|r| r.ok())
            .map(|d| AurixPacket::decode(&d).unwrap())
    }

    async fn bind(&mut self, s: &MediaSession) -> Option<AurixPacket> {
        self.conn.send_datagram(Self::bind_bytes(s, 7)).unwrap();
        let mut ack = self.recv().await?;
        assert_eq!(ack.header.packet_type, PacketType::SessionBindAck);
        assert!(ack.header.has_flag(PacketFlags::Encrypted));
        assert!(ack.open(&s.keys), "ack must be authenticated");
        Some(ack)
    }

    fn send_audio(&mut self, s: &MediaSession, channel: &ChannelId, payload: &[u8]) {
        let seq = self.next_seq();
        let pkt = AurixPacket::audio(
            seq,
            seq * 960,
            s.ssrc,
            channel_id_hash(channel),
            Bytes::copy_from_slice(payload),
        )
        .seal(&s.keys)
        .freeze();
        self.conn.send_datagram(pkt).unwrap();
    }

    async fn recv_audio(&self, s: &MediaSession) -> Option<AurixPacket> {
        loop {
            let mut p = self.recv().await?;
            if p.header.packet_type == PacketType::Audio {
                assert!(p.open(&s.keys));
                return Some(p);
            }
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn quic_connections_authenticate_like_udp_and_stay_with_one_session() {
    let (sfu, addr) = start_sfu(QuicOptions::default()).await;
    let info = sfu.quic_info().unwrap();
    let tls = client_tls_config(&info).unwrap();
    let app = AppId::new();
    let channel = ChannelId::new();
    let s_a = session(&sfu, app, channel, "a");
    let s_b = session(&sfu, app, channel, "b");
    let s_c = session(&sfu, app, channel, "c");

    // Media before a bind: the connection is nobody yet.
    let mut a = Raw::connect(addr, &info, &tls).await;
    assert!(!a.zero_rtt);
    let mut b = Raw::connect(addr, &info, &tls).await;
    b.bind(&s_b).await.expect("B binds");
    a.send_audio(&s_a, &channel, b"unbound");
    assert!(
        b.recv_audio(&s_b).await.is_none(),
        "unbound QUIC media is dropped"
    );
    assert!(s_a.quic().is_none());

    // A bind signed with another session's key, and one for a session the connection does
    // not own after it authenticated, are both refused.
    let forged = AurixPacket::session_bind(
        &s_a.session_id,
        s_a.ssrc,
        chrono::Utc::now().timestamp_millis(),
        1,
    )
    .encode_authenticated(&s_c.keys)
    .freeze();
    a.conn.send_datagram(forged).unwrap();
    assert!(a.recv().await.is_none(), "forged bind gets no ack");
    a.bind(&s_a).await.expect("A binds");
    assert_eq!(s_a.transport_kind(), MediaTransportKind::Quic);
    a.conn.send_datagram(Raw::bind_bytes(&s_c, 9)).unwrap();
    assert!(a.recv().await.is_none(), "a connection owns one session");
    assert!(s_c.quic().is_none());
    assert_eq!(s_a.quic().unwrap().session_id(), Some(s_a.session_id));

    // Authenticated media flows between the two connections; the SSRC must match.
    a.send_audio(&s_a, &channel, b"hello");
    let got = b.recv_audio(&s_b).await.expect("B hears A");
    assert_eq!(got.header.ssrc, s_a.ssrc);
    assert_eq!(&got.payload[..], b"hello");
    let seq = a.next_seq();
    let spoofed = AurixPacket::audio(
        seq,
        seq * 960,
        s_c.ssrc,
        channel_id_hash(&channel),
        Bytes::from_static(b"spoof"),
    )
    .seal(&s_a.keys)
    .freeze();
    a.conn.send_datagram(spoofed).unwrap();
    assert!(
        b.recv_audio(&s_b).await.is_none(),
        "SSRC mismatch is dropped"
    );

    // Replay of an already-accepted uplink packet is dropped by the anti-replay window.
    let seq = a.next_seq();
    let pkt = AurixPacket::audio(
        seq,
        seq * 960,
        s_a.ssrc,
        channel_id_hash(&channel),
        Bytes::from_static(b"once"),
    )
    .seal(&s_a.keys)
    .freeze();
    a.conn.send_datagram(pkt.clone()).unwrap();
    assert!(b.recv_audio(&s_b).await.is_some());
    a.conn.send_datagram(pkt).unwrap();
    assert!(
        b.recv_audio(&s_b).await.is_none(),
        "replayed datagram dropped"
    );

    // A new connection for A supersedes the first: the old one is closed by the node and
    // whatever was in flight on it is not media anymore.
    let mut a2 = Raw::connect(addr, &info, &tls).await;
    assert!(a2.zero_rtt, "same tls config resumes with 0-RTT");
    a2.bind(&s_a).await.expect("A rebinds on a new connection");
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(
        a.conn.close_reason().is_some(),
        "superseded connection closed"
    );
    a2.send_audio(&s_a, &channel, b"new path");
    assert_eq!(
        &b.recv_audio(&s_b).await.expect("via new path").payload[..],
        b"new path"
    );
    assert!(a2.conn.close_reason().is_none());

    // The node's count follows the live connections; closing B frees its session's path.
    assert_eq!(sfu.quic().unwrap().connection_count(), 2);
    b.conn.close(quinn::VarInt::from_u32(0), b"bye");
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(s_b.quic().is_none());
    assert_eq!(sfu.quic().unwrap().connection_count(), 1);
    drop(b.endpoint);
    drop(a.endpoint);
    drop(a2.endpoint);
    sfu.shutdown();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn replayed_early_bind_is_refused_and_zero_rtt_can_be_disabled() {
    let (sfu, addr) = start_sfu(QuicOptions::default()).await;
    let info = sfu.quic_info().unwrap();
    let tls = client_tls_config(&info).unwrap();
    let app = AppId::new();
    let channel = ChannelId::new();
    let s_a = session(&sfu, app, channel, "a");

    // First connection: 1-RTT, gets a ticket. Second: 0-RTT with the bind as early data.
    let mut first = Raw::connect(addr, &info, &tls).await;
    first.bind(&s_a).await.unwrap();
    let early_bind = Raw::bind_bytes(&s_a, 11);
    let second = Raw::connect(addr, &info, &tls).await;
    assert!(second.zero_rtt);
    second.conn.send_datagram(early_bind.clone()).unwrap();
    let mut ack = second.recv().await.expect("0-RTT bind acked");
    assert_eq!(ack.header.packet_type, PacketType::SessionBindAck);
    assert!(ack.open(&s_a.keys));

    // The very same early data on yet another 0-RTT connection (what an on-path replayer
    // can do) is not a bind: its timestamp is not newer than the accepted one.
    let replayer = Raw::connect(addr, &info, &tls).await;
    assert!(replayer.zero_rtt);
    replayer.conn.send_datagram(early_bind).unwrap();
    assert!(
        replayer.recv().await.is_none(),
        "replayed early bind refused"
    );
    assert!(s_a
        .quic()
        .is_some_and(|l| l.remote_address() == second.endpoint.local_addr().unwrap()));
    assert!(second.conn.close_reason().is_none());
    drop(first.endpoint);
    drop(second.endpoint);
    drop(replayer.endpoint);
    sfu.shutdown();

    // With 0-RTT off the node offers no early data: every connection is 1-RTT.
    let (sfu, addr) = start_sfu(QuicOptions {
        zero_rtt: false,
        ..QuicOptions::default()
    })
    .await;
    let info = sfu.quic_info().unwrap();
    let tls = client_tls_config(&info).unwrap();
    let s = session(&sfu, app, channel, "a");
    let mut one = Raw::connect(addr, &info, &tls).await;
    one.bind(&s).await.unwrap();
    let two = Raw::connect(addr, &info, &tls).await;
    assert!(!two.zero_rtt, "no early data without 0-RTT");
    drop(one.endpoint);
    drop(two.endpoint);
    sfu.shutdown();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn connection_cap_refuses_and_disabled_quic_leaves_udp_clients_alone() {
    let (sfu, addr) = start_sfu(QuicOptions {
        max_connections: 1,
        ..QuicOptions::default()
    })
    .await;
    let info = sfu.quic_info().unwrap();
    let tls = client_tls_config(&info).unwrap();
    let app = AppId::new();
    let channel = ChannelId::new();
    let s_a = session(&sfu, app, channel, "a");
    let s_b = session(&sfu, app, channel, "b");

    let mut a = Raw::connect(addr, &info, &tls).await;
    a.bind(&s_a).await.unwrap();
    let socket = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
    let mut endpoint = quinn::Endpoint::new(
        quinn::EndpointConfig::default(),
        None,
        socket,
        Arc::new(quinn::TokioRuntime),
    )
    .unwrap();
    endpoint.set_default_client_config(client_config(tls.clone(), IDLE, 128, None).unwrap());
    let refused = tokio::time::timeout(
        Duration::from_secs(2),
        endpoint.connect(addr, &info.server_name).unwrap(),
    )
    .await
    .expect("refusal arrives");
    assert!(
        matches!(refused, Err(quinn::ConnectionError::ConnectionClosed(_))),
        "{refused:?}"
    );
    assert_eq!(sfu.quic().unwrap().connection_count(), 1);

    // An old client that never heard of QUIC binds over plain UDP on the same socket, and
    // the two paths exchange media.
    let udp = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    udp.send_to(&Raw::bind_bytes(&s_b, 3), addr).await.unwrap();
    let mut buf = [0u8; 256];
    let (n, _) = tokio::time::timeout(Duration::from_secs(2), udp.recv_from(&mut buf))
        .await
        .expect("udp bind ack")
        .unwrap();
    let mut ack = AurixPacket::decode(&buf[..n]).unwrap();
    assert_eq!(ack.header.packet_type, PacketType::SessionBindAck);
    assert!(ack.open(&s_b.keys));
    assert_eq!(s_b.transport_kind(), MediaTransportKind::Udp);
    a.send_audio(&s_a, &channel, b"quic->udp");
    let (n, _) = tokio::time::timeout(Duration::from_secs(2), udp.recv_from(&mut buf))
        .await
        .expect("udp client hears quic client")
        .unwrap();
    let mut got = AurixPacket::decode(&buf[..n]).unwrap();
    assert!(got.open(&s_b.keys));
    assert_eq!(&got.payload[..], b"quic->udp");
    let pkt = AurixPacket::audio(
        1,
        960,
        s_b.ssrc,
        channel_id_hash(&channel),
        Bytes::from_static(b"udp->quic"),
    )
    .seal(&s_b.keys);
    udp.send_to(&pkt, addr).await.unwrap();
    assert_eq!(
        &a.recv_audio(&s_a)
            .await
            .expect("quic client hears udp client")
            .payload[..],
        b"udp->quic"
    );
    drop(a.endpoint);
    drop(endpoint);
    sfu.shutdown();

    // QUIC disabled: nothing advertised, and a QUIC handshake attempt at the media socket is
    // just unknown UDP that is dropped.
    let (sfu, addr) = start_sfu(QuicOptions {
        enabled: false,
        ..QuicOptions::default()
    })
    .await;
    assert!(sfu.quic_info().is_none());
    assert!(sfu.quic().is_none());
    let socket = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
    let mut endpoint = quinn::Endpoint::new(
        quinn::EndpointConfig::default(),
        None,
        socket,
        Arc::new(quinn::TokioRuntime),
    )
    .unwrap();
    endpoint.set_default_client_config(
        client_config(tls, Duration::from_millis(500), 128, None).unwrap(),
    );
    let res = tokio::time::timeout(
        Duration::from_secs(3),
        endpoint.connect(addr, &info.server_name).unwrap(),
    )
    .await
    .expect("times out at the QUIC layer");
    assert!(
        matches!(res, Err(quinn::ConnectionError::TimedOut)),
        "{res:?}"
    );
    let s = session(&sfu, app, channel, "udp");
    let udp = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    udp.send_to(&Raw::bind_bytes(&s, 5), addr).await.unwrap();
    let (n, _) = tokio::time::timeout(Duration::from_secs(2), udp.recv_from(&mut buf))
        .await
        .expect("udp still binds")
        .unwrap();
    assert_eq!(
        AurixPacket::decode(&buf[..n]).unwrap().header.packet_type,
        PacketType::SessionBindAck
    );
    drop(endpoint);
    sfu.shutdown();
}
