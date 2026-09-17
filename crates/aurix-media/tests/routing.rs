//! End-to-end AURX routing tests over real UDP sockets on loopback.

use aurix_common::protocol::*;
use aurix_common::types::*;
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
            .send_to(&pkt.encode_authenticated(&self.session.media_key), sfu)
            .await
            .unwrap();
        let mut buf = [0u8; 256];
        let (n, _) = tokio::time::timeout(Duration::from_secs(2), self.sock.recv_from(&mut buf))
            .await
            .expect("bind ack")
            .unwrap();
        let ack = AurixPacket::decode(&buf[..n]).unwrap();
        assert_eq!(ack.header.packet_type, PacketType::SessionBindAck);
        assert!(
            ack.verify_auth(&self.session.media_key),
            "ack must be authenticated"
        );
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
            .send_to(&pkt.encode_authenticated(&self.session.media_key), sfu)
            .await
            .unwrap();
    }

    async fn recv(&self) -> Option<AurixPacket> {
        let mut buf = [0u8; 1500];
        match tokio::time::timeout(Duration::from_millis(300), self.sock.recv_from(&mut buf)).await
        {
            Ok(Ok((n, _))) => Some(AurixPacket::decode(&buf[..n]).unwrap()),
            _ => None,
        }
    }
}

async fn start_sfu() -> (SfuNode, SocketAddr) {
    let mut sfu = SfuNode::new(
        MediaNodeId::new(),
        Region::EuWest,
        SfuOptions {
            max_participants: 3,
            ..SfuOptions::default()
        },
    );
    sfu.start("127.0.0.1:0").await.unwrap();
    let addr = sfu.local_addr().unwrap();
    (sfu, addr)
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
    let got = b.recv().await.expect("b receives a's audio");
    assert_eq!(got.header.ssrc, s_a.ssrc);
    assert_eq!(&got.payload[..], b"hello-opus");
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
        .send_to(&spoof.encode_authenticated(b"guess"), addr)
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
    b.sock
        .send_to(&pkt.encode_authenticated(&s_b.media_key), addr)
        .await
        .unwrap();
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

    // Replay of an already-seen sequence is rejected.
    let replay = AurixPacket::audio(
        2,
        0,
        s_a.ssrc,
        channel_id_hash(&channel),
        Bytes::from_static(b"replay"),
    );
    a.sock
        .send_to(&replay.encode_authenticated(&s_a.media_key), addr)
        .await
        .unwrap();
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
        AurixPacket::session_bind(&s.session_id, s.ssrc, now, 1).encode_authenticated(&s.media_key);
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
    .encode_authenticated(&s.media_key);
    attacker.send_to(&stale, addr).await.unwrap();
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
