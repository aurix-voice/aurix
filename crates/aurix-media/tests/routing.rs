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
    sfu.start("127.0.0.1:0").await.unwrap();
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
    sfu.start("127.0.0.1:0").await.unwrap();
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
    sfu.start("127.0.0.1:0").await.unwrap();
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
