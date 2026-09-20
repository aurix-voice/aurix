//! End-to-end AURX routing tests over real UDP sockets on loopback.

use aurix_common::crypto::MediaKeys;
use aurix_common::protocol::*;
use aurix_common::types::*;
use aurix_media::session::Transport;
use aurix_media::{MediaEvent, SfuNode, SfuOptions};
use bytes::Bytes;
use std::net::SocketAddr;
use std::time::Duration;
use tokio::net::UdpSocket;

struct Client {
    sock: UdpSocket,
    session: std::sync::Arc<aurix_media::MediaSession>,
    seq: u32,
}

impl Client {
    async fn new(session: std::sync::Arc<aurix_media::MediaSession>) -> Self {
        let sock = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        Self {
            sock,
            session,
            seq: 1,
        }
    }

    fn next_seq(&mut self) -> u32 {
        let s = self.seq;
        self.seq += 1;
        s
    }

    async fn bind(&mut self, sfu: SocketAddr) {
        let now = chrono::Utc::now().timestamp_millis();
        let pkt = AurixPacket::session_bind(&self.session.session_id, self.session.ssrc, now, 7);
        self.sock
            .send_to(&pkt.encode_authenticated(&self.session.keys), sfu)
            .await
            .unwrap();
        let mut buf = [0u8; 256];
        let (n, _) = tokio::time::timeout(Duration::from_secs(2), self.sock.recv_from(&mut buf))
            .await
            .expect("bind ack")
            .unwrap();
        let mut ack = AurixPacket::decode(&buf[..n]).unwrap();
        assert_eq!(ack.header.packet_type, PacketType::SessionBindAck);
        assert!(
            ack.header.has_flag(PacketFlags::Encrypted),
            "ack must be encrypted"
        );
        assert!(ack.open(&self.session.keys), "ack must be authenticated");
        assert_eq!(ack.payload.len(), 8);
    }

    async fn send_audio(&mut self, sfu: SocketAddr, channel: &ChannelId, payload: &[u8]) {
        let seq = self.next_seq();
        let pkt = AurixPacket::audio(
            seq,
            seq * 960,
            self.session.ssrc,
            channel_id_hash(channel),
            Bytes::copy_from_slice(payload),
        );
        self.sock
            .send_to(&pkt.seal(&self.session.keys), sfu)
            .await
            .unwrap();
    }

    async fn send_audio_with_level(
        &mut self,
        sfu: SocketAddr,
        channel: &ChannelId,
        level: u8,
        payload: &[u8],
    ) {
        let seq = self.next_seq();
        let pkt = AurixPacket::audio_with_level(
            seq,
            seq * 960,
            self.session.ssrc,
            channel_id_hash(channel),
            level,
            payload,
        );
        self.sock
            .send_to(&pkt.seal(&self.session.keys), sfu)
            .await
            .unwrap();
    }

    /// Receive one downlink packet, asserting it is sealed for this session, and return the
    /// opened (plaintext) packet together with the ciphertext bytes as seen on the wire.
    async fn recv_raw(&self) -> Option<(AurixPacket, Vec<u8>)> {
        let mut buf = [0u8; 1500];
        match tokio::time::timeout(Duration::from_millis(300), self.sock.recv_from(&mut buf)).await
        {
            Ok(Ok((n, _))) => {
                let mut pkt = AurixPacket::decode(&buf[..n]).unwrap();
                assert!(pkt.header.has_flag(PacketFlags::Encrypted));
                let wire_payload = pkt.payload.to_vec();
                assert!(pkt.open(&self.session.keys), "downlink must verify");
                Some((pkt, wire_payload))
            }
            _ => None,
        }
    }

    async fn recv(&self) -> Option<AurixPacket> {
        self.recv_raw().await.map(|(p, _)| p)
    }
}

async fn start_sfu() -> (SfuNode, SocketAddr) {
    start_sfu_with(SfuOptions::default()).await
}

async fn start_sfu_with(options: SfuOptions) -> (SfuNode, SocketAddr) {
    let mut sfu = SfuNode::new(
        MediaNodeId::new(),
        Region::EuWest,
        SfuOptions {
            max_participants: 3,
            ..options
        },
    );
    sfu.start("127.0.0.1:0".parse().unwrap()).await.unwrap();
    let addr = sfu.local_addr().unwrap();
    (sfu, addr)
}

/// A client whose media path is the WebSocket tunnel: packets go in through the router as
/// binary frames would, and downlinks come out of the tunnel's queue.
struct TunnelClient {
    tunnel: std::sync::Arc<aurix_media::tunnel::MediaTunnel>,
    rx: tokio::sync::mpsc::Receiver<Vec<u8>>,
    session: std::sync::Arc<aurix_media::MediaSession>,
    seq: u32,
}

impl TunnelClient {
    fn open(sfu: &SfuNode, session: std::sync::Arc<aurix_media::MediaSession>) -> Self {
        let (tunnel, rx) = sfu.open_tunnel(&session.session_id).unwrap();
        Self {
            tunnel,
            rx,
            session,
            seq: 1,
        }
    }

    fn next_seq(&mut self) -> u32 {
        let s = self.seq;
        self.seq += 1;
        s
    }

    async fn push(&self, sfu: &SfuNode, wire: &[u8]) -> aurix_common::Result<()> {
        sfu.packet_router()
            .unwrap()
            .route_tunnel_packet(wire, &self.tunnel)
            .await
    }

    async fn bind(&mut self, sfu: &SfuNode) {
        let now = chrono::Utc::now().timestamp_millis();
        let pkt = AurixPacket::session_bind(&self.session.session_id, self.session.ssrc, now, 7);
        self.push(sfu, &pkt.encode_authenticated(&self.session.keys))
            .await
            .expect("tunnel bind accepted");
        let mut ack = self.recv().await.expect("bind ack over the tunnel");
        assert_eq!(ack.header.packet_type, PacketType::SessionBindAck);
        assert!(ack.header.has_flag(PacketFlags::Encrypted));
        assert!(ack.open(&self.session.keys));
    }

    async fn send_audio(&mut self, sfu: &SfuNode, channel: &ChannelId, payload: &[u8]) {
        let seq = self.next_seq();
        let pkt = AurixPacket::audio(
            seq,
            seq * 960,
            self.session.ssrc,
            channel_id_hash(channel),
            Bytes::copy_from_slice(payload),
        );
        let _ = self.push(sfu, &pkt.seal(&self.session.keys)).await;
    }

    /// Next sealed downlink frame, still closed (as it would be written to the socket).
    async fn recv(&mut self) -> Option<AurixPacket> {
        let wire = tokio::time::timeout(Duration::from_millis(300), self.rx.recv())
            .await
            .ok()??;
        Some(AurixPacket::decode(&wire).unwrap())
    }

    async fn recv_open(&mut self) -> Option<AurixPacket> {
        let mut pkt = self.recv().await?;
        assert!(pkt.header.has_flag(PacketFlags::Encrypted));
        assert!(
            pkt.open(&self.session.keys),
            "tunnel downlink must be sealed for this session"
        );
        Some(pkt)
    }
}

#[tokio::test]
async fn tunnel_carries_authenticated_media_and_binds_only_its_own_session() {
    let (sfu, addr) = start_sfu().await;
    let app = AppId::new();
    let channel = ChannelId::new();
    let s_a = sfu
        .create_session(SessionId::new(), UserId::new(), app, "a".into())
        .unwrap();
    let s_b = sfu
        .create_session(SessionId::new(), UserId::new(), app, "b".into())
        .unwrap();
    for s in [&s_a, &s_b] {
        sfu.join_channel(
            &s.session_id,
            channel,
            ChannelConfig::default(),
            ChannelRole::Speaker,
        )
        .unwrap();
    }
    let mut events = sfu.subscribe_events();
    let mut a = TunnelClient::open(&sfu, s_a.clone());
    let mut b = Client::new(s_b.clone()).await;

    // Media through an unbound tunnel is rejected (same rule as an unbound UDP source).
    a.send_audio(&sfu, &channel, b"early").await;
    assert!(b.recv().await.is_none());
    assert!(!s_a.is_tunneled());

    // A bind naming another session, even signed with that session's key, is refused: the
    // connection authenticated as A and can only ever bind A.
    let now = chrono::Utc::now().timestamp_millis();
    let foreign = AurixPacket::session_bind(&s_b.session_id, s_b.ssrc, now, 1)
        .encode_authenticated(&s_b.keys);
    assert!(a.push(&sfu, &foreign).await.is_err());
    assert!(s_b.endpoint().is_none());

    a.bind(&sfu).await;
    assert!(s_a.is_tunneled());
    assert_eq!(s_a.transport_kind(), MediaTransportKind::Tunnel);
    let bound = tokio::time::timeout(Duration::from_secs(1), events.recv())
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(
        bound,
        MediaEvent::SessionBound {
            transport: MediaTransportKind::Tunnel,
            ..
        }
    ));
    b.bind(addr).await;
    assert_eq!(s_b.transport_kind(), MediaTransportKind::Udp);

    // Tunnel → UDP and UDP → tunnel, each downlink sealed for its receiver.
    a.send_audio(&sfu, &channel, b"from-tunnel").await;
    let got = b
        .recv()
        .await
        .expect("udp receiver hears the tunneled sender");
    assert_eq!(got.header.ssrc, s_a.ssrc);
    assert_eq!(&got.payload[..], b"from-tunnel");
    b.send_audio(addr, &channel, b"from-udp").await;
    let got = a.recv_open().await.expect("tunneled receiver hears udp");
    assert_eq!(got.header.ssrc, s_b.ssrc);
    assert_eq!(&got.payload[..], b"from-udp");
    assert!(a.recv().await.is_none(), "sender must not hear itself");

    // Replay through the tunnel is dropped like on UDP (seq 2 carried "from-tunnel").
    let replay = AurixPacket::audio(
        2,
        1920,
        s_a.ssrc,
        channel_id_hash(&channel),
        Bytes::from_static(b"replay"),
    );
    assert!(a.push(&sfu, &replay.seal(&s_a.keys)).await.is_err());
    assert!(b.recv().await.is_none());

    // Wrong SSRC / wrong key / unencrypted frames are rejected on the tunnel too.
    let seq = a.next_seq();
    let imposter = AurixPacket::audio(
        seq,
        0,
        s_b.ssrc,
        channel_id_hash(&channel),
        Bytes::from_static(b"imposter"),
    );
    assert!(a.push(&sfu, &imposter.seal(&s_b.keys)).await.is_err());
    let seq = a.next_seq();
    let plain = AurixPacket::audio(
        seq,
        0,
        s_a.ssrc,
        channel_id_hash(&channel),
        Bytes::from_static(b"plain"),
    );
    assert!(a.push(&sfu, &plain.encode()).await.is_err());
    assert!(a
        .push(&sfu, &plain.encode_authenticated(&s_a.keys))
        .await
        .is_err());
    assert!(b.recv().await.is_none());

    // Heartbeat over the tunnel is answered over the tunnel.
    let mut hb = AurixPacket::heartbeat(s_a.ssrc, 4242);
    hb.header.sequence = a.next_seq();
    a.push(&sfu, &hb.seal(&s_a.keys)).await.unwrap();
    let ack = a.recv_open().await.expect("heartbeat ack over the tunnel");
    assert_eq!(ack.header.packet_type, PacketType::HeartbeatAck);

    // A UDP bind moves the session off the tunnel; the old tunnel no longer carries media.
    let mut a_udp = Client::new(s_a.clone()).await;
    a_udp.bind(addr).await;
    assert!(!s_a.is_tunneled());
    assert_eq!(s_a.transport_kind(), MediaTransportKind::Udp);
    a.send_audio(&sfu, &channel, b"stale-tunnel").await;
    assert!(b.recv().await.is_none());
    a_udp.send_audio(addr, &channel, b"via-udp").await;
    assert_eq!(&b.recv().await.unwrap().payload[..], b"via-udp");
    b.send_audio(addr, &channel, b"to-udp-now").await;
    assert!(a_udp.recv().await.is_some());
    assert!(a.recv().await.is_none(), "stale tunnel gets no downlink");

    // Closing a tunnel that is no longer the path is a no-op; closing the live one unbinds.
    assert!(!sfu.close_tunnel(&a.tunnel));
    assert_eq!(
        s_a.get_remote_addr(),
        Some(a_udp.sock.local_addr().unwrap())
    );
    let mut a2 = TunnelClient::open(&sfu, s_a.clone());
    a2.bind(&sfu).await;
    assert!(s_a.is_tunneled());
    assert!(
        s_a.get_remote_addr().is_none(),
        "tunnel bind replaces the UDP path"
    );
    a_udp.send_audio(addr, &channel, b"udp-after-tunnel").await;
    assert!(
        b.recv().await.is_none(),
        "the replaced UDP address is unbound"
    );
    assert!(sfu.close_tunnel(&a2.tunnel));
    assert!(s_a.endpoint().is_none());
    b.send_audio(addr, &channel, b"nobody-home").await;
    assert!(a2.recv().await.is_none());
}

#[tokio::test]
async fn tunnel_queue_drops_only_the_stalled_receiver() {
    let (sfu, addr) = start_sfu_with(SfuOptions {
        tunnel_queue_packets: 8,
        ..SfuOptions::default()
    })
    .await;
    let app = AppId::new();
    let channel = ChannelId::new();
    let s_a = sfu
        .create_session(SessionId::new(), UserId::new(), app, "a".into())
        .unwrap();
    let s_b = sfu
        .create_session(SessionId::new(), UserId::new(), app, "b".into())
        .unwrap();
    let s_c = sfu
        .create_session(SessionId::new(), UserId::new(), app, "c".into())
        .unwrap();
    for s in [&s_a, &s_b, &s_c] {
        sfu.join_channel(
            &s.session_id,
            channel,
            ChannelConfig::default(),
            ChannelRole::Speaker,
        )
        .unwrap();
    }
    let mut a = Client::new(s_a.clone()).await;
    let mut b = TunnelClient::open(&sfu, s_b.clone());
    let mut c = Client::new(s_c.clone()).await;
    a.bind(addr).await;
    b.bind(&sfu).await;
    c.bind(addr).await;

    // B's connection stalls (nobody drains its queue) while A keeps talking.
    for i in 0..20u8 {
        a.send_audio(addr, &channel, &[i; 40]).await;
    }
    let mut heard_c = 0;
    while c.recv().await.is_some() {
        heard_c += 1;
    }
    assert_eq!(heard_c, 20, "the healthy UDP receiver gets everything");
    let mut heard_b = 0;
    while b.recv_open().await.is_some() {
        heard_b += 1;
    }
    assert_eq!(heard_b, 8, "the stalled tunnel keeps only its queue depth");
    assert!(b.tunnel.packets_dropped() >= 12);
    assert_eq!(b.tunnel.packets_sent(), 9, "8 audio frames + the bind ack");

    // Once drained, delivery resumes.
    a.send_audio(addr, &channel, b"again").await;
    assert_eq!(&b.recv_open().await.unwrap().payload[..], b"again");
}

#[tokio::test]
async fn bound_sessions_exchange_audio_and_spoofing_is_rejected() {
    let (sfu, addr) = start_sfu().await;
    let app = AppId::new();
    let channel = ChannelId::new();

    let s_a = sfu
        .create_session(SessionId::new(), UserId::new(), app, "a".into())
        .unwrap();
    let s_b = sfu
        .create_session(SessionId::new(), UserId::new(), app, "b".into())
        .unwrap();
    sfu.join_channel(
        &s_a.session_id,
        channel,
        ChannelConfig::default(),
        ChannelRole::Speaker,
    )
    .unwrap();
    sfu.join_channel(
        &s_b.session_id,
        channel,
        ChannelConfig::default(),
        ChannelRole::Speaker,
    )
    .unwrap();

    let mut events = sfu.subscribe_events();
    let mut a = Client::new(s_a.clone()).await;
    let mut b = Client::new(s_b.clone()).await;

    // Audio before binding is dropped: the source address is unknown.
    a.send_audio(addr, &channel, b"early").await;
    assert!(b.recv().await.is_none());

    a.bind(addr).await;
    b.bind(addr).await;
    let bound = tokio::time::timeout(Duration::from_secs(1), events.recv())
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(bound, MediaEvent::SessionBound { .. }));

    a.send_audio(addr, &channel, b"hello-opus").await;
    let (got, wire) = b.recv_raw().await.expect("b receives a's audio");
    assert_eq!(got.header.ssrc, s_a.ssrc);
    assert_eq!(&got.payload[..], b"hello-opus");
    assert_ne!(
        &wire[..],
        b"hello-opus",
        "payload must be encrypted on the wire"
    );
    assert!(a.recv().await.is_none(), "sender must not hear itself");

    // Speaking event was emitted for A.
    let mut saw_speaking = false;
    while let Ok(Ok(ev)) = tokio::time::timeout(Duration::from_millis(200), events.recv()).await {
        if let MediaEvent::SpeakingChanged {
            user_id,
            speaking: true,
            ..
        } = ev
        {
            if user_id == s_a.user_id {
                saw_speaking = true;
            }
        }
    }
    assert!(saw_speaking);

    // Spoof 1: attacker from a fresh socket uses A's SSRC without the key -> dropped.
    let attacker = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let spoof = AurixPacket::audio(
        99,
        0,
        s_a.ssrc,
        channel_id_hash(&channel),
        Bytes::from_static(b"evil"),
    );
    attacker.send_to(&spoof.encode(), addr).await.unwrap();
    attacker
        .send_to(&spoof.seal(&MediaKeys::derive(b"guess")), addr)
        .await
        .unwrap();
    assert!(
        b.recv().await.is_none(),
        "spoofed audio must not be forwarded"
    );
    assert_eq!(
        s_a.get_remote_addr().unwrap(),
        a.sock.local_addr().unwrap(),
        "attacker must not steal the binding"
    );

    // Spoof 2: B (bound) tries to send with A's SSRC -> SSRC/session mismatch.
    let pkt = AurixPacket::audio(
        50,
        0,
        s_a.ssrc,
        channel_id_hash(&channel),
        Bytes::from_static(b"imposter"),
    );
    b.sock.send_to(&pkt.seal(&s_b.keys), addr).await.unwrap();
    assert!(a.recv().await.is_none());

    // Unauthenticated packet from the bound address is rejected when auth is required.
    let seq = a.next_seq();
    let pkt = AurixPacket::audio(
        seq,
        0,
        s_a.ssrc,
        channel_id_hash(&channel),
        Bytes::from_static(b"plain"),
    );
    a.sock.send_to(&pkt.encode(), addr).await.unwrap();
    assert!(b.recv().await.is_none());

    // Signed-but-unencrypted audio is rejected too: v2 requires sealed payloads.
    let seq = a.next_seq();
    let pkt = AurixPacket::audio(
        seq,
        0,
        s_a.ssrc,
        channel_id_hash(&channel),
        Bytes::from_static(b"signed-plain"),
    );
    a.sock
        .send_to(&pkt.encode_authenticated(&s_a.keys), addr)
        .await
        .unwrap();
    assert!(b.recv().await.is_none());

    // Ciphertext tampering (with a fixed-up CRC) fails the tag and is dropped.
    let seq = a.next_seq();
    let pkt = AurixPacket::audio(
        seq,
        0,
        s_a.ssrc,
        channel_id_hash(&channel),
        Bytes::from_static(b"tamper-me"),
    );
    let mut wire = pkt.seal(&s_a.keys).to_vec();
    wire[HEADER_SIZE] ^= 0x80;
    let crc = crc32fast::hash(&wire[HEADER_SIZE..HEADER_SIZE + 9]);
    wire[26..30].copy_from_slice(&crc.to_be_bytes());
    a.sock.send_to(&wire, addr).await.unwrap();
    assert!(b.recv().await.is_none());

    // Replay of an already-seen sequence is rejected.
    let replay = AurixPacket::audio(
        2,
        0,
        s_a.ssrc,
        channel_id_hash(&channel),
        Bytes::from_static(b"replay"),
    );
    a.sock.send_to(&replay.seal(&s_a.keys), addr).await.unwrap();
    assert!(b.recv().await.is_none());

    // Muted sender is not forwarded.
    sfu.server_mute_user(&s_a.user_id, true).unwrap();
    a.send_audio(addr, &channel, b"muted").await;
    assert!(b.recv().await.is_none());
    sfu.server_mute_user(&s_a.user_id, false).unwrap();

    // Leaving the channel stops delivery.
    sfu.leave_channel(&s_b.session_id, &channel).unwrap();
    a.send_audio(addr, &channel, b"gone").await;
    assert!(b.recv().await.is_none());
}

#[tokio::test]
async fn capacity_is_enforced_atomically_and_bind_replay_rejected() {
    let (sfu, addr) = start_sfu().await;
    let app = AppId::new();
    let mut sessions = Vec::new();
    for _ in 0..3 {
        sessions.push(
            sfu.create_session(SessionId::new(), UserId::new(), app, "x".into())
                .unwrap(),
        );
    }
    assert!(sfu
        .create_session(SessionId::new(), UserId::new(), app, "overflow".into())
        .is_err());
    assert_eq!(sfu.active_participants(), 3);
    sfu.destroy_session(&sessions[0].session_id).unwrap();
    assert_eq!(sfu.active_participants(), 2);

    // Replaying a captured SessionBind from a new address must not move the binding.
    let s = sessions[1].clone();
    let mut c = Client::new(s.clone()).await;
    let now = chrono::Utc::now().timestamp_millis();
    let bind =
        AurixPacket::session_bind(&s.session_id, s.ssrc, now, 1).encode_authenticated(&s.keys);
    c.sock.send_to(&bind, addr).await.unwrap();
    assert!(c.recv().await.is_some());
    let original = c.sock.local_addr().unwrap();

    let attacker = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    attacker.send_to(&bind, addr).await.unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(s.get_remote_addr().unwrap(), original);

    // Stale timestamp is rejected too.
    let stale = AurixPacket::session_bind(
        &s.session_id,
        s.ssrc,
        now - SESSION_BIND_MAX_SKEW_MS - 1000,
        2,
    )
    .encode_authenticated(&s.keys);
    attacker.send_to(&stale, addr).await.unwrap();

    // An encrypted SessionBind is refused even with the right key (must be signed-only).
    let sealed_bind = AurixPacket::session_bind(&s.session_id, s.ssrc, now + 1, 3).seal(&s.keys);
    attacker.send_to(&sealed_bind, addr).await.unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(s.get_remote_addr().unwrap(), original);
    let _ = c.next_seq();
}

#[tokio::test]
async fn cross_app_sessions_cannot_join_channel() {
    let (sfu, _addr) = start_sfu().await;
    let channel = ChannelId::new();
    let s1 = sfu
        .create_session(SessionId::new(), UserId::new(), AppId::new(), "a".into())
        .unwrap();
    let s2 = sfu
        .create_session(SessionId::new(), UserId::new(), AppId::new(), "b".into())
        .unwrap();
    sfu.join_channel(
        &s1.session_id,
        channel,
        ChannelConfig::default(),
        ChannelRole::Speaker,
    )
    .unwrap();
    assert!(sfu
        .join_channel(
            &s2.session_id,
            channel,
            ChannelConfig::default(),
            ChannelRole::Speaker
        )
        .is_err());
}

/// Operator edits to a live channel reach the sessions in it, are tenant-scoped, cannot
/// change the channel type, and the per-sender policy is the merge over joined channels.
#[tokio::test]
async fn e2ee_channel_admits_only_encrypted_frames_for_capable_receivers() {
    use aurix_media::router::InjectedFrame;

    let (sfu, addr) = start_sfu().await;
    let app = AppId::new();
    let channel = ChannelId::new();
    let cfg = ChannelConfig {
        e2ee: true,
        ..ChannelConfig::default()
    };
    let mut sessions = Vec::new();
    for name in ["alice", "bob", "legacy"] {
        let s = sfu
            .create_session(SessionId::new(), UserId::new(), app, name.into())
            .unwrap();
        sfu.join_channel(&s.session_id, channel, cfg.clone(), ChannelRole::Speaker)
            .unwrap();
        sessions.push(s);
    }
    let (s_a, s_b, s_l) = (sessions.remove(0), sessions.remove(0), sessions.remove(0));
    s_a.set_e2ee_capable(true);
    s_b.set_e2ee_capable(true);
    let mut a = Client::new(s_a.clone()).await;
    let mut b = Client::new(s_b.clone()).await;
    let mut l = Client::new(s_l.clone()).await;
    a.bind(addr).await;
    b.bind(addr).await;
    l.bind(addr).await;

    // Plaintext into an encrypted channel is dropped for everyone.
    a.send_audio(addr, &channel, b"plain").await;
    assert!(b.recv().await.is_none());
    assert!(l.recv().await.is_none());

    // Encrypted frames reach capable members untouched and skip the legacy client.
    let seq = a.next_seq();
    let mut pkt = AurixPacket::audio(
        seq,
        seq * 960,
        s_a.ssrc,
        channel_id_hash(&channel),
        Bytes::from_static(b"sealed"),
    );
    pkt.header.flags |= PacketFlags::E2ee as u16;
    a.sock.send_to(&pkt.seal(&s_a.keys), addr).await.unwrap();
    let got = b.recv().await.expect("capable receiver");
    assert!(got.header.has_flag(PacketFlags::E2ee));
    assert_eq!(got.header.ssrc, s_a.ssrc);
    assert_eq!(&got.payload[..], b"sealed");
    assert!(l.recv().await.is_none(), "legacy client must get nothing");

    // Server-side audio has no sender key: it cannot be injected.
    let router = sfu.router().unwrap();
    let frame = || InjectedFrame {
        ssrc: 0xE2EE,
        sequence: 1,
        timestamp: 960,
        payload: Bytes::from_static(b"tts"),
    };
    assert!(router
        .inject_system_audio(&app, &channel, frame())
        .await
        .is_err());
    assert!(router
        .inject_participant_audio(&s_a, &channel, frame(), true, false)
        .await
        .is_err());
    assert!(b.recv().await.is_none());
}

#[tokio::test]
async fn channel_config_updates_apply_live_and_merge_per_sender() {
    let (sfu, _addr) = start_sfu().await;
    let app = AppId::new();
    let voice = ChannelId::new();
    let music = ChannelId::new();
    let s1 = sfu
        .create_session(SessionId::new(), UserId::new(), app, "a".into())
        .unwrap();
    let s2 = sfu
        .create_session(SessionId::new(), UserId::new(), app, "b".into())
        .unwrap();
    let voice_cfg = ChannelConfig {
        bitrate: 24_000,
        min_bitrate: 8_000,
        max_bandwidth: OpusBandwidth::Wideband,
        complexity: Some(5),
        ..ChannelConfig::default()
    };
    for s in [&s1, &s2] {
        sfu.join_channel(
            &s.session_id,
            voice,
            voice_cfg.clone(),
            ChannelRole::Speaker,
        )
        .unwrap();
    }
    let music_cfg = ChannelConfig {
        bitrate: 96_000,
        min_bitrate: 32_000,
        enable_dtx: false,
        audio_profile: AudioProfile::Music,
        channel_type: ChannelType::Echo,
        ..ChannelConfig::default()
    };
    sfu.join_channel(
        &s1.session_id,
        music,
        music_cfg.clone(),
        ChannelRole::Speaker,
    )
    .unwrap();

    let p1 = sfu.session_audio_policy(&s1);
    assert_eq!(p1, voice_cfg.audio_policy().merge(music_cfg.audio_policy()));
    assert_eq!((p1.bitrate_bps, p1.signal), (96_000, OpusSignal::Music));
    assert_eq!(sfu.session_audio_policy(&s2), voice_cfg.audio_policy());

    // Operator lowers the voice channel and tries to flip its type: sessions in the channel
    // are returned for notification, the type stays.
    let mut edited = voice_cfg.clone();
    edited.bitrate = 16_000;
    edited.min_bitrate = 6_000;
    edited.channel_type = ChannelType::Positional;
    let affected = sfu
        .update_channel_config(&voice, &app, edited.clone())
        .expect("channel is live on this node");
    let ids: std::collections::HashSet<_> = affected.iter().map(|s| s.session_id).collect();
    assert_eq!(ids, [s1.session_id, s2.session_id].into_iter().collect());
    let live = sfu.get_channel(&voice).unwrap();
    assert_eq!(live.config().bitrate, 16_000);
    assert_eq!(live.config().channel_type, ChannelType::Team);
    assert_eq!(live.channel_type, ChannelType::Team);
    assert_eq!(sfu.session_audio_policy(&s2).bitrate_bps, 16_000);
    assert_eq!(
        sfu.session_audio_policy(&s1).bitrate_bps,
        96_000,
        "music channel still dominates s1's uplink"
    );

    // Another tenant cannot touch it; an unknown channel is not live.
    assert!(sfu
        .update_channel_config(&voice, &AppId::new(), edited.clone())
        .is_none());
    assert_eq!(sfu.get_channel(&voice).unwrap().config().bitrate, 16_000);
    assert!(sfu
        .update_channel_config(&ChannelId::new(), &app, edited)
        .is_none());

    // Leaving the music channel drops s1 back to the (edited) voice policy.
    sfu.leave_channel(&s1.session_id, &music).unwrap();
    assert_eq!(
        sfu.session_audio_policy(&s1),
        sfu.get_channel(&voice).unwrap().audio_policy()
    );
}

/// Echo channel (microphone test): each participant's frames come straight back to them,
/// sealed like any downlink, and nobody else in the channel hears them.
#[tokio::test]
async fn echo_channel_loops_audio_back_to_the_sender_only() {
    let (sfu, addr) = start_sfu().await;
    let app = AppId::new();
    let channel = ChannelId::new();
    let cfg = ChannelConfig {
        channel_type: ChannelType::Echo,
        ..ChannelConfig::default()
    };
    let mut clients = Vec::new();
    for name in ["alice", "bob"] {
        let s = sfu
            .create_session(SessionId::new(), UserId::new(), app, name.into())
            .unwrap();
        sfu.join_channel(&s.session_id, channel, cfg.clone(), ChannelRole::Speaker)
            .unwrap();
        let mut c = Client::new(s).await;
        c.bind(addr).await;
        clients.push(c);
    }
    let (mut alice, bob) = (clients.remove(0), clients.remove(0));

    alice.send_audio(addr, &channel, b"mic-test").await;
    let back = alice.recv().await.expect("alice hears herself");
    assert_eq!(back.header.packet_type, PacketType::Audio);
    assert_eq!(back.header.ssrc, alice.session.ssrc);
    assert!(
        !back.header.has_flag(PacketFlags::VolumeAttenuated),
        "loopback is unity gain"
    );
    assert_eq!(&back.payload[..], b"mic-test");
    assert!(
        bob.recv().await.is_none(),
        "bob never hears alice's mic test"
    );

    // Receiver-local volume shapes the loopback like any other downlink.
    alice
        .session
        .prefs
        .write()
        .set_gain(alice.session.user_id, 0.5);
    alice.send_audio(addr, &channel, b"quieter").await;
    let mut back = alice.recv().await.unwrap();
    assert!(back.header.has_flag(PacketFlags::VolumeAttenuated));
    let (volume, direction) = back.take_downlink_meta();
    assert!((volume - 0.5).abs() < 0.01, "{volume}");
    assert!(direction.is_none());
    assert_eq!(&back.payload[..], b"quieter");

    // A browser in the same echo channel is equally private: its frames go nowhere else.
    let web = sfu
        .create_session(SessionId::new(), UserId::new(), app, "web".into())
        .unwrap();
    web.set_transport(Transport::WebRtc);
    sfu.join_channel(&web.session_id, channel, cfg, ChannelRole::Speaker)
        .unwrap();
    sfu.route_webrtc_audio(&web.session_id, 960, b"web-test".to_vec(), None)
        .await
        .unwrap();
    assert!(alice.recv().await.is_none());
    assert!(bob.recv().await.is_none());
}

/// Receiver-side preferences: a local mute (per channel or everywhere), a per-participant gain
/// and a cross-mute all shape the downlink of the receiver only; other listeners and the
/// sender's own state are untouched.
#[tokio::test]
async fn receiver_preferences_filter_and_scale_downlink() {
    let (sfu, addr) = start_sfu().await;
    let app = AppId::new();
    let team = ChannelId::new();
    let party = ChannelId::new();

    let s_a = sfu
        .create_session(SessionId::new(), UserId::new(), app, "a".into())
        .unwrap();
    let s_b = sfu
        .create_session(SessionId::new(), UserId::new(), app, "b".into())
        .unwrap();
    let s_c = sfu
        .create_session(SessionId::new(), UserId::new(), app, "c".into())
        .unwrap();
    for s in [&s_a, &s_b, &s_c] {
        for ch in [team, party] {
            sfu.join_channel(
                &s.session_id,
                ch,
                ChannelConfig::default(),
                ChannelRole::Speaker,
            )
            .unwrap();
        }
    }
    let mut a = Client::new(s_a.clone()).await;
    let mut b = Client::new(s_b.clone()).await;
    let mut c = Client::new(s_c.clone()).await;
    a.bind(addr).await;
    b.bind(addr).await;
    c.bind(addr).await;

    async fn hears(client: &Client, sender: &Client) -> Option<AurixPacket> {
        client
            .recv()
            .await
            .filter(|p| p.header.ssrc == sender.session.ssrc)
    }

    // Baseline: both hear A at full volume.
    a.send_audio(addr, &team, b"t1").await;
    assert_eq!(&hears(&b, &a).await.unwrap().payload[..], b"t1");
    assert_eq!(&hears(&c, &a).await.unwrap().payload[..], b"t1");

    // B mutes A in `team` only: silence in team, still audible in party; C unaffected.
    s_b.prefs.write().set_muted(s_a.user_id, Some(team), true);
    a.send_audio(addr, &team, b"t2").await;
    assert!(hears(&b, &a).await.is_none(), "channel-scoped local mute");
    assert_eq!(&hears(&c, &a).await.unwrap().payload[..], b"t2");
    a.send_audio(addr, &party, b"p1").await;
    assert_eq!(&hears(&b, &a).await.unwrap().payload[..], b"p1");
    assert_eq!(&hears(&c, &a).await.unwrap().payload[..], b"p1");
    assert!(
        s_a.is_transmitting_allowed(),
        "a local mute must not touch the sender's own mute state"
    );

    // Mute everywhere, then clearing the global mute also clears the channel-scoped one.
    s_b.prefs.write().set_muted(s_a.user_id, None, true);
    a.send_audio(addr, &party, b"p2").await;
    assert!(hears(&b, &a).await.is_none(), "all-channel local mute");
    assert_eq!(&hears(&c, &a).await.unwrap().payload[..], b"p2");
    s_b.prefs.write().set_muted(s_a.user_id, None, false);
    a.send_audio(addr, &team, b"t3").await;
    assert_eq!(&hears(&b, &a).await.unwrap().payload[..], b"t3");
    assert_eq!(&hears(&c, &a).await.unwrap().payload[..], b"t3");

    // Per-participant gain rides in the VolumeAttenuated byte, for B only.
    s_b.prefs.write().set_gain(s_a.user_id, 0.5);
    a.send_audio(addr, &team, b"t4").await;
    let got = hears(&b, &a).await.unwrap();
    assert!(got.header.has_flag(PacketFlags::VolumeAttenuated));
    assert_eq!(got.payload[0], encode_volume_byte(0.5));
    assert_eq!(&got.payload[1..], b"t4");
    let got = hears(&c, &a).await.unwrap();
    assert!(!got.header.has_flag(PacketFlags::VolumeAttenuated));
    assert_eq!(&got.payload[..], b"t4");
    // Boost above unity is allowed up to the cap, out-of-range values are clamped.
    s_b.prefs.write().set_gain(s_a.user_id, 9.0);
    a.send_audio(addr, &team, b"t5").await;
    assert_eq!(hears(&b, &a).await.unwrap().payload[0], 255);
    assert_eq!(&hears(&c, &a).await.unwrap().payload[..], b"t5");
    s_b.prefs.write().set_gain(s_a.user_id, 1.0);

    // Cross-mute is mutual: B blocked A -> B hears nothing from A and A hears nothing from B.
    s_b.prefs.write().set_blocked(s_a.user_id, true);
    s_a.prefs.write().set_blocked_by(s_b.user_id, true);
    a.send_audio(addr, &party, b"p3").await;
    assert!(
        hears(&b, &a).await.is_none(),
        "blocker does not hear target"
    );
    assert_eq!(&hears(&c, &a).await.unwrap().payload[..], b"p3");
    b.send_audio(addr, &party, b"from-b").await;
    assert!(
        hears(&a, &b).await.is_none(),
        "target does not hear blocker"
    );
    assert_eq!(&hears(&c, &b).await.unwrap().payload[..], b"from-b");
    s_b.prefs.write().set_blocked(s_a.user_id, false);
    s_a.prefs.write().set_blocked_by(s_b.user_id, false);
    a.send_audio(addr, &party, b"p4").await;
    assert_eq!(&hears(&b, &a).await.unwrap().payload[..], b"p4");
    assert_eq!(&hears(&c, &a).await.unwrap().payload[..], b"p4");
}

/// Multi-channel sessions: the transmission mode decides which joined channels receive the
/// uplink, focus attenuates every other channel on the receiver side, and the node caps how
/// many (positional) channels one session may join.
#[tokio::test]
async fn transmission_mode_focus_and_session_channel_limits() {
    let mut sfu = SfuNode::new(
        MediaNodeId::new(),
        Region::EuWest,
        SfuOptions {
            max_participants: 3,
            max_channels_per_session: 3,
            max_positional_channels_per_session: 1,
            unfocused_channel_gain: 0.25,
            ..SfuOptions::default()
        },
    );
    sfu.start("127.0.0.1:0".parse().unwrap()).await.unwrap();
    let addr = sfu.local_addr().unwrap();
    let app = AppId::new();
    let team = ChannelId::new();
    let party = ChannelId::new();

    let s_a = sfu
        .create_session(SessionId::new(), UserId::new(), app, "a".into())
        .unwrap();
    let s_b = sfu
        .create_session(SessionId::new(), UserId::new(), app, "b".into())
        .unwrap();
    for s in [&s_a, &s_b] {
        for ch in [team, party] {
            sfu.join_channel(
                &s.session_id,
                ch,
                ChannelConfig::default(),
                ChannelRole::Speaker,
            )
            .unwrap();
        }
    }
    let mut a = Client::new(s_a.clone()).await;
    let mut b = Client::new(s_b.clone()).await;
    a.bind(addr).await;
    b.bind(addr).await;

    // Default `All`: both channels are forwarded.
    a.send_audio(addr, &team, b"t1").await;
    assert_eq!(&b.recv().await.unwrap().payload[..], b"t1");
    a.send_audio(addr, &party, b"p1").await;
    assert_eq!(&b.recv().await.unwrap().payload[..], b"p1");

    // `Single(team)`: party frames are dropped, team frames still go through.
    s_a.set_transmission(TransmissionMode::Single { channel_id: team })
        .unwrap();
    a.send_audio(addr, &party, b"p2").await;
    assert!(b.recv().await.is_none(), "single mode drops other channels");
    a.send_audio(addr, &team, b"t2").await;
    assert_eq!(&b.recv().await.unwrap().payload[..], b"t2");

    // `None`: nothing is forwarded.
    s_a.set_transmission(TransmissionMode::None).unwrap();
    a.send_audio(addr, &team, b"t3").await;
    assert!(b.recv().await.is_none());
    s_a.set_transmission(TransmissionMode::All).unwrap();

    // Focus on `team` at the receiver: party audio arrives attenuated, team at unity.
    s_b.set_focus(Some(team)).unwrap();
    a.send_audio(addr, &party, b"p3").await;
    let got = b.recv().await.unwrap();
    assert!(got.header.has_flag(PacketFlags::VolumeAttenuated));
    assert_eq!(got.payload[0], encode_volume_byte(0.25));
    assert_eq!(&got.payload[1..], b"p3");
    a.send_audio(addr, &team, b"t4").await;
    let got = b.recv().await.unwrap();
    assert!(!got.header.has_flag(PacketFlags::VolumeAttenuated));
    assert_eq!(&got.payload[..], b"t4");
    // Focus stacks with per-sender gain.
    s_b.prefs.write().set_gain(s_a.user_id, 2.0);
    a.send_audio(addr, &party, b"p4").await;
    assert_eq!(b.recv().await.unwrap().payload[0], encode_volume_byte(0.5));
    s_b.prefs.write().set_gain(s_a.user_id, 1.0);

    // Leaving the focused / single-target channel resets both, reported to the caller.
    s_a.set_transmission(TransmissionMode::Single { channel_id: team })
        .unwrap();
    let left = sfu.leave_channel(&s_a.session_id, &team).unwrap();
    assert!(left.transmission_reset && !left.focus_reset);
    assert_eq!(s_a.transmission(), TransmissionMode::None);
    let left = sfu.leave_channel(&s_b.session_id, &team).unwrap();
    assert!(!left.transmission_reset && left.focus_reset);
    assert!(s_b.focus().is_none());
    a.send_audio(addr, &party, b"p5").await;
    assert!(b.recv().await.is_none(), "transmission fell back to none");
    s_a.set_transmission(TransmissionMode::All).unwrap();
    a.send_audio(addr, &party, b"p6").await;
    let got = b.recv().await.unwrap();
    assert!(!got.header.has_flag(PacketFlags::VolumeAttenuated));
    assert_eq!(&got.payload[..], b"p6");

    // Per-session limits: 3 channels total, 1 positional. A is in `party` only now.
    let positional = ChannelConfig {
        channel_type: ChannelType::Positional,
        ..ChannelConfig::default()
    };
    sfu.join_channel(
        &s_a.session_id,
        ChannelId::new(),
        positional.clone(),
        ChannelRole::Speaker,
    )
    .unwrap();
    let err = sfu
        .join_channel(
            &s_a.session_id,
            ChannelId::new(),
            positional,
            ChannelRole::Speaker,
        )
        .unwrap_err();
    assert_eq!(err.error_code(), "CHANNEL_LIMIT_EXCEEDED");
    sfu.join_channel(
        &s_a.session_id,
        ChannelId::new(),
        ChannelConfig::default(),
        ChannelRole::Speaker,
    )
    .unwrap();
    assert_eq!(s_a.get_channels().len(), 3);
    let err = sfu
        .join_channel(
            &s_a.session_id,
            ChannelId::new(),
            ChannelConfig::default(),
            ChannelRole::Speaker,
        )
        .unwrap_err();
    assert_eq!(err.error_code(), "CHANNEL_LIMIT_EXCEEDED");
    // Re-joining an already joined channel is idempotent and not counted twice.
    sfu.join_channel(
        &s_a.session_id,
        party,
        ChannelConfig::default(),
        ChannelRole::Speaker,
    )
    .unwrap();
    assert_eq!(s_a.get_channels().len(), 3);
}

/// In a directional positional channel every receiver gets the speaker's direction in its own
/// frame of reference (`Directional` metadata ahead of the frame), stacked with distance and
/// per-participant gain in the `VolumeAttenuated` byte. E2EE frames are forwarded untouched.
#[tokio::test]
async fn directional_positional_downlink_carries_listener_relative_direction() {
    let (sfu, addr) = start_sfu().await;
    let app = AppId::new();
    let world = ChannelId::new();
    let cfg = ChannelConfig {
        channel_type: ChannelType::Positional,
        positional_config: Some(PositionalConfig {
            near_distance: 10.0,
            far_distance: 50.0,
            rolloff: RolloffCurve::Linear,
            max_radius: 100.0,
            ..PositionalConfig::default()
        }),
        ..ChannelConfig::default()
    };
    let s_a = sfu
        .create_session(SessionId::new(), UserId::new(), app, "a".into())
        .unwrap();
    let s_b = sfu
        .create_session(SessionId::new(), UserId::new(), app, "b".into())
        .unwrap();
    let s_c = sfu
        .create_session(SessionId::new(), UserId::new(), app, "c".into())
        .unwrap();
    for s in [&s_a, &s_b, &s_c] {
        sfu.join_channel(&s.session_id, world, cfg.clone(), ChannelRole::Speaker)
            .unwrap();
        s.set_e2ee_capable(true);
    }
    let mut a = Client::new(s_a.clone()).await;
    let mut b = Client::new(s_b.clone()).await;
    let mut c = Client::new(s_c.clone()).await;
    a.bind(addr).await;
    b.bind(addr).await;
    c.bind(addr).await;

    let facing = |x: f32, z: f32| Orientation3D {
        forward_x: x,
        forward_y: 0.0,
        forward_z: z,
        up_x: 0.0,
        up_y: 1.0,
        up_z: 0.0,
    };
    // Nobody has a pose yet: a positional channel cannot place the speaker, nothing is sent.
    a.send_audio(addr, &world, b"d0").await;
    assert!(b.recv().await.is_none());
    assert!(c.recv().await.is_none());

    // A stands 5 m to +X of the origin; B looks down +Z (A on the right), C looks down +X
    // (A straight ahead).
    sfu.update_position(
        &s_a.user_id,
        &world,
        Position3D::new(5.0, 0.0, 0.0),
        facing(0.0, 1.0),
    );
    sfu.update_position(
        &s_b.user_id,
        &world,
        Position3D::new(0.0, 0.0, 0.0),
        facing(0.0, 1.0),
    );
    sfu.update_position(
        &s_c.user_id,
        &world,
        Position3D::new(0.0, 0.0, 0.0),
        facing(1.0, 0.0),
    );
    a.send_audio(addr, &world, b"d1").await;
    let got = b.recv().await.unwrap();
    assert!(got.header.has_flag(PacketFlags::Directional));
    assert!(!got.header.has_flag(PacketFlags::VolumeAttenuated));
    let dir = decode_direction([got.payload[0], got.payload[1]]);
    assert!(
        (dir.azimuth - std::f32::consts::FRAC_PI_2).abs() < 0.03,
        "{dir:?}"
    );
    assert!(dir.elevation.abs() < 0.03);
    assert_eq!(&got.payload[2..], b"d1");
    let got = c.recv().await.unwrap();
    let dir = decode_direction([got.payload[0], got.payload[1]]);
    assert!(dir.azimuth.abs() < 0.03, "{dir:?}");
    assert_eq!(&got.payload[2..], b"d1");

    // The receiver's own helper strips both kinds of metadata in one go.
    let mut got = got;
    let (vol, d) = got.take_downlink_meta();
    assert_eq!(vol, 1.0);
    assert!(d.unwrap().azimuth.abs() < 0.03);
    assert_eq!(&got.payload[..], b"d1");

    // Move A behind-left of B and out to 30 m: linear rolloff 0.5 x B's gain 0.5 = 0.25,
    // direction in front of the gain byte.
    s_b.prefs.write().set_gain(s_a.user_id, 0.5);
    sfu.update_position(
        &s_a.user_id,
        &world,
        Position3D::new(-30.0, 0.0, 0.0),
        facing(0.0, 1.0),
    );
    a.send_audio(addr, &world, b"d2").await;
    let mut got = b.recv().await.unwrap();
    assert!(got.header.has_flag(PacketFlags::Directional));
    assert!(got.header.has_flag(PacketFlags::VolumeAttenuated));
    assert_eq!(got.payload[0], encode_volume_byte(0.25));
    let (vol, d) = got.take_downlink_meta();
    assert!((vol - 0.25).abs() < 0.01, "{vol}");
    assert!((d.unwrap().azimuth + std::f32::consts::FRAC_PI_2).abs() < 0.03);
    assert_eq!(&got.payload[..], b"d2");
    assert!(
        !got.header.has_flag(PacketFlags::Directional)
            && !got.header.has_flag(PacketFlags::VolumeAttenuated)
    );
    let (vol, _) = c.recv().await.unwrap().take_downlink_meta();
    assert!((vol - 0.5).abs() < 0.01, "C has no local gain: {vol}");

    // Beyond max_radius nobody hears A at all.
    sfu.update_position(
        &s_a.user_id,
        &world,
        Position3D::new(500.0, 0.0, 0.0),
        facing(0.0, 1.0),
    );
    a.send_audio(addr, &world, b"d3").await;
    assert!(b.recv().await.is_none());
    assert!(c.recv().await.is_none());

    // E2EE frames cannot be re-scaled and carry no server metadata.
    sfu.update_position(
        &s_a.user_id,
        &world,
        Position3D::new(5.0, 0.0, 0.0),
        facing(0.0, 1.0),
    );
    let seq = a.next_seq();
    let mut pkt = AurixPacket::audio(
        seq,
        seq * 960,
        s_a.ssrc,
        channel_id_hash(&world),
        Bytes::from_static(b"e2ee"),
    );
    pkt.header.flags |= PacketFlags::E2ee as u16;
    a.sock.send_to(&pkt.seal(&s_a.keys), addr).await.unwrap();
    let got = b.recv().await.unwrap();
    assert!(got.header.has_flag(PacketFlags::E2ee));
    assert!(!got.header.has_flag(PacketFlags::Directional));
    assert!(!got.header.has_flag(PacketFlags::VolumeAttenuated));
    assert_eq!(&got.payload[..], b"e2ee");
}

/// A WebRTC uplink carries one frame for every channel the browser transmits to. A receiver
/// sharing several of those channels with the sender must hear it exactly once — via the
/// channel where it is loudest for that receiver — and the transmission mode decides which
/// channels (and therefore which receivers) are reached at all.
#[tokio::test]
async fn webrtc_frame_reaches_shared_receiver_once_via_loudest_channel() {
    let mut sfu = SfuNode::new(
        MediaNodeId::new(),
        Region::EuWest,
        SfuOptions {
            max_participants: 3,
            unfocused_channel_gain: 0.25,
            ..SfuOptions::default()
        },
    );
    sfu.start("127.0.0.1:0".parse().unwrap()).await.unwrap();
    let addr = sfu.local_addr().unwrap();
    let app = AppId::new();
    let team = ChannelId::new();
    let party = ChannelId::new();

    let s_w = sfu
        .create_session(SessionId::new(), UserId::new(), app, "web".into())
        .unwrap();
    s_w.set_transport(Transport::WebRtc);
    let s_b = sfu
        .create_session(SessionId::new(), UserId::new(), app, "b".into())
        .unwrap();
    let s_c = sfu
        .create_session(SessionId::new(), UserId::new(), app, "c".into())
        .unwrap();
    for (s, chans) in [
        (&s_w, vec![team, party]),
        (&s_b, vec![team, party]),
        (&s_c, vec![party]),
    ] {
        for ch in chans {
            sfu.join_channel(
                &s.session_id,
                ch,
                ChannelConfig::default(),
                ChannelRole::Speaker,
            )
            .unwrap();
        }
    }
    let mut b = Client::new(s_b.clone()).await;
    let mut c = Client::new(s_c.clone()).await;
    b.bind(addr).await;
    c.bind(addr).await;

    // `All`, no focus: B (in both channels) gets a single copy, C (party only) gets one too.
    sfu.route_webrtc_audio(&s_w.session_id, 960, b"w1".to_vec(), None)
        .await
        .unwrap();
    let got = b.recv().await.unwrap();
    assert_eq!(&got.payload[..], b"w1");
    assert!(
        b.recv().await.is_none(),
        "shared receiver must not get a duplicate"
    );
    assert_eq!(&c.recv().await.unwrap().payload[..], b"w1");
    assert!(c.recv().await.is_none());

    // B focuses `party`: the one copy comes through `party` at unity, not through `team` at 0.25.
    s_b.set_focus(Some(party)).unwrap();
    sfu.route_webrtc_audio(&s_w.session_id, 1920, b"w2".to_vec(), None)
        .await
        .unwrap();
    let got = b.recv().await.unwrap();
    assert!(!got.header.has_flag(PacketFlags::VolumeAttenuated));
    assert_eq!(got.header.channel_id_hash, channel_id_hash(&party));
    assert_eq!(&got.payload[..], b"w2");
    assert!(b.recv().await.is_none());
    c.recv().await.unwrap();

    // Sender restricts to `team`: B now hears it through `team`, attenuated; C hears nothing.
    s_w.set_transmission(TransmissionMode::Single { channel_id: team })
        .unwrap();
    sfu.route_webrtc_audio(&s_w.session_id, 2880, b"w3".to_vec(), None)
        .await
        .unwrap();
    let got = b.recv().await.unwrap();
    assert!(got.header.has_flag(PacketFlags::VolumeAttenuated));
    assert_eq!(got.header.channel_id_hash, channel_id_hash(&team));
    assert_eq!(got.payload[0], encode_volume_byte(0.25));
    assert_eq!(&got.payload[1..], b"w3");
    assert!(b.recv().await.is_none());
    assert!(
        c.recv().await.is_none(),
        "party is outside the sender's mode"
    );

    // `None`: nobody hears the browser.
    s_w.set_transmission(TransmissionMode::None).unwrap();
    sfu.route_webrtc_audio(&s_w.session_id, 3840, b"w4".to_vec(), None)
        .await
        .unwrap();
    assert!(b.recv().await.is_none());
    assert!(c.recv().await.is_none());
}

/// Client-measured audio levels: the level byte never reaches other participants, quiet frames
/// keep the sender out of the speaking state, loud ones put it in, and the node periodically
/// reports changed levels per channel (including the drop back to silence).
#[tokio::test]
async fn audio_levels_drive_speaking_and_energy_reports() {
    let mut sfu = SfuNode::new(
        MediaNodeId::new(),
        Region::EuWest,
        SfuOptions {
            max_participants: 3,
            speaking_timeout_ms: 200,
            speaking_energy_threshold: 0.01, // -40 dBov
            energy_interval_ms: 50,
            ..SfuOptions::default()
        },
    );
    sfu.start("127.0.0.1:0".parse().unwrap()).await.unwrap();
    let addr = sfu.local_addr().unwrap();
    let app = AppId::new();
    let channel = ChannelId::new();

    let s_a = sfu
        .create_session(SessionId::new(), UserId::new(), app, "a".into())
        .unwrap();
    let s_b = sfu
        .create_session(SessionId::new(), UserId::new(), app, "b".into())
        .unwrap();
    for s in [&s_a, &s_b] {
        sfu.join_channel(
            &s.session_id,
            channel,
            ChannelConfig::default(),
            ChannelRole::Speaker,
        )
        .unwrap();
    }
    let mut events = sfu.subscribe_events();
    let mut a = Client::new(s_a.clone()).await;
    let mut b = Client::new(s_b.clone()).await;
    a.bind(addr).await;
    b.bind(addr).await;

    async fn drain(
        events: &mut tokio::sync::broadcast::Receiver<MediaEvent>,
        for_ms: u64,
    ) -> Vec<MediaEvent> {
        let mut out = Vec::new();
        let deadline = tokio::time::Instant::now() + Duration::from_millis(for_ms);
        while let Ok(Ok(ev)) = tokio::time::timeout_at(deadline, events.recv()).await {
            out.push(ev);
        }
        out
    }
    fn speaking_of(events: &[MediaEvent], user: UserId) -> Vec<bool> {
        events
            .iter()
            .filter_map(|e| match e {
                MediaEvent::SpeakingChanged {
                    user_id, speaking, ..
                } if *user_id == user => Some(*speaking),
                _ => None,
            })
            .collect()
    }
    fn energy_of(events: &[MediaEvent], ch: ChannelId, user: UserId) -> Vec<u8> {
        events
            .iter()
            .filter_map(|e| match e {
                MediaEvent::ChannelEnergy {
                    channel_id, levels, ..
                } if *channel_id == ch => levels.iter().find(|(u, _)| *u == user).map(|(_, l)| *l),
                _ => None,
            })
            .collect()
    }
    drain(&mut events, 100).await; // SessionBound etc.

    // Levels go stale after 2 × interval (100 ms) and decay to a single silence report; each
    // phase below either waits that decay out or stops well before it can fire.
    const AFTER_DECAY_MS: u64 = 300;
    const BEFORE_DECAY_MS: u64 = 80;

    // Quiet frames (-60 dBov): forwarded as audio, level byte stripped, but not "speaking".
    for _ in 0..5 {
        a.send_audio_with_level(addr, &channel, 60, b"quiet-opus")
            .await;
    }
    let got = b.recv().await.expect("b hears quiet audio");
    assert!(!got.header.has_flag(PacketFlags::Energy));
    assert_eq!(&got.payload[..], b"quiet-opus");
    let evs = drain(&mut events, AFTER_DECAY_MS).await;
    assert!(
        speaking_of(&evs, s_a.user_id).is_empty(),
        "quiet labelled frames must not flip speaking: {evs:?}"
    );
    let quiet = energy_of(&evs, channel, s_a.user_id);
    assert!(
        quiet.len() == 2 && quiet[0] == 60 && quiet[1] == 127,
        "level of quiet frames is still reported: {quiet:?}"
    );
    assert!(
        energy_of(&evs, channel, s_b.user_id).is_empty(),
        "silent participants are not reported"
    );

    // Loud frames (-6 dBov): speaking starts and the level is reported once; when the frames
    // stop, speaking times out and the level decays to a single silence (127) report.
    for _ in 0..5 {
        a.send_audio_with_level(addr, &channel, 6, b"loud-opus")
            .await;
    }
    while b.recv().await.is_some() {}
    let evs = drain(&mut events, 600).await;
    assert_eq!(speaking_of(&evs, s_a.user_id), vec![true, false]);
    assert_eq!(energy_of(&evs, channel, s_a.user_id), vec![6, 127]);

    // Unlabelled frames keep the legacy behaviour: speaking by arrival, no level report.
    a.send_audio(addr, &channel, b"legacy").await;
    let evs = drain(&mut events, 150).await;
    assert_eq!(speaking_of(&evs, s_a.user_id), vec![true]);
    assert!(energy_of(&evs, channel, s_a.user_id).is_empty());

    // A steady level is reported once — and once more when a new member joins the channel so
    // late joiners get a baseline for everyone already talking. Frames are refreshed before the
    // level can go stale so nothing but the join causes a report.
    for _ in 0..4 {
        a.send_audio_with_level(addr, &channel, 6, b"steady").await;
    }
    let evs = drain(&mut events, BEFORE_DECAY_MS).await;
    assert_eq!(energy_of(&evs, channel, s_a.user_id), vec![6]);
    for _ in 0..4 {
        a.send_audio_with_level(addr, &channel, 6, b"steady").await;
    }
    let s_c = sfu
        .create_session(SessionId::new(), UserId::new(), app, "c".into())
        .unwrap();
    sfu.join_channel(
        &s_c.session_id,
        channel,
        ChannelConfig::default(),
        ChannelRole::Listener,
    )
    .unwrap();
    let evs = drain(&mut events, BEFORE_DECAY_MS).await;
    assert_eq!(
        energy_of(&evs, channel, s_a.user_id),
        vec![6],
        "unchanged level is re-reported for the late joiner"
    );
    while b.recv().await.is_some() {}
}

struct ToneTts;

#[async_trait::async_trait]
impl aurix_common::tts_stt::TtsProvider for ToneTts {
    async fn synthesize(
        &self,
        text: &str,
        _voice: &str,
    ) -> aurix_common::error::Result<aurix_common::tts_stt::PcmAudio> {
        // 24 kHz stereo, length scales with text so truncation can be exercised.
        let frames = 24_000 * text.len() / 10;
        let mut samples = Vec::with_capacity(frames * 2);
        for i in 0..frames {
            let v = ((i as f32 / 24_000.0 * 440.0 * std::f32::consts::TAU).sin() * 9000.0) as i16;
            samples.push(v);
            samples.push(v);
        }
        Ok(aurix_common::tts_stt::PcmAudio {
            sample_rate: 24_000,
            channels: 2,
            samples,
        })
    }
    fn provider_name(&self) -> &str {
        "tone"
    }
}

/// Synthesized speech is injected through the normal delivery path: it carries the speaker's
/// synthesized SSRC, is sealed per receiver, honours receiver preferences and the requester's
/// destination, and announcements reach everybody regardless of blocks.
#[tokio::test]
async fn tts_injection_follows_channel_routing() {
    use aurix_common::protocol::TtsState;
    use aurix_media::tts::*;

    let (sfu, addr) = start_sfu().await;
    let app = AppId::new();
    let channel = ChannelId::new();
    let cfg = ChannelConfig::default();
    let mut clients = Vec::new();
    for name in ["alice", "bob", "carol"] {
        let s = sfu
            .create_session(SessionId::new(), UserId::new(), app, name.into())
            .unwrap();
        sfu.join_channel(&s.session_id, channel, cfg.clone(), ChannelRole::Speaker)
            .unwrap();
        let mut c = Client::new(s).await;
        c.bind(addr).await;
        clients.push(c);
    }
    let (alice, bob, carol) = (clients.remove(0), clients.remove(0), clients.remove(0));
    // Carol has muted Alice locally: she must not hear Alice's synthesized voice either.
    carol
        .session
        .prefs
        .write()
        .set_muted(alice.session.user_id, None, true);

    let engine = std::sync::Arc::new(TtsEngine::new(
        std::sync::Arc::new(ToneTts),
        TtsEngineOptions {
            max_audio: Duration::from_millis(200),
            max_queued_per_session: 1,
            ..TtsEngineOptions::default()
        },
    ));
    engine.attach_router(sfu.router().unwrap().clone());
    let mut status = engine.subscribe();

    let id = engine
        .submit(TtsRequest {
            app_id: app,
            channel_id: channel,
            text: "hello".into(), // 500 ms of audio → truncated to 200 ms = 10 frames
            voice: "alloy".into(),
            source: TtsSource::Participant {
                session: alice.session.clone(),
                to_channel: true,
                to_self: true,
            },
            client_ref: Some("r1".into()),
            request_id: None,
        })
        .unwrap();
    // Per-session queue limit.
    let err = engine
        .submit(TtsRequest {
            app_id: app,
            channel_id: channel,
            text: "again".into(),
            voice: "alloy".into(),
            source: TtsSource::Participant {
                session: alice.session.clone(),
                to_channel: true,
                to_self: false,
            },
            client_ref: None,
            request_id: None,
        })
        .unwrap_err();
    assert_eq!(err.error_code(), "RATE_LIMIT_EXCEEDED");

    let mut states = Vec::new();
    while let Ok(Ok(ev)) = tokio::time::timeout(Duration::from_secs(2), status.recv()).await {
        assert_eq!(ev.request_id, id);
        assert_eq!(ev.client_ref.as_deref(), Some("r1"));
        states.push(ev.state);
        if matches!(ev.state, TtsState::Finished | TtsState::Failed) {
            assert_eq!(ev.duration_ms, Some(200));
            break;
        }
    }
    assert_eq!(
        states,
        vec![TtsState::Queued, TtsState::Playing, TtsState::Finished]
    );

    let synth_ssrc = participant_voice_ssrc(alice.session.ssrc);
    let mut bob_frames = Vec::new();
    while let Some(p) = bob.recv().await {
        assert_eq!(p.header.packet_type, PacketType::Audio);
        assert_eq!(p.header.ssrc, synth_ssrc);
        assert_eq!(p.header.channel_id_hash, channel_id_hash(&channel));
        bob_frames.push(p);
    }
    assert_eq!(bob_frames.len(), 10, "10 × 20 ms after truncation");
    let seqs: Vec<u32> = bob_frames.iter().map(|p| p.header.sequence).collect();
    assert_eq!(seqs, (0..10).collect::<Vec<_>>());
    assert_eq!(bob_frames[1].header.timestamp, 960);
    let mut dec = opus::Decoder::new(48_000, opus::Channels::Mono).unwrap();
    let mut pcm = vec![0i16; 5760];
    let mut rms = 0.0;
    for p in &bob_frames {
        let n = dec.decode(&p.payload, &mut pcm, false).unwrap();
        assert_eq!(n, 960);
        rms = (pcm[..n].iter().map(|v| f64::from(*v).powi(2)).sum::<f64>() / n as f64).sqrt();
    }
    assert!(rms > 5000.0, "tone survives resample+opus: rms {rms}");

    let mut alice_frames = 0;
    while let Some(p) = alice.recv().await {
        assert_eq!(p.header.ssrc, synth_ssrc);
        alice_frames += 1;
    }
    assert_eq!(alice_frames, 10, "destination=both echoes to the requester");
    assert!(carol.recv().await.is_none(), "local mute applies to TTS");
    assert_eq!(engine.pending(), 0);

    // Announcement: nobody's voice, everybody hears it (Carol's mute of Alice is irrelevant),
    // stable per-channel SSRC, continuing sequence across requests.
    for _ in 0..2 {
        let id = engine
            .submit(TtsRequest {
                app_id: app,
                channel_id: channel,
                text: "x".into(), // 100 ms → 5 frames
                voice: "alloy".into(),
                source: TtsSource::System,
                client_ref: None,
                request_id: None,
            })
            .unwrap();
        loop {
            let ev = status.recv().await.unwrap();
            if ev.request_id == id && ev.state == TtsState::Finished {
                assert_eq!(ev.session_id, None);
                break;
            }
        }
    }
    let sys = system_voice_ssrc(&channel);
    assert_ne!(sys & SYNTH_SSRC_FLAG, 0);
    for c in [&alice, &bob, &carol] {
        let mut seqs = Vec::new();
        while let Some(p) = c.recv().await {
            assert_eq!(p.header.ssrc, sys);
            seqs.push(p.header.sequence);
        }
        assert_eq!(seqs, (0..10).collect::<Vec<_>>(), "{}", c.session.user_id);
    }

    // Another tenant cannot announce into this channel.
    let id = engine
        .submit(TtsRequest {
            app_id: AppId::new(),
            channel_id: channel,
            text: "x".into(),
            voice: "alloy".into(),
            source: TtsSource::System,
            client_ref: None,
            request_id: None,
        })
        .unwrap();
    loop {
        let ev = status.recv().await.unwrap();
        if ev.request_id == id && ev.state != TtsState::Queued && ev.state != TtsState::Playing {
            assert_eq!(ev.state, TtsState::Failed);
            break;
        }
    }

    // Spoken translation: private to the requesting listener on the channel's translator SSRC
    // (never a participant's or the announcement SSRC), no `TtsStatus` for anyone, and its
    // sequence clock is per listener so two listeners' translations do not share one stream.
    // Carol's local mute of Alice is irrelevant: the voice is the server's, not Alice's.
    let translator = translation_voice_ssrc(&channel);
    assert_ne!(translator, sys);
    for listener in [&carol, &bob] {
        engine
            .submit(TtsRequest {
                app_id: app,
                channel_id: channel,
                text: "x".into(), // 5 frames
                voice: "nova".into(),
                source: TtsSource::Listener {
                    listener: listener.session.clone(),
                },
                client_ref: None,
                request_id: None,
            })
            .unwrap();
    }
    assert!(
        tokio::time::timeout(Duration::from_millis(600), status.recv())
            .await
            .is_err(),
        "listener-scoped speech emits no status events"
    );
    for _ in 0..20 {
        if engine.pending() == 0 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(engine.pending(), 0);
    for c in [&carol, &bob] {
        let mut seqs = Vec::new();
        while let Some(p) = c.recv().await {
            assert_eq!(p.header.ssrc, translator);
            assert_eq!(p.header.channel_id_hash, channel_id_hash(&channel));
            seqs.push(p.header.sequence);
        }
        assert_eq!(
            seqs,
            (0..5).collect::<Vec<_>>(),
            "{} hears only her own translation",
            c.session.user_id
        );
    }
    assert!(
        alice.recv().await.is_none(),
        "the speaker never hears listeners' translations"
    );
    // A listener who left the channel (or belongs to another tenant) gets nothing.
    sfu.leave_channel(&carol.session.session_id, &channel)
        .unwrap();
    engine
        .submit(TtsRequest {
            app_id: app,
            channel_id: channel,
            text: "x".into(),
            voice: "nova".into(),
            source: TtsSource::Listener {
                listener: carol.session.clone(),
            },
            client_ref: None,
            request_id: None,
        })
        .unwrap();
    for _ in 0..20 {
        if engine.pending() == 0 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(carol.recv().await.is_none());
    assert!(bob.recv().await.is_none());
    sfu.join_channel(
        &carol.session.session_id,
        channel,
        cfg.clone(),
        ChannelRole::Speaker,
    )
    .unwrap();

    // Cancellation mid-playout stops the stream; only the owner may cancel.
    let id = engine
        .submit(TtsRequest {
            app_id: app,
            channel_id: channel,
            text: "long text that plays for a while".into(),
            voice: "alloy".into(),
            source: TtsSource::Participant {
                session: bob.session.clone(),
                to_channel: true,
                to_self: false,
            },
            client_ref: None,
            request_id: None,
        })
        .unwrap();
    loop {
        let ev = status.recv().await.unwrap();
        if ev.request_id == id && ev.state == TtsState::Playing {
            break;
        }
    }
    assert!(!engine.cancel(&id, Some(alice.session.session_id)));
    assert!(engine.cancel(&id, Some(bob.session.session_id)));
    loop {
        let ev = status.recv().await.unwrap();
        if ev.request_id == id && ev.state != TtsState::Playing {
            assert_eq!(ev.state, TtsState::Cancelled);
            break;
        }
    }
    let mut got = 0;
    while alice.recv().await.is_some() {
        got += 1;
    }
    assert!(got < 10, "cancelled after {got} frames");
}

/// A session that negotiated PCMU sends and receives μ-law while the channel stays Opus:
/// its uplink is transcoded before recording/fan-out, Opus participants never see μ-law,
/// and only PCMU receivers get frames re-encoded (with the usual downlink metadata).
#[tokio::test]
async fn pcmu_sessions_are_transcoded_at_the_edge() {
    use aurix_common::g711::{self, PCMU_FRAME_SAMPLES, PCMU_SAMPLE_RATE};

    fn ulaw_tone(frame: usize, amp: f32) -> Vec<u8> {
        let pcm: Vec<i16> = (0..PCMU_FRAME_SAMPLES)
            .map(|i| {
                let t = (frame * PCMU_FRAME_SAMPLES + i) as f32 / PCMU_SAMPLE_RATE as f32;
                ((t * 440.0 * std::f32::consts::TAU).sin() * amp * 32767.0) as i16
            })
            .collect();
        let mut out = Vec::new();
        g711::encode(&pcm, &mut out);
        out
    }
    fn rms_i16(pcm: &[i16]) -> f32 {
        (pcm.iter()
            .map(|&s| (s as f32 / 32768.0).powi(2))
            .sum::<f32>()
            / pcm.len() as f32)
            .sqrt()
    }

    let (sfu, addr) = start_sfu().await;
    let app = AppId::new();
    let team = ChannelId::new();

    // Negotiation is native-only: a browser session stays on Opus.
    let web = sfu
        .create_session(SessionId::new(), UserId::new(), app, "web".into())
        .unwrap();
    web.set_transport(Transport::WebRtc);
    assert!(web.set_codec(AudioCodec::Pcmu).is_err());
    assert_eq!(web.codec(), AudioCodec::Opus);
    sfu.destroy_session(&web.session_id).unwrap();

    let s_a = sfu
        .create_session(SessionId::new(), UserId::new(), app, "a".into())
        .unwrap();
    let s_b = sfu
        .create_session(SessionId::new(), UserId::new(), app, "b".into())
        .unwrap();
    let s_c = sfu
        .create_session(SessionId::new(), UserId::new(), app, "c".into())
        .unwrap();
    for s in [&s_a, &s_b, &s_c] {
        sfu.join_channel(
            &s.session_id,
            team,
            ChannelConfig::default(),
            ChannelRole::Speaker,
        )
        .unwrap();
    }
    let mut a = Client::new(s_a.clone()).await;
    let mut b = Client::new(s_b.clone()).await;
    let mut c = Client::new(s_c.clone()).await;
    a.bind(addr).await;
    b.bind(addr).await;
    c.bind(addr).await;

    // Negotiation is per session.
    assert_eq!(s_a.codec(), AudioCodec::Opus);
    s_a.set_codec(AudioCodec::Pcmu).unwrap();
    s_c.set_codec(AudioCodec::Pcmu).unwrap();

    // Before negotiating, a μ-law frame is refused: B still sends Opus-flagged frames only.
    let seq = b.next_seq();
    let mut bad = AurixPacket::audio(
        seq,
        seq * 960,
        s_b.ssrc,
        channel_id_hash(&team),
        Bytes::from(ulaw_tone(0, 0.5)),
    );
    bad.header.flags |= PacketFlags::Pcmu as u16;
    b.sock.send_to(&bad.seal(&s_b.keys), addr).await.unwrap();
    assert!(
        a.recv().await.is_none(),
        "unnegotiated PCMU must be dropped"
    );

    // A (PCMU) speaks: B (Opus) hears Opus, C (PCMU) hears μ-law, both at 440 Hz.
    let mut opus_dec = opus::Decoder::new(48_000, opus::Channels::Mono).unwrap();
    let mut pcm48 = vec![0i16; 960];
    let mut b_level = 0.0;
    let mut c_level = 0.0;
    for i in 0..10 {
        let seq = a.next_seq();
        let mut pkt = AurixPacket::audio_with_level(
            seq,
            seq * 960,
            s_a.ssrc,
            channel_id_hash(&team),
            12,
            &ulaw_tone(i, 0.5),
        );
        pkt.header.flags |= PacketFlags::Pcmu as u16;
        a.sock.send_to(&pkt.seal(&s_a.keys), addr).await.unwrap();

        let got = b.recv().await.expect("B hears A");
        assert_eq!(got.header.ssrc, s_a.ssrc);
        assert!(!got.header.has_flag(PacketFlags::Pcmu), "Opus receiver");
        assert!(!got.header.has_flag(PacketFlags::Energy), "level stripped");
        let n = opus_dec.decode(&got.payload, &mut pcm48, false).unwrap();
        assert_eq!(n, 960, "20 ms Opus frame");
        b_level = rms_i16(&pcm48[..n]);

        let got = c.recv().await.expect("C hears A");
        assert_eq!(got.header.ssrc, s_a.ssrc);
        assert!(got.header.has_flag(PacketFlags::Pcmu), "PCMU receiver");
        assert_eq!(got.payload.len(), PCMU_FRAME_SAMPLES);
        let mut pcm8 = Vec::new();
        g711::decode(&got.payload, &mut pcm8);
        c_level = rms_i16(&pcm8);
    }
    assert!((b_level - 0.3535).abs() < 0.06, "B level {b_level}");
    assert!((c_level - 0.3535).abs() < 0.03, "C level {c_level}");
    assert!(a.recv().await.is_none(), "no echo to the sender");

    // B (Opus) speaks: A gets μ-law with its per-participant gain in the volume byte.
    s_a.prefs.write().set_gain(s_b.user_id, 0.5);
    let mut enc =
        opus::Encoder::new(48_000, opus::Channels::Mono, opus::Application::Voip).unwrap();
    let mut a_level = 0.0;
    for i in 0..10 {
        let pcm: Vec<i16> = (0..960)
            .map(|k| {
                let t = (i * 960 + k) as f32 / 48_000.0;
                ((t * 440.0 * std::f32::consts::TAU).sin() * 0.5 * 32767.0) as i16
            })
            .collect();
        let frame = enc.encode_vec(&pcm, 1275).unwrap();
        b.send_audio(addr, &team, &frame).await;

        let got = a.recv().await.expect("A hears B");
        assert!(got.header.has_flag(PacketFlags::Pcmu));
        assert!(got.header.has_flag(PacketFlags::VolumeAttenuated));
        assert_eq!(got.payload[0], encode_volume_byte(0.5));
        assert_eq!(got.payload.len(), 1 + PCMU_FRAME_SAMPLES);
        let mut pcm8 = Vec::new();
        g711::decode(&got.payload[1..], &mut pcm8);
        a_level = rms_i16(&pcm8);

        let got = c.recv().await.expect("C hears B");
        assert!(got.header.has_flag(PacketFlags::Pcmu));
        assert!(!got.header.has_flag(PacketFlags::VolumeAttenuated));
        assert_eq!(got.payload.len(), PCMU_FRAME_SAMPLES);
    }
    // Gain is applied by the receiver from the volume byte, the payload itself is at level.
    assert!((a_level - 0.3535).abs() < 0.06, "A level {a_level}");

    // Malformed μ-law (not a 10/20/40/60 ms frame) and E2EE μ-law never reach anyone.
    for (payload, e2ee) in [(vec![0xffu8; 100], false), (ulaw_tone(0, 0.5), true)] {
        let seq = a.next_seq();
        let mut pkt = AurixPacket::audio(
            seq,
            seq * 960,
            s_a.ssrc,
            channel_id_hash(&team),
            Bytes::from(payload),
        );
        pkt.header.flags |= PacketFlags::Pcmu as u16;
        if e2ee {
            pkt.header.flags |= PacketFlags::E2ee as u16;
        }
        a.sock.send_to(&pkt.seal(&s_a.keys), addr).await.unwrap();
        assert!(b.recv().await.is_none(), "e2ee={e2ee}");
        assert!(c.recv().await.is_none(), "e2ee={e2ee}");
    }

    // Back to Opus: A now receives plain Opus again and its μ-law uplink is refused.
    s_a.set_codec(AudioCodec::Opus).unwrap();
    let frame = enc.encode_vec(&vec![0i16; 960], 1275).unwrap();
    b.send_audio(addr, &team, &frame).await;
    let got = a.recv().await.expect("A hears B as Opus");
    assert!(!got.header.has_flag(PacketFlags::Pcmu));
    assert_eq!(&got.payload[1..], &frame[..]);
}

#[tokio::test]
async fn listeners_get_one_server_mixed_stream_per_channel() {
    use aurix_common::g711::{self, PCMU_FRAME_SAMPLES};
    use aurix_media::mix::channel_mix_ssrc;

    fn tone_frame(enc: &mut opus::Encoder, frame: usize, hz: f32, amp: f32) -> Vec<u8> {
        let pcm: Vec<i16> = (0..960)
            .map(|k| {
                let t = (frame * 960 + k) as f32 / 48_000.0;
                ((t * hz * std::f32::consts::TAU).sin() * amp * 32767.0) as i16
            })
            .collect();
        enc.encode_vec(&pcm, 1275).unwrap()
    }
    fn rms(pcm: &[i16]) -> f32 {
        (pcm.iter()
            .map(|&s| (s as f32 / 32768.0).powi(2))
            .sum::<f32>()
            / pcm.len().max(1) as f32)
            .sqrt()
    }
    async fn drain_udp(c: &Client) -> Vec<AurixPacket> {
        let mut out = Vec::new();
        while let Some(p) = c.recv().await {
            out.push(p);
        }
        out
    }

    let mut sfu = SfuNode::new(
        MediaNodeId::new(),
        Region::EuWest,
        SfuOptions {
            max_participants: 16,
            ..SfuOptions::default()
        },
    );
    sfu.start("127.0.0.1:0".parse().unwrap()).await.unwrap();
    let addr = sfu.local_addr().unwrap();
    assert!(sfu.downlink_mix_enabled());
    let app = AppId::new();
    let channel = ChannelId::new();
    let config = ChannelConfig {
        channel_type: ChannelType::Team,
        max_participants: 1000,
        audience: Some(AudienceConfig::default()),
        ..ChannelConfig::default()
    };
    let mk = |name: &str| {
        sfu.create_session(SessionId::new(), UserId::new(), app, name.into())
            .unwrap()
    };
    let s_a = mk("a");
    let s_b = mk("b");
    let s_c = mk("c");
    let s_l = mk("l");
    let s_t = mk("t");
    let s_p = mk("p");
    for (s, role) in [
        (&s_a, ChannelRole::Speaker),
        (&s_b, ChannelRole::Speaker),
        (&s_c, ChannelRole::Speaker),
        (&s_l, ChannelRole::Listener),
        (&s_t, ChannelRole::Listener),
        (&s_p, ChannelRole::Listener),
    ] {
        sfu.join_channel(&s.session_id, channel, config.clone(), role)
            .unwrap();
        s.set_e2ee_capable(true);
    }
    let mut a = Client::new(s_a.clone()).await;
    let mut b = Client::new(s_b.clone()).await;
    let mut c = Client::new(s_c.clone()).await;
    let mut l = Client::new(s_l.clone()).await;
    let mut t = TunnelClient::open(&sfu, s_t.clone());
    let mut p = Client::new(s_p.clone()).await;
    for cl in [&mut a, &mut b, &mut c, &mut l, &mut p] {
        cl.bind(addr).await;
    }
    t.bind(&sfu).await;
    s_p.set_codec(AudioCodec::Pcmu).unwrap();
    // C is a speaker who asked for a mixed downlink; the listeners are mixed by the
    // channel's `mix_for_listeners`.
    sfu.set_downlink_mode(&s_c.session_id, DownlinkMode::Mixed)
        .unwrap();
    assert_eq!(s_c.downlink_mode(), DownlinkMode::Mixed);

    // A listener may not transmit: the frame is refused before it reaches anyone.
    l.send_audio(addr, &channel, b"listener").await;
    assert!(a.recv().await.is_none());
    assert!(b.recv().await.is_none());

    // A (440 Hz) and B (660 Hz) talk together, paced like real clients.
    let mut enc_a =
        opus::Encoder::new(48_000, opus::Channels::Mono, opus::Application::Voip).unwrap();
    let mut enc_b =
        opus::Encoder::new(48_000, opus::Channels::Mono, opus::Application::Voip).unwrap();
    for i in 0..25 {
        a.send_audio(addr, &channel, &tone_frame(&mut enc_a, i, 440.0, 0.3))
            .await;
        b.send_audio(addr, &channel, &tone_frame(&mut enc_b, i, 660.0, 0.3))
            .await;
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    let mix_ssrc = channel_mix_ssrc(&channel);
    assert_ne!(mix_ssrc, s_a.ssrc);
    assert_ne!(mix_ssrc, s_b.ssrc);

    // The UDP listener: one stereo Opus stream from the channel's mix SSRC, contiguous
    // sequence, both voices audible, never the speakers' own SSRCs.
    let got = drain_udp(&l).await;
    assert!(got.len() >= 15, "listener got {} mixed frames", got.len());
    let mut dec = opus::Decoder::new(48_000, opus::Channels::Stereo).unwrap();
    let mut pcm = vec![0i16; 960 * 2];
    let mut level = 0.0f32;
    let mut prev_seq = None;
    for pkt in &got {
        assert_eq!(pkt.header.ssrc, mix_ssrc);
        assert!(pkt.header.has_flag(PacketFlags::Mixed));
        assert!(!pkt.header.has_flag(PacketFlags::E2ee));
        assert!(!pkt.header.has_flag(PacketFlags::Pcmu));
        assert_eq!(pkt.header.channel_id_hash, channel_id_hash(&channel));
        if let Some(prev) = prev_seq {
            assert_eq!(pkt.header.sequence, prev + 1, "contiguous mixed sequence");
        }
        prev_seq = Some(pkt.header.sequence);
        let n = dec.decode(&pkt.payload, &mut pcm, false).unwrap();
        assert_eq!(n, 960, "20 ms stereo frame");
        level = level.max(rms(&pcm[..n * 2]));
    }
    // Two 0.3-amplitude tones summed: ~0.3 RMS (each contributes 0.3/√2 in power).
    assert!((0.2..0.45).contains(&level), "mixed level {level}");

    // The tunneled listener gets the same mix, sealed with its own keys, over the tunnel.
    let mut tunneled = Vec::new();
    while let Some(pkt) = t.recv_open().await {
        tunneled.push(pkt);
    }
    assert!(tunneled.len() >= 15, "tunnel got {} frames", tunneled.len());
    assert!(tunneled
        .iter()
        .all(|p| p.header.ssrc == mix_ssrc && p.header.has_flag(PacketFlags::Mixed)));

    // The PCMU listener gets the mix as 20 ms μ-law frames at 8 kHz.
    let got_p = drain_udp(&p).await;
    assert!(
        got_p.len() >= 15,
        "pcmu listener got {} frames",
        got_p.len()
    );
    let mut p_level = 0.0f32;
    for pkt in &got_p {
        assert_eq!(pkt.header.ssrc, mix_ssrc);
        assert!(pkt.header.has_flag(PacketFlags::Mixed));
        assert!(pkt.header.has_flag(PacketFlags::Pcmu));
        assert_eq!(pkt.payload.len(), PCMU_FRAME_SAMPLES);
        let mut pcm8 = Vec::new();
        g711::decode(&pkt.payload, &mut pcm8);
        p_level = p_level.max(rms(&pcm8));
    }
    assert!((0.2..0.45).contains(&p_level), "pcmu mixed level {p_level}");

    // C hears A and B mixed on a private mixer (a speaker must never hear itself).
    let got_c = drain_udp(&c).await;
    assert!(
        got_c.len() >= 15,
        "speaker C got {} mixed frames",
        got_c.len()
    );
    assert!(got_c
        .iter()
        .all(|p| p.header.ssrc == mix_ssrc && p.header.has_flag(PacketFlags::Mixed)));
    // Speakers on per-speaker downlinks still get the two source streams.
    let got_a = drain_udp(&a).await;
    assert!(got_a
        .iter()
        .all(|p| p.header.ssrc == s_b.ssrc && !p.header.has_flag(PacketFlags::Mixed)));
    assert!(got_a.len() >= 20);

    // C alone talks: the listeners hear the mix, C hears nothing back.
    for i in 0..10 {
        c.send_audio(addr, &channel, &tone_frame(&mut enc_a, i, 440.0, 0.3))
            .await;
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(!drain_udp(&l).await.is_empty());
    assert!(drain_udp(&c).await.is_empty(), "no self-audio in C's mix");

    // End-to-end encrypted frames bypass the mixer: they arrive as A's own stream.
    let seq = a.next_seq();
    let mut e2ee = AurixPacket::audio(
        seq,
        seq * 960,
        s_a.ssrc,
        channel_id_hash(&channel),
        Bytes::from_static(b"sealed-by-the-app"),
    );
    e2ee.header.flags |= PacketFlags::E2ee as u16;
    a.sock.send_to(&e2ee.seal(&s_a.keys), addr).await.unwrap();
    let got = drain_udp(&l).await;
    let e2ee_frames: Vec<_> = got
        .iter()
        .filter(|p| p.header.has_flag(PacketFlags::E2ee))
        .collect();
    assert_eq!(e2ee_frames.len(), 1);
    assert_eq!(e2ee_frames[0].header.ssrc, s_a.ssrc);
    assert!(!e2ee_frames[0].header.has_flag(PacketFlags::Mixed));
    assert_eq!(&e2ee_frames[0].payload[..], b"sealed-by-the-app");
    assert!(got
        .iter()
        .all(|p| p.header.has_flag(PacketFlags::E2ee) || p.header.ssrc == mix_ssrc));

    // Back to per-speaker streams for C: it hears A's own SSRC again; the listeners' mix is
    // unaffected by C's choice.
    sfu.set_downlink_mode(&s_c.session_id, DownlinkMode::Streams)
        .unwrap();
    drain_udp(&c).await;
    for i in 0..5 {
        a.send_audio(addr, &channel, &tone_frame(&mut enc_a, i, 440.0, 0.3))
            .await;
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let got_c = drain_udp(&c).await;
    assert_eq!(got_c.len(), 5);
    assert!(got_c
        .iter()
        .all(|p| p.header.ssrc == s_a.ssrc && !p.header.has_flag(PacketFlags::Mixed)));
    let got_l = drain_udp(&l).await;
    assert!(!got_l.is_empty());
    assert!(got_l.iter().all(|p| p.header.ssrc == mix_ssrc));

    // Leaving the channel stops the mixed stream for that receiver only.
    sfu.leave_channel(&s_l.session_id, &channel).unwrap();
    for i in 0..5 {
        a.send_audio(addr, &channel, &tone_frame(&mut enc_a, i, 440.0, 0.3))
            .await;
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(drain_udp(&l).await.is_empty());
    assert!(!drain_udp(&p).await.is_empty());
}

/// A session adopted after a cross-node failover keeps its SSRC, so the receivers that were
/// already listening to it keep their per-SSRC anti-replay windows: its downlink sequence
/// must continue above what the old node handed out, not restart at zero.
#[tokio::test]
async fn adopted_session_downlink_sequence_continues_above_the_mirrored_one() {
    let (sfu, addr) = start_sfu().await;
    let app = AppId::new();
    let channel = ChannelId::new();
    let alice_id = SessionId::new();
    let alice_user = UserId::new();
    let alice = sfu
        .create_session(alice_id, alice_user, app, "alice".into())
        .unwrap();
    let bob = sfu
        .create_session(SessionId::new(), UserId::new(), app, "bob".into())
        .unwrap();
    for s in [&alice, &bob] {
        sfu.join_channel(
            &s.session_id,
            channel,
            ChannelConfig::default(),
            ChannelRole::Speaker,
        )
        .unwrap();
    }
    let mut a = Client::new(alice.clone()).await;
    let mut b = Client::new(bob.clone()).await;
    a.bind(addr).await;
    b.bind(addr).await;

    // Bob's receive side behaves like a real client: one replay window per sender SSRC.
    let mut window = ReplayWindow::default();
    for i in 0..3 {
        a.send_audio(addr, &channel, b"before").await;
        let pkt = b.recv().await.expect("downlink before the move");
        assert_eq!(pkt.header.ssrc, alice.ssrc);
        assert!(window.check_and_update(pkt.header.sequence), "packet {i}");
    }
    let mirrored = alice.audio_sequence();
    assert!(mirrored >= 3);
    let ssrc = alice.ssrc;

    // "Node 1 died": the session goes away here and is adopted with the same id/SSRC, its
    // sequence resumed from the mirror plus the migration gap.
    sfu.destroy_session(&alice_id).unwrap();
    let gap = 1u32 << 16;
    let adopted = sfu
        .adopt_session(
            alice_id,
            alice_user,
            app,
            "alice".into(),
            ssrc,
            mirrored + gap,
        )
        .unwrap();
    assert_eq!(adopted.ssrc, ssrc);
    assert_ne!(
        adopted.media_key, alice.media_key,
        "media key must be fresh"
    );
    sfu.join_channel(
        &alice_id,
        channel,
        ChannelConfig::default(),
        ChannelRole::Speaker,
    )
    .unwrap();
    let mut a2 = Client::new(adopted.clone()).await;
    a2.bind(addr).await;
    while b.recv().await.is_some() {}

    a2.send_audio(addr, &channel, b"after").await;
    let pkt = b.recv().await.expect("downlink after the move");
    assert_eq!(pkt.header.ssrc, ssrc);
    assert!(
        pkt.header.sequence >= mirrored + gap,
        "sequence {} must continue above the mirrored {mirrored} + gap",
        pkt.header.sequence
    );
    assert!(
        window.check_and_update(pkt.header.sequence),
        "a receiver's anti-replay window must accept the migrated stream"
    );

    // Restarting at zero (what a plain re-creation would do) is exactly what the window rejects.
    assert!(!window.check_and_update(0));
}

/// One `[::]` socket serves IPv4 and IPv6 players at once: the IPv4 peer is recorded with its
/// real IPv4 address (not `::ffff:…`), audio crosses families both ways, and only advertised
/// addresses the socket can actually serve survive.
#[tokio::test]
async fn dual_stack_sfu_routes_between_ipv4_and_ipv6_clients() {
    let v6 = match UdpSocket::bind("[::1]:0").await {
        Ok(s) => s,
        Err(e) => {
            eprintln!("skipping: no IPv6 loopback ({e})");
            return;
        }
    };
    let mut sfu = SfuNode::new(
        MediaNodeId::new(),
        Region::EuWest,
        SfuOptions {
            max_participants: 3,
            advertised_addrs: vec![
                "203.0.113.7:10000".parse().unwrap(),
                "[2001:db8::7]:10000".parse().unwrap(),
            ],
            ..SfuOptions::default()
        },
    );
    if let Err(e) = sfu.start("[::]:0".parse().unwrap()).await {
        eprintln!("skipping: dual-stack bind unavailable ({e})");
        return;
    }
    assert_eq!(
        sfu.family(),
        Some(aurix_common::net::BoundFamily::DualStack)
    );
    assert_eq!(sfu.advertised_addrs().len(), 2, "both families advertised");
    let port = sfu.local_addr().unwrap().port();
    let via_v4: SocketAddr = format!("127.0.0.1:{port}").parse().unwrap();
    let via_v6: SocketAddr = format!("[::1]:{port}").parse().unwrap();

    let app = AppId::new();
    let channel = ChannelId::new();
    let s_a = sfu
        .create_session(SessionId::new(), UserId::new(), app, "a".into())
        .unwrap();
    let s_b = sfu
        .create_session(SessionId::new(), UserId::new(), app, "b".into())
        .unwrap();
    for s in [&s_a, &s_b] {
        sfu.join_channel(
            &s.session_id,
            channel,
            ChannelConfig::default(),
            ChannelRole::Speaker,
        )
        .unwrap();
    }
    let mut a = Client::new(s_a.clone()).await;
    let mut b = Client {
        sock: v6,
        session: s_b.clone(),
        seq: 1,
    };
    a.bind(via_v4).await;
    b.bind(via_v6).await;

    match s_a.endpoint() {
        Some(aurix_media::session::MediaEndpoint::Udp(ep)) => {
            assert!(ep.is_ipv4(), "IPv4 peer must be canonical, got {ep}");
            assert_eq!(ep, a.sock.local_addr().unwrap());
        }
        other => panic!("unexpected endpoint {other:?}"),
    }
    match s_b.endpoint() {
        Some(aurix_media::session::MediaEndpoint::Udp(ep)) => {
            assert!(ep.is_ipv6());
            assert_eq!(ep, b.sock.local_addr().unwrap());
        }
        other => panic!("unexpected endpoint {other:?}"),
    }

    a.send_audio(via_v4, &channel, b"from-v4").await;
    let got = b.recv().await.expect("IPv6 client hears the IPv4 client");
    assert_eq!(got.header.ssrc, s_a.ssrc);
    b.send_audio(via_v6, &channel, b"from-v6").await;
    let got = a.recv().await.expect("IPv4 client hears the IPv6 client");
    assert_eq!(got.header.ssrc, s_b.ssrc);
}

/// An IPv4-only bind drops IPv6 advertisements instead of handing clients a dead candidate.
#[tokio::test]
async fn ipv4_only_sfu_advertises_ipv4_only() {
    let (sfu, _addr) = start_sfu_with(SfuOptions {
        advertised_addrs: vec![
            "203.0.113.7:10000".parse().unwrap(),
            "[2001:db8::7]:10000".parse().unwrap(),
        ],
        ..SfuOptions::default()
    })
    .await;
    assert_eq!(sfu.family(), Some(aurix_common::net::BoundFamily::V4));
    assert_eq!(
        sfu.advertised_addrs(),
        &["203.0.113.7:10000".parse::<SocketAddr>().unwrap()]
    );
}
