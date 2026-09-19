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
    connected: bool,
    received: Vec<Vec<u8>>,
    rtp_time: u64,
    pending: Option<(String, str0m::change::SdpPendingOffer)>,
}

impl BrowserClient {
    async fn new() -> Self {
        let sock = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let local = sock.local_addr().unwrap();
        let mut rtc = Rtc::builder()
            .clear_codecs()
            .enable_opus(true)
            .build(Instant::now());
        rtc.add_local_candidate(Candidate::host(local, "udp").unwrap());
        let mut api = rtc.sdp_api();
        let mid = api.add_media(MediaKind::Audio, Direction::SendRecv, None, None, None);
        let (offer, pending) = api.apply().unwrap();
        Self {
            rtc,
            sock,
            local,
            mid,
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
                        self.received.push(d.data.to_vec());
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
    sfu.start("127.0.0.1:0").await.unwrap();
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
    let n = dec.decode(&browser.received[1], &mut pcm, false).unwrap();
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
