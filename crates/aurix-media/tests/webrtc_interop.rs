//! A str0m-based "browser" connects to the SFU over WebRTC and exchanges audio with a
//! native AURX client in the same channel.

use aurix_common::protocol::*;
use aurix_common::types::*;
use aurix_media::mixer::{encode_pcm_frame, FRAME_SAMPLES, OUTPUT_CHANNELS, SAMPLE_RATE};
use aurix_media::{SfuNode, SfuOptions};
use bytes::Bytes;
use std::net::SocketAddr;
use std::time::{Duration, Instant};
use str0m::change::SdpAnswer;
use str0m::format::Codec;
use str0m::media::{Direction, MediaKind, MediaTime, Mid};
use str0m::net::{Protocol, Receive};
use str0m::{Candidate, Event, Input, Output, Rtc};
use tokio::net::UdpSocket;

/// Drives a client-side str0m `Rtc` over a real UDP socket.
struct BrowserClient {
    rtc: Rtc,
    sock: UdpSocket,
    local: SocketAddr,
    mid: Mid,
    /// Extra recvonly audio m-lines (per-participant tracks), in offer order.
    extra_mids: Vec<Mid>,
    connected: bool,
    /// Downlink Opus frames by the m-line they arrived on.
    received: Vec<(Mid, Vec<u8>)>,
    rtp_time: u64,
    pending: Option<(String, str0m::change::SdpPendingOffer)>,
}

impl BrowserClient {
    async fn new() -> Self {
        Self::with_participant_tracks(0).await
    }

    /// Like a browser that pre-negotiates `extra` per-participant downlink tracks.
    async fn with_participant_tracks(extra: usize) -> Self {
        let sock = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let local = sock.local_addr().unwrap();
        let mut rtc = Rtc::builder()
            .clear_codecs()
            .enable_opus(true)
            .build(Instant::now());
        rtc.add_local_candidate(Candidate::host(local, "udp").unwrap());
        let mut api = rtc.sdp_api();
        let mid = api.add_media(MediaKind::Audio, Direction::SendRecv, None, None, None);
        let extra_mids = (0..extra)
            .map(|_| api.add_media(MediaKind::Audio, Direction::RecvOnly, None, None, None))
            .collect();
        let (offer, pending) = api.apply().unwrap();
        Self {
            rtc,
            sock,
            local,
            mid,
            extra_mids,
            connected: false,
            received: Vec::new(),
            rtp_time: 0,
            pending: Some((offer.to_sdp_string(), pending)),
        }
    }
}

impl BrowserClient {
    fn offer(&mut self) -> String {
        self.pending.as_ref().unwrap().0.clone()
    }

    fn accept_answer(&mut self, answer: &str) {
        let (_, pending) = self.pending.take().unwrap();
        let answer = SdpAnswer::from_sdp_string(answer).unwrap();
        self.rtc.sdp_api().accept_answer(pending, answer).unwrap();
    }

    /// Pump the state machine for `dur`, sending/receiving on the socket.
    async fn run_for(&mut self, dur: Duration) {
        let end = Instant::now() + dur;
        let mut buf = vec![0u8; 2000];
        while Instant::now() < end {
            // Drain outputs.
            let timeout = loop {
                match self.rtc.poll_output().unwrap() {
                    Output::Transmit(t) => {
                        self.sock.send_to(&t.contents, t.destination).await.unwrap();
                    }
                    Output::Event(Event::Connected) => self.connected = true,
                    Output::Event(Event::MediaData(d)) => {
                        assert_eq!(d.params.spec().codec, Codec::Opus);
                        self.received.push((d.mid, d.data.to_vec()));
                    }
                    Output::Event(_) => {}
                    Output::Timeout(t) => break t,
                }
            };
            let wait = timeout
                .saturating_duration_since(Instant::now())
                .min(Duration::from_millis(20));
            match tokio::time::timeout(wait, self.sock.recv_from(&mut buf)).await {
                Ok(Ok((n, src))) => {
                    let recv = Receive {
                        proto: Protocol::Udp,
                        source: src,
                        destination: self.local,
                        contents: (&buf[..n]).try_into().unwrap(),
                    };
                    self.rtc
                        .handle_input(Input::Receive(Instant::now(), recv))
                        .unwrap();
                }
                _ => {
                    self.rtc
                        .handle_input(Input::Timeout(Instant::now()))
                        .unwrap();
                }
            }
        }
    }

    fn received_on(&self, mid: Mid) -> Vec<&Vec<u8>> {
        self.received
            .iter()
            .filter(|(m, _)| *m == mid)
            .map(|(_, d)| d)
            .collect()
    }

    fn send_opus(&mut self, data: Vec<u8>) {
        let writer = self.rtc.writer(self.mid).unwrap();
        let pt = writer
            .payload_params()
            .find(|p| p.spec().codec == Codec::Opus)
            .unwrap()
            .pt();
        let ts = MediaTime::new(self.rtp_time, str0m::media::Frequency::FORTY_EIGHT_KHZ);
        writer.write(pt, Instant::now(), ts, data).unwrap();
        self.rtp_time += FRAME_SAMPLES as u64;
    }
}

fn tone() -> Vec<i16> {
    (0..FRAME_SAMPLES)
        .map(|i| {
            ((i as f32 * 440.0 * std::f32::consts::TAU / SAMPLE_RATE as f32).sin()
                * 0.4
                * i16::MAX as f32) as i16
        })
        .collect()
}

#[tokio::test]
async fn browser_and_aurx_client_hear_each_other() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .try_init();
    let mut sfu = SfuNode::new(MediaNodeId::new(), Region::EuWest, SfuOptions::default());
    sfu.start("127.0.0.1:0".parse().unwrap()).await.unwrap();
    let sfu_addr = sfu.local_addr().unwrap();
    let app = AppId::new();
    let channel = ChannelId::new();

    // Native client
    let native = sfu
        .create_session(SessionId::new(), UserId::new(), app, "native".into())
        .unwrap();
    sfu.join_channel(
        &native.session_id,
        channel,
        ChannelConfig::default(),
        ChannelRole::Speaker,
    )
    .unwrap();
    let native_sock = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let bind = AurixPacket::session_bind(
        &native.session_id,
        native.ssrc,
        chrono::Utc::now().timestamp_millis(),
        1,
    );
    native_sock
        .send_to(&bind.encode_authenticated(&native.keys), sfu_addr)
        .await
        .unwrap();
    let mut buf = [0u8; 256];
    tokio::time::timeout(Duration::from_secs(2), native_sock.recv_from(&mut buf))
        .await
        .unwrap()
        .unwrap();

    // Browser client
    let browser_session = sfu
        .create_session(SessionId::new(), UserId::new(), app, "browser".into())
        .unwrap();
    sfu.join_channel(
        &browser_session.session_id,
        channel,
        ChannelConfig::default(),
        ChannelRole::Speaker,
    )
    .unwrap();
    let mut browser = BrowserClient::new().await;
    let offer = browser.offer();
    let answer = sfu
        .attach_webrtc(&browser_session.session_id, &offer)
        .unwrap();
    assert!(answer.contains("a=ice-lite"), "SFU answers as ICE-lite");
    assert!(answer.contains("opus/48000/2"), "answer negotiates Opus");
    let fmtp = answer
        .lines()
        .find(|l| l.starts_with("a=fmtp:") && l.contains("sprop-stereo=1"))
        .expect("answer announces a stereo downlink");
    assert!(
        fmtp.contains("useinbandfec=1") && !fmtp.contains(";stereo=1"),
        "uplink stays mono: {fmtp}"
    );
    browser.accept_answer(&answer);

    // ICE + DTLS handshake
    for _ in 0..40 {
        browser.run_for(Duration::from_millis(100)).await;
        if browser.connected && browser_session.get_remote_addr().is_some() {
            break;
        }
    }
    assert!(browser.connected, "browser did not connect");
    assert_eq!(
        browser_session.get_remote_addr(),
        Some(browser.local),
        "SFU learned the ICE-verified peer address"
    );

    // Browser -> native: send a few Opus frames; native receives them as AURX audio with browser SSRC.
    let frame = encode_pcm_frame(&tone()).unwrap();
    for _ in 0..5 {
        browser.send_opus(frame.clone());
        browser.run_for(Duration::from_millis(20)).await;
    }
    let mut got_from_browser = 0;
    let mut nbuf = [0u8; 1500];
    while let Ok(Ok((n, _))) =
        tokio::time::timeout(Duration::from_millis(300), native_sock.recv_from(&mut nbuf)).await
    {
        let mut pkt = AurixPacket::decode(&nbuf[..n]).unwrap();
        assert!(
            pkt.open(&native.keys),
            "downlink must be sealed for the native session"
        );
        if pkt.header.packet_type == PacketType::Audio {
            assert_eq!(pkt.header.ssrc, browser_session.ssrc);
            assert_eq!(pkt.header.channel_id_hash, channel_id_hash(&channel));
            assert_eq!(&pkt.payload[..], &frame[..]);
            got_from_browser += 1;
        }
    }
    assert!(
        got_from_browser >= 3,
        "native client got {got_from_browser} frames from browser"
    );

    // Native -> browser: send Opus frames; the browser receives a mixed Opus downlink stream.
    for seq in 1..=10u32 {
        let pkt = AurixPacket::audio(
            seq,
            seq * 960,
            native.ssrc,
            channel_id_hash(&channel),
            Bytes::from(frame.clone()),
        );
        native_sock
            .send_to(&pkt.seal(&native.keys), sfu_addr)
            .await
            .unwrap();
        browser.run_for(Duration::from_millis(20)).await;
    }
    browser.run_for(Duration::from_millis(300)).await;
    assert!(
        browser.received.len() >= 3,
        "browser received {} mixed frames",
        browser.received.len()
    );
    let mut dec = opus::Decoder::new(SAMPLE_RATE, opus::Channels::Stereo).unwrap();
    let mut pcm = vec![0i16; FRAME_SAMPLES * OUTPUT_CHANNELS];
    let n = dec.decode(&browser.received[1].1, &mut pcm, false).unwrap();
    assert_eq!(n, FRAME_SAMPLES);
    assert!(
        pcm.iter().any(|s| s.abs() > 200),
        "downlink carries real (non-silent) audio"
    );

    // Disconnect cleanup
    sfu.destroy_session(&browser_session.session_id).unwrap();
    assert!(!sfu
        .webrtc_manager()
        .unwrap()
        .has_session(&browser_session.session_id));
    assert!(sfu.get_session(&browser_session.session_id).is_none());
}

struct NativeSpeaker {
    session: std::sync::Arc<aurix_media::MediaSession>,
    sock: UdpSocket,
    seq: u32,
}

impl NativeSpeaker {
    async fn join(sfu: &SfuNode, sfu_addr: SocketAddr, app: AppId, channel: ChannelId) -> Self {
        let session = sfu
            .create_session(SessionId::new(), UserId::new(), app, "native".into())
            .unwrap();
        sfu.join_channel(
            &session.session_id,
            channel,
            ChannelConfig::default(),
            ChannelRole::Speaker,
        )
        .unwrap();
        let sock = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let bind = AurixPacket::session_bind(
            &session.session_id,
            session.ssrc,
            chrono::Utc::now().timestamp_millis(),
            1,
        );
        sock.send_to(&bind.encode_authenticated(&session.keys), sfu_addr)
            .await
            .unwrap();
        let mut buf = [0u8; 256];
        tokio::time::timeout(Duration::from_secs(2), sock.recv_from(&mut buf))
            .await
            .unwrap()
            .unwrap();
        Self {
            session,
            sock,
            seq: 0,
        }
    }

    async fn speak(&mut self, sfu_addr: SocketAddr, channel: &ChannelId, frame: &[u8]) {
        self.seq += 1;
        let pkt = AurixPacket::audio(
            self.seq,
            self.seq * 960,
            self.session.ssrc,
            channel_id_hash(channel),
            Bytes::copy_from_slice(frame),
        );
        self.sock
            .send_to(&pkt.seal(&self.session.keys), sfu_addr)
            .await
            .unwrap();
    }
}

/// Latest `ParticipantStreams` snapshot for `session` (waits for at least one, then drains
/// what followed), as `(mid, user)` pairs.
async fn next_layout(
    events: &mut tokio::sync::broadcast::Receiver<aurix_media::MediaEvent>,
    session: &SessionId,
) -> Vec<(String, Option<UserId>)> {
    let mut latest = None;
    loop {
        let wait = if latest.is_some() {
            Duration::from_millis(200)
        } else {
            Duration::from_secs(3)
        };
        let ev = match tokio::time::timeout(wait, events.recv()).await {
            Ok(ev) => ev.unwrap(),
            Err(_) => return latest.expect("ParticipantStreams snapshot"),
        };
        if let aurix_media::MediaEvent::ParticipantStreams {
            session_id,
            streams,
        } = ev
        {
            if session_id == *session {
                latest = Some(streams.into_iter().map(|s| (s.mid, s.user_id)).collect());
            }
        }
    }
}

/// A browser that offers extra recvonly audio m-lines gets speakers forwarded on their own
/// tracks (frames byte-identical to the uplink), speakers beyond the tracks in the mix, the
/// layout announced on every change, and pinning that reserves a track for a participant.
#[tokio::test]
async fn browser_gets_per_participant_tracks() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .try_init();
    let mut sfu = SfuNode::new(
        MediaNodeId::new(),
        Region::EuWest,
        SfuOptions {
            webrtc_participant_streams: 2,
            ..SfuOptions::default()
        },
    );
    sfu.start("127.0.0.1:0".parse().unwrap()).await.unwrap();
    let sfu_addr = sfu.local_addr().unwrap();
    let mut events = sfu.subscribe_events();
    let app = AppId::new();
    let channel = ChannelId::new();

    let mut alice = NativeSpeaker::join(&sfu, sfu_addr, app, channel).await;
    let mut bob = NativeSpeaker::join(&sfu, sfu_addr, app, channel).await;
    let mut carol = NativeSpeaker::join(&sfu, sfu_addr, app, channel).await;

    let browser_session = sfu
        .create_session(SessionId::new(), UserId::new(), app, "browser".into())
        .unwrap();
    sfu.join_channel(
        &browser_session.session_id,
        channel,
        ChannelConfig::default(),
        ChannelRole::Speaker,
    )
    .unwrap();
    // Three tracks offered, the node serves two: the third m-line is answered but stays silent.
    let mut browser = BrowserClient::with_participant_tracks(3).await;
    let offer = browser.offer();
    let answer = sfu
        .attach_webrtc(&browser_session.session_id, &offer)
        .unwrap();
    assert_eq!(
        answer.matches("m=audio").count(),
        4,
        "answer keeps every audio m-line: {answer}"
    );
    assert_eq!(answer.matches("a=sendonly").count(), 3, "{answer}");
    browser.accept_answer(&answer);
    for _ in 0..40 {
        browser.run_for(Duration::from_millis(100)).await;
        if browser.connected && browser_session.get_remote_addr().is_some() {
            break;
        }
    }
    assert!(browser.connected, "browser did not connect");

    let layout = next_layout(&mut events, &browser_session.session_id).await;
    let served: Vec<String> = browser.extra_mids[..2]
        .iter()
        .map(|m| m.to_string())
        .collect();
    assert_eq!(
        layout,
        served.iter().map(|m| (m.clone(), None)).collect::<Vec<_>>(),
        "initial layout: two idle tracks"
    );

    // Alice and Bob speak: each gets a track; Carol arrives while both are busy -> mix.
    let frame_a = encode_pcm_frame(&tone()).unwrap();
    let quiet: Vec<i16> = tone().iter().map(|s| s / 4).collect();
    let frame_b = encode_pcm_frame(&quiet).unwrap();
    assert_ne!(frame_a, frame_b);
    for _ in 0..10 {
        alice.speak(sfu_addr, &channel, &frame_a).await;
        bob.speak(sfu_addr, &channel, &frame_b).await;
        carol.speak(sfu_addr, &channel, &frame_a).await;
        browser.run_for(Duration::from_millis(20)).await;
    }
    browser.run_for(Duration::from_millis(300)).await;
    let layout = next_layout(&mut events, &browser_session.session_id).await;
    let user_on = |layout: &[(String, Option<UserId>)], user: UserId| -> Option<Mid> {
        layout
            .iter()
            .find(|(_, u)| *u == Some(user))
            .map(|(m, _)| Mid::from(m.as_str()))
    };
    // Alice's frame reaches the SFU first, so she binds first; Bob second.
    let mid_a = user_on(&layout, alice.session.user_id).expect("alice has a track");
    let mid_b = user_on(&layout, bob.session.user_id).expect("bob has a track");
    assert_ne!(mid_a, mid_b);
    assert!(
        user_on(&layout, carol.session.user_id).is_none(),
        "carol stays in the mix: {layout:?}"
    );
    let on_a = browser.received_on(mid_a);
    let on_b = browser.received_on(mid_b);
    assert!(
        on_a.len() >= 5 && on_b.len() >= 5,
        "{} / {}",
        on_a.len(),
        on_b.len()
    );
    assert!(
        on_a.iter().all(|f| **f == frame_a) && on_b.iter().all(|f| **f == frame_b),
        "per-participant tracks carry the speaker's own frames untouched"
    );
    let mixed = browser.received_on(browser.mid);
    assert!(
        mixed.len() >= 3,
        "carol is heard in the mix ({})",
        mixed.len()
    );
    let mut dec = opus::Decoder::new(SAMPLE_RATE, opus::Channels::Stereo).unwrap();
    let mut pcm = vec![0i16; FRAME_SAMPLES * OUTPUT_CHANNELS];
    dec.decode(mixed[mixed.len() / 2], &mut pcm, false).unwrap();
    assert!(pcm.iter().any(|s| s.abs() > 200), "mix is not silent");
    assert!(
        browser
            .received
            .iter()
            .all(|(m, _)| *m == browser.mid || *m == mid_a || *m == mid_b),
        "the unserved third track stays silent"
    );

    // Pin Carol: she takes over a track from an active unpinned speaker with her next frame.
    sfu.set_participant_streams(&browser_session.session_id, vec![carol.session.user_id])
        .unwrap();
    browser.received.clear();
    for _ in 0..10 {
        alice.speak(sfu_addr, &channel, &frame_a).await;
        bob.speak(sfu_addr, &channel, &frame_b).await;
        carol.speak(sfu_addr, &channel, &frame_a).await;
        browser.run_for(Duration::from_millis(20)).await;
    }
    browser.run_for(Duration::from_millis(300)).await;
    let layout = next_layout(&mut events, &browser_session.session_id).await;
    let mid_c = user_on(&layout, carol.session.user_id).expect("pinned carol has a track");
    assert!(browser.received_on(mid_c).len() >= 5);
    let displaced = [alice.session.user_id, bob.session.user_id]
        .into_iter()
        .filter(|u| user_on(&layout, *u).is_none())
        .count();
    assert_eq!(
        displaced, 1,
        "exactly one unpinned speaker moved to the mix: {layout:?}"
    );
    assert!(
        browser.received_on(browser.mid).len() >= 3,
        "the displaced speaker is heard in the mix"
    );

    // Pinning too many, or a native session, is rejected.
    assert!(sfu
        .set_participant_streams(
            &browser_session.session_id,
            vec![UserId::new(), UserId::new(), UserId::new()],
        )
        .is_err());
    assert!(sfu
        .set_participant_streams(&alice.session.session_id, vec![])
        .is_err());

    sfu.destroy_session(&browser_session.session_id).unwrap();
    assert!(!sfu
        .webrtc_manager()
        .unwrap()
        .has_session(&browser_session.session_id));
}
