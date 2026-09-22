// SPDX-FileCopyrightText: 2025 Aurix contributors
// SPDX-License-Identifier: Apache-2.0

//! A browser-shaped WebTransport client (the `wtransport` client, which enforces the W3C
//! `serverCertificateHashes` rules: ≤ 14-day ECDSA P-256 certificate, hash match) against a
//! real `SfuNode`: session at `/aurix`, `SessionBind` ownership, sealed AURX both ways, the
//! mix with a native UDP peer, refusals and the limits.

use aurix_client::media::{IncomingAudio, MediaTransport, SequenceCounter};
use aurix_common::crypto::MediaKeys;
use aurix_common::protocol::{
    channel_id_hash, AurixPacket, PacketType, WebTransportInfo, MAX_PACKET_SIZE,
};
use aurix_common::types::*;
use aurix_media::webtransport::WebTransportOptions;
use aurix_media::{MediaEvent, MediaSession, SfuNode, SfuOptions};
use bytes::Bytes;
use std::net::SocketAddr;
use std::sync::atomic::AtomicU32;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use wtransport::endpoint::endpoint_side::Client;
use wtransport::tls::Sha256Digest;
use wtransport::{ClientConfig, Connection, Endpoint};

fn wt_options() -> WebTransportOptions {
    WebTransportOptions {
        enabled: true,
        port: 0,
        queue_packets: 32,
        bind_timeout: Duration::from_millis(800),
        idle_timeout: Duration::from_secs(5),
        cert_validity: Duration::from_secs(2 * 86_400),
        ..WebTransportOptions::default()
    }
}

async fn start_sfu(options: WebTransportOptions) -> (SfuNode, SocketAddr, WebTransportInfo) {
    let mut sfu = SfuNode::new(
        MediaNodeId::new(),
        Region::EuWest,
        SfuOptions {
            max_participants: 4,
            webtransport: options,
            ..SfuOptions::default()
        },
    );
    sfu.start("127.0.0.1:0".parse().unwrap()).await.unwrap();
    let udp = sfu.local_addr().unwrap();
    let info = sfu
        .webtransport_info()
        .expect("node advertises WebTransport");
    (sfu, udp, info)
}

fn digests(info: &WebTransportInfo) -> Vec<Sha256Digest> {
    info.cert_sha256
        .iter()
        .map(|h| {
            let bytes: [u8; 32] = hex::decode(h).unwrap().try_into().unwrap();
            Sha256Digest::from(bytes)
        })
        .collect()
}

fn browser(info: &WebTransportInfo) -> Endpoint<Client> {
    let config = ClientConfig::builder()
        .with_bind_default()
        .with_server_certificate_hashes(digests(info))
        .build();
    Endpoint::client(config).unwrap()
}

async fn connect(info: &WebTransportInfo) -> Connection {
    connect_to(info, &info.urls[0])
        .await
        .expect("WebTransport session")
}

async fn connect_to(
    info: &WebTransportInfo,
    url: &str,
) -> Result<Connection, wtransport::error::ConnectingError> {
    tokio::time::timeout(Duration::from_secs(3), browser(info).connect(url))
        .await
        .expect("handshake finished")
}

/// The browser side of one media session: AURX keys, SSRC and the sequence counter.
struct Tab {
    session: Arc<MediaSession>,
    conn: Connection,
    keys: MediaKeys,
    seq: u32,
}

impl Tab {
    fn new(session: Arc<MediaSession>, conn: Connection) -> Self {
        let keys = MediaKeys::derive(&session.media_key);
        Self {
            session,
            conn,
            keys,
            seq: 1,
        }
    }

    fn bind_packet(&self) -> Vec<u8> {
        AurixPacket::session_bind(
            &self.session.session_id,
            self.session.ssrc,
            chrono::Utc::now().timestamp_millis(),
            rand::random(),
        )
        .encode_authenticated(&self.keys)
        .to_vec()
    }

    /// `SessionBind` out, sealed `SessionBindAck` back on the same session.
    async fn bind(&self) -> bool {
        self.conn.send_datagram(self.bind_packet()).unwrap();
        let Some(mut ack) = self.recv().await else {
            return false;
        };
        ack.header.packet_type == PacketType::SessionBindAck
            && ack.header.ssrc == self.session.ssrc
            && ack.open(&self.keys)
    }

    fn send_audio(&mut self, channel_hash: u32, payload: &[u8]) {
        let seq = self.seq;
        self.seq += 1;
        let packet = AurixPacket::audio(
            seq,
            seq * 960,
            self.session.ssrc,
            channel_hash,
            Bytes::copy_from_slice(payload),
        );
        self.conn.send_datagram(packet.seal(&self.keys)).unwrap();
    }

    fn send_heartbeat(&mut self) {
        let mut packet = AurixPacket::heartbeat(self.session.ssrc, self.seq * 960);
        packet.header.sequence = self.seq;
        self.seq += 1;
        self.conn.send_datagram(packet.seal(&self.keys)).unwrap();
    }

    /// Next datagram decoded as an AURX packet (not yet opened).
    async fn recv(&self) -> Option<AurixPacket> {
        let datagram =
            tokio::time::timeout(Duration::from_millis(700), self.conn.receive_datagram())
                .await
                .ok()?
                .ok()?;
        AurixPacket::decode(&datagram.payload()).ok()
    }

    /// Next opened downlink audio packet.
    async fn recv_audio(&self) -> Option<AurixPacket> {
        loop {
            let mut packet = self.recv().await?;
            if packet.header.packet_type == PacketType::Audio && packet.open(&self.keys) {
                return Some(packet);
            }
        }
    }
}

fn new_session(sfu: &SfuNode, app: AppId, channel: ChannelId, name: &str) -> Arc<MediaSession> {
    let session = sfu
        .create_session(SessionId::new(), UserId::new(), app, name.into())
        .unwrap();
    sfu.join_channel(
        &session.session_id,
        channel,
        ChannelConfig::default(),
        ChannelRole::Speaker,
    )
    .unwrap();
    session
}

async fn tab(
    sfu: &SfuNode,
    info: &WebTransportInfo,
    app: AppId,
    channel: ChannelId,
    name: &str,
) -> Tab {
    let session = new_session(sfu, app, channel, name);
    let tab = Tab::new(session, connect(info).await);
    assert!(tab.bind().await, "{name} binds over WebTransport");
    tab
}

fn native_peer(
    sfu: &SfuNode,
    udp: SocketAddr,
    app: AppId,
    channel: ChannelId,
) -> (
    Arc<MediaSession>,
    Arc<MediaTransport>,
    mpsc::UnboundedReceiver<IncomingAudio>,
) {
    let session = new_session(sfu, app, channel, "native");
    let sequence: SequenceCounter = Arc::new(AtomicU32::new(1));
    let media = Arc::new(
        MediaTransport::bind_udp(
            udp,
            session.session_id,
            session.ssrc,
            &session.media_key,
            sequence,
            3,
            Duration::from_secs(2),
        )
        .expect("UDP bind"),
    );
    let (tx, rx) = mpsc::unbounded_channel();
    media.start(Arc::new(move |frame| {
        let _ = tx.send(frame);
    }));
    (session, media, rx)
}

async fn wait_for(mut cond: impl FnMut() -> bool) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    while !cond() {
        assert!(
            tokio::time::Instant::now() < deadline,
            "condition not met within 3s"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

async fn bound_event(events: &mut tokio::sync::broadcast::Receiver<MediaEvent>) -> MediaEvent {
    loop {
        let ev = tokio::time::timeout(Duration::from_secs(2), events.recv())
            .await
            .expect("media event")
            .expect("event stream open");
        if matches!(ev, MediaEvent::SessionBound { .. }) {
            return ev;
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn browser_binds_at_aurix_and_carries_sealed_media_both_ways() {
    let (sfu, udp, info) = start_sfu(wt_options()).await;
    let wt = sfu.webtransport().unwrap().local_addr();
    assert_ne!(wt.port(), udp.port(), "own UDP port, normally 443");
    assert_eq!(info.urls, vec![format!("https://{wt}/aurix")]);
    assert_eq!(info.cert_sha256.len(), 2, "current and next pin");
    let app = AppId::new();
    let channel = ChannelId::new();
    let hash = channel_id_hash(&channel);
    let mut events = sfu.subscribe_events();

    let mut a = tab(&sfu, &info, app, channel, "a").await;
    assert_eq!(a.session.transport_kind(), MediaTransportKind::WebTransport);
    assert!(a.session.is_webtransport());
    assert!(matches!(
        bound_event(&mut events).await,
        MediaEvent::SessionBound {
            transport: MediaTransportKind::WebTransport,
            ..
        }
    ));
    let b = tab(&sfu, &info, app, channel, "b").await;
    assert_eq!(sfu.webtransport().unwrap().connection_count(), 2);

    for i in 0..5u8 {
        a.send_audio(hash, &[0xF8, i, 1, 2, 3]);
    }
    let mut got = Vec::new();
    while let Some(p) = b.recv_audio().await {
        got.push(p);
        if got.len() == 5 {
            break;
        }
    }
    assert_eq!(got.len(), 5, "B hears every datagram A sent");
    assert!(got.iter().all(|p| p.header.ssrc == a.session.ssrc));
    assert_eq!(got[4].payload[1], 4);
    assert!(a.recv_audio().await.is_none(), "A does not hear itself");

    // A full-size packet is one datagram, intact.
    let big = vec![0xABu8; MAX_PACKET_SIZE - 30 - 16 - 8];
    a.send_audio(hash, &big);
    let p = b.recv_audio().await.expect("large frame");
    assert_eq!(p.payload.len(), big.len());

    // Heartbeats are answered on the same session.
    a.send_heartbeat();
    let mut ack = a.recv().await.expect("heartbeat ack");
    assert_eq!(ack.header.packet_type, PacketType::HeartbeatAck);
    assert!(ack.open(&a.keys));

    // Closing the browser session frees the node's path for the session.
    let link = a.session.webtransport().expect("A's link");
    a.conn.close(wtransport::VarInt::from_u32(0), b"tab closed");
    wait_for(|| a.session.webtransport().is_none()).await;
    assert!(!link.is_open());
    wait_for(|| sfu.webtransport().unwrap().connection_count() == 1).await;
    assert!(b.session.is_webtransport());
    sfu.shutdown();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn browser_and_native_udp_peer_share_a_channel() {
    let (sfu, udp, info) = start_sfu(wt_options()).await;
    let app = AppId::new();
    let channel = ChannelId::new();
    let hash = channel_id_hash(&channel);

    let mut browser = tab(&sfu, &info, app, channel, "browser").await;
    let (native_session, native, mut native_rx) = native_peer(&sfu, udp, app, channel);
    assert_eq!(native_session.transport_kind(), MediaTransportKind::Udp);

    browser.send_audio(hash, &[0xF8, 7, 7]);
    let f = tokio::time::timeout(Duration::from_millis(700), native_rx.recv())
        .await
        .expect("native hears the browser")
        .unwrap();
    assert_eq!(f.sender_ssrc, browser.session.ssrc);
    assert_eq!(&f.payload[..], &[0xF8, 7, 7]);

    native.send_audio(hash, 960, Some(40), &[0xF8, 9, 9, 9]);
    let p = browser.recv_audio().await.expect("browser hears native");
    assert_eq!(p.header.ssrc, native_session.ssrc);
    assert_eq!(&p.payload[p.payload.len() - 4..], &[0xF8, 9, 9, 9]);
    native.stop();
    sfu.shutdown();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn wrong_path_wrong_pin_wrong_key_and_unbound_sessions_are_refused() {
    let (sfu, _udp, info) = start_sfu(wt_options()).await;
    let app = AppId::new();
    let channel = ChannelId::new();
    let server = sfu.webtransport().unwrap();

    // Any path but /aurix gets a 404 at the WebTransport handshake.
    let other = info.urls[0].replace("/aurix", "/other");
    assert!(connect_to(&info, &other).await.is_err());

    // A hash the node does not present fails the TLS handshake.
    let bogus = WebTransportInfo {
        urls: info.urls.clone(),
        cert_sha256: vec!["ab".repeat(32)],
    };
    assert!(connect_to(&bogus, &bogus.urls[0]).await.is_err());
    wait_for(|| server.connection_count() == 0).await;

    // A SessionBind signed with the wrong key gets no ack and never binds the session.
    let session = new_session(&sfu, app, channel, "wrong-key");
    let conn = connect(&info).await;
    let wrong = MediaKeys::derive(&[0u8; 32]);
    let bind = AurixPacket::session_bind(
        &session.session_id,
        session.ssrc,
        chrono::Utc::now().timestamp_millis(),
        1,
    )
    .encode_authenticated(&wrong);
    conn.send_datagram(bind).unwrap();
    let unbound = Tab::new(session.clone(), conn);
    assert!(unbound.recv().await.is_none());
    assert!(session.webtransport().is_none());

    // Audio before a bind is dropped, and the session is closed at the bind timeout.
    let mut early = Tab::new(
        new_session(&sfu, app, channel, "early"),
        connect(&info).await,
    );
    wait_for(|| server.connection_count() == 2).await;
    early.send_audio(channel_id_hash(&channel), &[1, 2, 3]);
    tokio::time::timeout(Duration::from_secs(3), early.conn.closed())
        .await
        .expect("closed at the bind timeout");
    wait_for(|| server.connection_count() == 0).await;
    assert!(early.session.webtransport().is_none());

    // A second bind for another session on an owned connection is refused (sticky owner).
    let owner = tab(&sfu, &info, app, channel, "owner").await;
    let intruder = new_session(&sfu, app, channel, "intruder");
    let bind = AurixPacket::session_bind(
        &intruder.session_id,
        intruder.ssrc,
        chrono::Utc::now().timestamp_millis(),
        2,
    )
    .encode_authenticated(&MediaKeys::derive(&intruder.media_key));
    owner.conn.send_datagram(bind).unwrap();
    assert!(owner.recv().await.is_none());
    assert!(intruder.webtransport().is_none());
    assert!(owner.session.is_webtransport());
    sfu.shutdown();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_new_session_replaces_the_link_and_the_stale_one_carries_nothing() {
    let (sfu, _udp, info) = start_sfu(wt_options()).await;
    let app = AppId::new();
    let channel = ChannelId::new();
    let hash = channel_id_hash(&channel);
    let mut speaker = tab(&sfu, &info, app, channel, "speaker").await;

    let listener = tab(&sfu, &info, app, channel, "listener").await;
    let first = listener.session.webtransport().unwrap();
    // The same tab reconnects (network change): the new session takes over the downlink.
    let again = Tab::new(listener.session.clone(), connect(&info).await);
    assert!(again.bind().await);
    let second = listener.session.webtransport().unwrap();
    assert_ne!(first.id(), second.id());
    wait_for(|| !first.is_open()).await;

    speaker.send_audio(hash, &[0xF8, 1]);
    assert!(again.recv_audio().await.is_some(), "new link hears");
    assert!(
        listener.recv_audio().await.is_none(),
        "stale link is closed"
    );
    wait_for(|| sfu.webtransport().unwrap().connection_count() == 2).await;
    sfu.shutdown();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn connection_cap_refuses_the_extra_session() {
    let (sfu, _udp, info) = start_sfu(WebTransportOptions {
        max_connections: 1,
        ..wt_options()
    })
    .await;
    let app = AppId::new();
    let channel = ChannelId::new();
    let _first = tab(&sfu, &info, app, channel, "first").await;
    assert!(connect_to(&info, &info.urls[0]).await.is_err());
    assert_eq!(sfu.webtransport().unwrap().connection_count(), 1);
    sfu.shutdown();
}
