//! The native client's QUIC media link against a real `SfuNode` on loopback: pinned
//! handshake, authenticated bind, datagram media, 0-RTT resume, migration, fallbacks.

use aurix_client::media::{
    IncomingAudio, MediaPath, MediaTransport, QuicClientState, SequenceCounter,
};
use aurix_client::ClientError;
use aurix_common::protocol::{channel_id_hash, QuicInfo};
use aurix_common::types::*;
use aurix_media::quic::QuicOptions;
use aurix_media::{MediaEvent, MediaSession, SfuNode, SfuOptions};
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;

const ATTEMPTS: u32 = 3;
const TIMEOUT: Duration = Duration::from_secs(2);
const IDLE: Duration = Duration::from_secs(5);

async fn start_sfu(bind: &str, quic: QuicOptions) -> (SfuNode, SocketAddr, QuicInfo) {
    let mut sfu = SfuNode::new(
        MediaNodeId::new(),
        Region::EuWest,
        SfuOptions {
            max_participants: 4,
            quic,
            ..SfuOptions::default()
        },
    );
    sfu.start(bind.parse().unwrap()).await.unwrap();
    let addr = sfu.local_addr().unwrap();
    let info = sfu.quic_info().expect("node advertises QUIC");
    (sfu, addr, info)
}

struct Peer {
    session: Arc<MediaSession>,
    media: Arc<MediaTransport>,
    rx: mpsc::UnboundedReceiver<IncomingAudio>,
}

async fn connect(
    server: SocketAddr,
    info: &QuicInfo,
    state: &QuicClientState,
    session: &Arc<MediaSession>,
    sequence: SequenceCounter,
) -> Result<Arc<MediaTransport>, ClientError> {
    MediaTransport::bind_quic(
        server,
        info,
        state,
        session.session_id,
        session.ssrc,
        &session.media_key,
        sequence,
        ATTEMPTS,
        TIMEOUT,
        IDLE,
    )
    .await
    .map(Arc::new)
}

async fn peer(
    sfu: &SfuNode,
    server: SocketAddr,
    info: &QuicInfo,
    state: &QuicClientState,
    app: AppId,
    channel: ChannelId,
    name: &str,
) -> Peer {
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
    let media = connect(server, info, state, &session, Arc::new(AtomicU32::new(1)))
        .await
        .expect("QUIC bind");
    let (tx, rx) = mpsc::unbounded_channel();
    media.start(Arc::new(move |frame| {
        let _ = tx.send(frame);
    }));
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
async fn quic_link_binds_and_carries_authenticated_media_both_ways() {
    let (sfu, addr, info) = start_sfu("127.0.0.1:0", QuicOptions::default()).await;
    let state = QuicClientState::for_node(None, &info).unwrap();
    let app = AppId::new();
    let channel = ChannelId::new();
    let mut events = sfu.subscribe_events();

    let mut a = peer(&sfu, addr, &info, &state, app, channel, "a").await;
    assert_eq!(a.media.path(), MediaPath::Quic);
    assert!(a.media.is_bound());
    assert!(!a.media.zero_rtt(), "first connection to a node is 1-RTT");
    assert_eq!(a.session.transport_kind(), MediaTransportKind::Quic);
    assert!(matches!(
        bound_event(&mut events).await,
        MediaEvent::SessionBound {
            transport: MediaTransportKind::Quic,
            ..
        }
    ));
    let mut b = peer(&sfu, addr, &info, &state, app, channel, "b").await;
    // TLS resumption is per node (the shared state holds the ticket), the AURX bind is
    // still authenticated per session: B's early bind names and signs B, not A.
    assert!(
        b.media.zero_rtt(),
        "second connection from the same state resumes"
    );
    assert_eq!(b.session.transport_kind(), MediaTransportKind::Quic);
    assert_eq!(a.session.transport_kind(), MediaTransportKind::Quic);
    assert_eq!(sfu.quic().unwrap().connection_count(), 2);

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
    assert_eq!(got.len(), 5, "B hears every frame A sent over QUIC");
    assert!(got.iter().all(|f| f.sender_ssrc == a.session.ssrc));
    assert!(got
        .iter()
        .all(|f| f.codec == AudioCodec::Opus && !f.e2ee && !f.mixed));
    assert_eq!(got[4].payload[1], 4);
    assert!(
        next_frame(&mut a.rx).await.is_none(),
        "A does not hear itself"
    );

    // PCMU (negotiated on both sides) keeps its flag through the QUIC path.
    a.session.set_codec(AudioCodec::Pcmu).unwrap();
    b.session.set_codec(AudioCodec::Pcmu).unwrap();
    b.media
        .send_audio_frame(hash, 960, None, AudioCodec::Pcmu, &[0xFFu8; 160]);
    let f = next_frame(&mut a.rx).await.expect("PCMU frame");
    assert_eq!(f.codec, AudioCodec::Pcmu);
    assert_eq!(f.payload.len(), 160);
    a.session.set_codec(AudioCodec::Opus).unwrap();
    b.session.set_codec(AudioCodec::Opus).unwrap();

    // An E2EE channel: the sealed payload crosses the QUIC path opaque to the node.
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
    b.media.send_audio_e2ee(
        sealed_hash,
        1920,
        None,
        AudioCodec::Opus,
        b"sealed-by-sender",
    );
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
    assert!(stats.packets_received >= 4);

    // Closing the client connection frees the node's path for the session.
    a.media.stop();
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(
        a.session.quic().is_none(),
        "closed link is cleared as the session's path"
    );
    assert_eq!(sfu.quic().unwrap().connection_count(), 1);
    sfu.shutdown();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn zero_rtt_resume_keeps_sequence_and_rejects_the_stale_connection() {
    let (sfu, addr, info) = start_sfu("127.0.0.1:0", QuicOptions::default()).await;
    let state = QuicClientState::for_node(None, &info).unwrap();
    let app = AppId::new();
    let channel = ChannelId::new();
    let hash = channel_id_hash(&channel);
    let a = peer(&sfu, addr, &info, &state, app, channel, "a").await;
    let mut b = peer(&sfu, addr, &info, &state, app, channel, "b").await;
    let sequence = a.media.sequence_counter();

    for i in 0..3u32 {
        a.media.send_audio(hash, i * 960, None, &[1, i as u8]);
    }
    for _ in 0..3 {
        next_frame(&mut b.rx).await.expect("frame before resume");
    }
    let before = sequence.load(Ordering::Relaxed);

    // Same node, same certificate: the state is reused and the new connection resumes with
    // its bind as 0-RTT early data. The old connection stays open to play the stale peer.
    let same = QuicClientState::for_node(Some(&state), &info).unwrap();
    assert!(Arc::ptr_eq(&same, &state));
    let resumed = connect(addr, &info, &same, &a.session, Arc::clone(&sequence))
        .await
        .expect("0-RTT bind");
    assert!(resumed.zero_rtt(), "resumption ticket → bind in early data");
    assert!(resumed.is_bound());
    assert_eq!(a.session.transport_kind(), MediaTransportKind::Quic);

    // The displaced connection is closed by the node and its media is refused; the new one
    // continues the shared sequence counter, so B's replay window keeps accepting A.
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(a.media.is_closed(), "superseded link is closed by the node");
    a.media.send_audio(hash, 9000, None, &[9, 9]);
    assert!(
        next_frame(&mut b.rx).await.is_none(),
        "stale link carries nothing"
    );
    assert!(sequence.load(Ordering::Relaxed) > before);
    let after_resume = sequence.load(Ordering::Relaxed);
    resumed.send_audio(hash, 3 * 960, None, &[2, 0]);
    let f = next_frame(&mut b.rx).await.expect("frame after resume");
    assert_eq!(f.payload[0], 2);
    assert_eq!(f.sender_ssrc, a.session.ssrc);
    assert!(sequence.load(Ordering::Relaxed) > after_resume);

    // Datagram loss: a gap in the sender's numbering blocks nothing behind it.
    sequence.fetch_add(7, Ordering::Relaxed);
    resumed.send_audio(hash, 4 * 960, None, &[3, 0]);
    let f = next_frame(&mut b.rx).await.expect("frame after a gap");
    assert_eq!(f.payload[0], 3);

    // Resuming again works the same way: the ticket store keeps serving 0-RTT and the newest
    // authenticated bind owns the session.
    let again = connect(addr, &info, &state, &a.session, Arc::clone(&sequence))
        .await
        .expect("second resume");
    assert!(again.zero_rtt());
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(resumed.is_closed());
    again.send_audio(hash, 5 * 960, None, &[4, 0]);
    let f = next_frame(&mut b.rx)
        .await
        .expect("frame after second resume");
    assert_eq!(f.payload[0], 4);
    sfu.shutdown();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rebind_migrates_the_connection_without_a_new_bind() {
    let (sfu, addr, info) = start_sfu("127.0.0.1:0", QuicOptions::default()).await;
    let state = QuicClientState::for_node(None, &info).unwrap();
    let app = AppId::new();
    let channel = ChannelId::new();
    let hash = channel_id_hash(&channel);
    let a = peer(&sfu, addr, &info, &state, app, channel, "a").await;
    let mut b = peer(&sfu, addr, &info, &state, app, channel, "b").await;
    let mut events = sfu.subscribe_events();

    let link = a.session.quic().expect("A is on QUIC");
    let old_local = a.media.local_addr().unwrap();
    let old_remote = link.remote_address();
    assert_eq!(link.migrations(), 0);

    let new_local = a.media.rebind().expect("QUIC link migrates in place");
    assert_ne!(new_local.port(), old_local.port());
    assert_eq!(a.media.local_addr().unwrap(), new_local);
    let b_new_local = b.media.rebind().expect("B migrates too");
    // The node only learns a new path from the first packet on it (quinn pings after a rebind);
    // wait until both migrations are visible before routing audio, or the frame for B
    // legitimately goes to its old, closed socket.
    let b_link = b.session.quic().expect("B is on QUIC");
    wait_for(|| {
        link.remote_address().port() == new_local.port()
            && b_link.remote_address().port() == b_new_local.port()
    })
    .await;

    a.media.send_audio(hash, 960, None, &[7, 1]);
    let f = next_frame(&mut b.rx).await.expect("frame after migration");
    assert_eq!(f.payload, &[7u8, 1][..]);
    assert!(a.media.is_bound() && !a.media.is_closed());
    assert!(
        Arc::ptr_eq(&a.session.quic().unwrap(), &link),
        "same connection, same session path"
    );
    assert_ne!(
        link.remote_address(),
        old_remote,
        "node follows the new path"
    );
    assert_eq!(link.remote_address().port(), new_local.port());
    assert!(link.migrations() >= 1);
    // No SessionBound was needed: migration is below the AURX session layer.
    assert!(tokio::time::timeout(Duration::from_millis(200), async {
        loop {
            if let Ok(MediaEvent::SessionBound { .. }) = events.recv().await {
                return;
            }
        }
    })
    .await
    .is_err());

    // Heartbeats keep flowing on the migrated path.
    a.media.send_heartbeat();
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(a.media.stats().rtt_samples, 1);
    sfu.shutdown();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn wrong_pin_and_wrong_key_are_refused() {
    let (sfu, addr, info) = start_sfu("127.0.0.1:0", QuicOptions::default()).await;
    let app = AppId::new();
    let session = sfu
        .create_session(SessionId::new(), UserId::new(), app, "a".into())
        .unwrap();

    // A pin for some other certificate: the TLS handshake fails before any AURX is sent.
    let wrong_pin = QuicInfo {
        cert_sha256: "00".repeat(32),
        server_name: info.server_name.clone(),
    };
    let state = QuicClientState::for_node(None, &wrong_pin).unwrap();
    let err = connect(
        addr,
        &wrong_pin,
        &state,
        &session,
        Arc::new(AtomicU32::new(1)),
    )
    .await
    .err()
    .expect("pinned handshake must fail");
    assert!(matches!(err, ClientError::Transport(_)), "{err:?}");
    assert!(session.quic().is_none());

    // A valid TLS handshake with the wrong media key: the node never acknowledges the bind.
    let state = QuicClientState::for_node(None, &info).unwrap();
    let err = MediaTransport::bind_quic(
        addr,
        &info,
        &state,
        session.session_id,
        session.ssrc,
        &[0u8; 32],
        Arc::new(AtomicU32::new(1)),
        1,
        Duration::from_millis(500),
        IDLE,
    )
    .await
    .err()
    .expect("unauthenticated bind gets no ack");
    assert!(matches!(err, ClientError::Transport(_)), "{err:?}");
    assert!(session.quic().is_none());
    sfu.shutdown();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn quic_over_ipv6_loopback() {
    let mut sfu = SfuNode::new(
        MediaNodeId::new(),
        Region::EuWest,
        SfuOptions {
            max_participants: 4,
            ..SfuOptions::default()
        },
    );
    if sfu.start("[::1]:0".parse().unwrap()).await.is_err() {
        eprintln!("no IPv6 loopback; skipping");
        return;
    }
    let addr = sfu.local_addr().unwrap();
    let info = sfu.quic_info().unwrap();
    let state = QuicClientState::for_node(None, &info).unwrap();
    let app = AppId::new();
    let channel = ChannelId::new();
    let a = peer(&sfu, addr, &info, &state, app, channel, "a").await;
    let mut b = peer(&sfu, addr, &info, &state, app, channel, "b").await;
    assert!(a.media.local_addr().unwrap().is_ipv6());
    a.media
        .send_audio(channel_id_hash(&channel), 960, None, &[6, 6]);
    assert_eq!(
        next_frame(&mut b.rx)
            .await
            .expect("frame over IPv6")
            .payload[0],
        6
    );
    sfu.shutdown();
}
