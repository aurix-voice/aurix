//! End-to-end AURX routing tests over real UDP sockets on loopback.

use aurix_common::crypto::MediaKeys;
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

    // Quiet frames (-60 dBov): forwarded as audio, level byte stripped, but not "speaking".
    for _ in 0..5 {
        a.send_audio_with_level(addr, &channel, 60, b"quiet-opus")
            .await;
    }
    let got = b.recv().await.expect("b hears quiet audio");
    assert!(!got.header.has_flag(PacketFlags::Energy));
    assert_eq!(&got.payload[..], b"quiet-opus");
    let evs = drain(&mut events, 150).await;
    assert!(
        speaking_of(&evs, s_a.user_id).is_empty(),
        "quiet labelled frames must not flip speaking: {evs:?}"
    );
    let quiet = energy_of(&evs, channel, s_a.user_id);
    assert!(
        quiet.first() == Some(&60) && quiet.iter().skip(1).all(|l| *l == 127),
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
    // late joiners get a baseline for everyone already talking.
    for _ in 0..4 {
        a.send_audio_with_level(addr, &channel, 6, b"steady").await;
    }
    let evs = drain(&mut events, 120).await;
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
    let evs = drain(&mut events, 120).await;
    assert_eq!(
        energy_of(&evs, channel, s_a.user_id),
        vec![6],
        "unchanged level is re-reported for the late joiner"
    );
    while b.recv().await.is_some() {}
}
