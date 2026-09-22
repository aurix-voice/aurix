//! The native client's TLS tunnel link against a real `SfuNode` on loopback: pinned TLS 1.3
//! handshake on the dedicated TCP port, authenticated bind, framed media both ways, link
//! replacement, refusals, and the mix of links inside one channel.

use aurix_client::media::{IncomingAudio, MediaPath, MediaTransport, SequenceCounter};
use aurix_client::ClientError;
use aurix_common::protocol::{channel_id_hash, TlsTunnelInfo};
use aurix_common::types::*;
use aurix_media::tls::TlsTunnelOptions;
use aurix_media::{MediaEvent, MediaSession, SfuNode, SfuOptions};
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;

const ATTEMPTS: u32 = 3;
const TIMEOUT: Duration = Duration::from_secs(2);

fn tls_options() -> TlsTunnelOptions {
    TlsTunnelOptions {
        enabled: true,
        port: 0,
        queue_packets: 32,
        bind_timeout: Duration::from_secs(2),
        idle_timeout: Duration::from_secs(5),
        ..TlsTunnelOptions::default()
    }
}

async fn start_sfu(options: TlsTunnelOptions) -> (SfuNode, SocketAddr, SocketAddr, TlsTunnelInfo) {
    let mut sfu = SfuNode::new(
        MediaNodeId::new(),
        Region::EuWest,
        SfuOptions {
            max_participants: 4,
            tls_tunnel: options,
            ..SfuOptions::default()
        },
    );
    sfu.start("127.0.0.1:0".parse().unwrap()).await.unwrap();
    let udp = sfu.local_addr().unwrap();
    let tls = sfu
        .tls_tunnel()
        .expect("node runs the TLS tunnel")
        .local_addr();
    let info = sfu
        .tls_tunnel_info()
        .expect("node advertises the TLS tunnel");
    (sfu, udp, tls, info)
}

struct Peer {
    session: Arc<MediaSession>,
    media: Arc<MediaTransport>,
    rx: mpsc::UnboundedReceiver<IncomingAudio>,
}

async fn connect(
    server: SocketAddr,
    info: &TlsTunnelInfo,
    session: &Arc<MediaSession>,
    sequence: SequenceCounter,
) -> Result<Arc<MediaTransport>, ClientError> {
    MediaTransport::bind_tls(
        server,
        info,
        session.session_id,
        session.ssrc,
        &session.media_key,
        sequence,
        ATTEMPTS,
        TIMEOUT,
    )
    .await
    .map(Arc::new)
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

fn started(
    media: Arc<MediaTransport>,
) -> (Arc<MediaTransport>, mpsc::UnboundedReceiver<IncomingAudio>) {
    let (tx, rx) = mpsc::unbounded_channel();
    media.start(Arc::new(move |frame| {
        let _ = tx.send(frame);
    }));
    (media, rx)
}

async fn peer(
    sfu: &SfuNode,
    server: SocketAddr,
    info: &TlsTunnelInfo,
    app: AppId,
    channel: ChannelId,
    name: &str,
) -> Peer {
    let session = new_session(sfu, app, channel, name);
    let media = connect(server, info, &session, Arc::new(AtomicU32::new(1)))
        .await
        .expect("TLS bind");
    let (media, rx) = started(media);
    Peer { session, media, rx }
}

async fn next_frame(rx: &mut mpsc::UnboundedReceiver<IncomingAudio>) -> Option<IncomingAudio> {
    tokio::time::timeout(Duration::from_millis(500), rx.recv())
        .await
        .ok()
        .flatten()
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
            .unwrap();
        if matches!(ev, MediaEvent::SessionBound { .. }) {
            return ev;
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tls_link_binds_and_carries_authenticated_media_both_ways() {
    let (sfu, udp, tls, info) = start_sfu(tls_options()).await;
    assert_ne!(tls.port(), udp.port(), "the tunnel has its own TCP port");
    assert_eq!(info.addrs, vec![tls.to_string()]);
    assert_eq!(info.cert_sha256.len(), 64);
    let app = AppId::new();
    let channel = ChannelId::new();
    let mut events = sfu.subscribe_events();

    let mut a = peer(&sfu, tls, &info, app, channel, "a").await;
    assert_eq!(a.media.path(), MediaPath::Tls);
    assert!(a.media.is_bound());
    assert!(!a.media.is_closed());
    assert_eq!(a.media.server(), Some(tls));
    assert!(a.media.local_addr().unwrap().is_ipv4());
    assert_eq!(a.session.transport_kind(), MediaTransportKind::Tls);
    assert!(a.session.is_tls());
    assert!(matches!(
        bound_event(&mut events).await,
        MediaEvent::SessionBound {
            transport: MediaTransportKind::Tls,
            ..
        }
    ));
    let mut b = peer(&sfu, tls, &info, app, channel, "b").await;
    assert_eq!(sfu.tls_tunnel().unwrap().connection_count(), 2);

    let hash = channel_id_hash(&channel);
    for i in 0..5u32 {
        a.media
            .send_audio(hash, i * 960, Some(40), &[0xF8, i as u8, 1, 2, 3]);
    }
    let mut got = Vec::new();
    while let Some(f) = next_frame(&mut b.rx).await {
        got.push(f);
        if got.len() == 5 {
            break;
        }
    }
    assert_eq!(got.len(), 5, "B hears every frame A sent over TLS");
    assert!(got.iter().all(|f| f.sender_ssrc == a.session.ssrc));
    assert!(got
        .iter()
        .all(|f| f.codec == AudioCodec::Opus && !f.e2ee && !f.mixed));
    assert_eq!(got[4].payload[1], 4);
    assert!(
        next_frame(&mut a.rx).await.is_none(),
        "A does not hear itself"
    );

    // A full-size packet crosses the framing intact.
    let big = vec![0xABu8; 1200];
    a.media.send_audio(hash, 5 * 960, None, &big);
    let f = next_frame(&mut b.rx).await.expect("large frame");
    assert_eq!(f.payload.len(), big.len());

    // An E2EE channel: the sealed payload crosses the tunnel opaque to the node.
    let sealed = ChannelId::new();
    let sealed_hash = channel_id_hash(&sealed);
    let cfg = ChannelConfig {
        e2ee: true,
        ..ChannelConfig::default()
    };
    for p in [&a, &b] {
        p.session.set_e2ee_capable(true);
        sfu.join_channel(
            &p.session.session_id,
            sealed,
            cfg.clone(),
            ChannelRole::Speaker,
        )
        .unwrap();
    }
    b.media
        .send_audio_e2ee(sealed_hash, 1920, None, b"sealed-by-sender");
    let f = next_frame(&mut a.rx).await.expect("e2ee frame");
    assert!(f.e2ee);
    assert_eq!(f.channel_hash, sealed_hash);
    assert_eq!(&f.payload[..], b"sealed-by-sender");

    // Heartbeats are answered on the same connection and measure RTT.
    a.media.send_heartbeat();
    tokio::time::sleep(Duration::from_millis(200)).await;
    let stats = a.media.stats();
    assert_eq!(stats.rtt_samples, 1);
    assert_eq!(stats.heartbeats_lost, 0);
    assert_eq!(stats.bad_auth, 0);
    assert_eq!(stats.uplink_dropped, 0);
    assert!(stats.packets_received >= 2);

    // Closing the client connection frees the node's path for the session.
    let link = a.session.tls().expect("A's link");
    a.media.stop();
    assert!(a.media.is_closed());
    wait_for(|| a.session.tls().is_none()).await;
    assert!(!link.is_open());
    wait_for(|| sfu.tls_tunnel().unwrap().connection_count() == 1).await;
    assert!(b.media.is_bound() && !b.media.is_closed());
    sfu.shutdown();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_new_bind_replaces_the_link_and_the_stale_one_carries_nothing() {
    let (sfu, _udp, tls, info) = start_sfu(tls_options()).await;
    let app = AppId::new();
    let channel = ChannelId::new();
    let hash = channel_id_hash(&channel);
    let a = peer(&sfu, tls, &info, app, channel, "a").await;
    let mut b = peer(&sfu, tls, &info, app, channel, "b").await;
    let sequence = a.media.sequence_counter();

    for i in 0..3u32 {
        a.media.send_audio(hash, i * 960, None, &[1, i as u8]);
    }
    for _ in 0..3 {
        next_frame(&mut b.rx).await.expect("frame before rebind");
    }
    let before = sequence.load(Ordering::Relaxed);
    let old_link = a.session.tls().unwrap();

    // The same session binds again on a fresh connection (reconnect after a network change):
    // the newest authenticated bind owns the session, the node closes the old connection.
    let resumed = connect(tls, &info, &a.session, Arc::clone(&sequence))
        .await
        .expect("second TLS bind");
    let (resumed, _rx) = started(resumed);
    assert!(resumed.is_bound());
    assert_eq!(a.session.transport_kind(), MediaTransportKind::Tls);
    let new_link = a.session.tls().unwrap();
    assert!(!Arc::ptr_eq(&old_link, &new_link));
    wait_for(|| a.media.is_closed()).await;
    assert!(!old_link.is_open());
    wait_for(|| sfu.tls_tunnel().unwrap().connection_count() == 2).await;

    a.media.send_audio(hash, 9000, None, &[9, 9]);
    assert!(
        next_frame(&mut b.rx).await.is_none(),
        "stale link carries nothing"
    );
    assert!(sequence.load(Ordering::Relaxed) > before);
    let after = sequence.load(Ordering::Relaxed);
    resumed.send_audio(hash, 3 * 960, None, &[2, 0]);
    let f = next_frame(&mut b.rx).await.expect("frame after rebind");
    assert_eq!(f.payload[0], 2);
    assert_eq!(f.sender_ssrc, a.session.ssrc);
    assert!(sequence.load(Ordering::Relaxed) > after);

    // A gap in the sender's numbering (packets lost on a previous link) blocks nothing.
    sequence.fetch_add(7, Ordering::Relaxed);
    resumed.send_audio(hash, 4 * 960, None, &[3, 0]);
    assert_eq!(
        next_frame(&mut b.rx)
            .await
            .expect("frame after a gap")
            .payload[0],
        3
    );
    sfu.shutdown();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn wrong_pin_wrong_key_and_foreign_session_are_refused() {
    let (sfu, _udp, tls, info) = start_sfu(tls_options()).await;
    let app = AppId::new();
    let channel = ChannelId::new();
    let session = new_session(&sfu, app, channel, "a");

    // A pin for some other certificate: the handshake fails before any AURX is sent.
    let wrong_pin = TlsTunnelInfo {
        cert_sha256: "00".repeat(32),
        ..info.clone()
    };
    let err = connect(tls, &wrong_pin, &session, Arc::new(AtomicU32::new(1)))
        .await
        .err()
        .expect("pinned handshake must fail");
    assert!(matches!(err, ClientError::Transport(_)), "{err:?}");
    assert!(session.tls().is_none());

    // A valid handshake with the wrong media key: the node never acknowledges the bind and
    // closes the connection at its bind timeout.
    let err = MediaTransport::bind_tls(
        tls,
        &info,
        session.session_id,
        session.ssrc,
        &[0u8; 32],
        Arc::new(AtomicU32::new(1)),
        1,
        Duration::from_millis(500),
    )
    .await
    .err()
    .expect("unauthenticated bind gets no ack");
    assert!(matches!(err, ClientError::Transport(_)), "{err:?}");
    assert!(session.tls().is_none());
    assert!(!session.is_tls());

    // A bind for a session the node does not know is ignored the same way.
    let err = MediaTransport::bind_tls(
        tls,
        &info,
        SessionId::new(),
        session.ssrc,
        &session.media_key,
        Arc::new(AtomicU32::new(1)),
        1,
        Duration::from_millis(500),
    )
    .await
    .err()
    .expect("unknown session gets no ack");
    assert!(matches!(err, ClientError::Transport(_)), "{err:?}");
    wait_for(|| sfu.tls_tunnel().unwrap().connection_count() == 0).await;
    sfu.shutdown();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tls_and_udp_peers_share_a_channel() {
    let (sfu, udp, tls, info) = start_sfu(tls_options()).await;
    let app = AppId::new();
    let channel = ChannelId::new();
    let hash = channel_id_hash(&channel);
    let mut a = peer(&sfu, tls, &info, app, channel, "a").await;

    let b_session = new_session(&sfu, app, channel, "b");
    let b_media = {
        let session = Arc::clone(&b_session);
        tokio::task::spawn_blocking(move || {
            MediaTransport::bind_udp(
                udp,
                session.session_id,
                session.ssrc,
                &session.media_key,
                Arc::new(AtomicU32::new(1)),
                ATTEMPTS,
                TIMEOUT,
            )
        })
        .await
        .unwrap()
        .expect("UDP bind")
    };
    let (b_media, mut b_rx) = started(Arc::new(b_media));
    assert_eq!(b_media.path(), MediaPath::Udp);
    assert_eq!(b_session.transport_kind(), MediaTransportKind::Udp);

    a.media.send_audio(hash, 960, None, &[5, 1]);
    let f = next_frame(&mut b_rx)
        .await
        .expect("UDP peer hears the TLS peer");
    assert_eq!(f.payload, &[5u8, 1][..]);
    b_media.send_audio(hash, 960, None, &[6, 1]);
    let f = next_frame(&mut a.rx)
        .await
        .expect("TLS peer hears the UDP peer");
    assert_eq!(f.payload, &[6u8, 1][..]);

    b_media.stop();
    sfu.shutdown();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn connection_cap_refuses_the_extra_connection() {
    let (sfu, _udp, tls, info) = start_sfu(TlsTunnelOptions {
        max_connections: 1,
        ..tls_options()
    })
    .await;
    let app = AppId::new();
    let channel = ChannelId::new();
    let a = peer(&sfu, tls, &info, app, channel, "a").await;
    let b_session = new_session(&sfu, app, channel, "b");
    let err = MediaTransport::bind_tls(
        tls,
        &info,
        b_session.session_id,
        b_session.ssrc,
        &b_session.media_key,
        Arc::new(AtomicU32::new(1)),
        1,
        Duration::from_millis(500),
    )
    .await
    .err()
    .expect("second connection is refused");
    assert!(
        matches!(err, ClientError::Transport(_) | ClientError::Timeout(_)),
        "{err:?}"
    );
    assert!(a.media.is_bound() && !a.media.is_closed());
    assert_eq!(sfu.tls_tunnel().unwrap().connection_count(), 1);
    sfu.shutdown();
}
