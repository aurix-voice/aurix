// SPDX-FileCopyrightText: 2025 Aurix contributors
// SPDX-License-Identifier: AGPL-3.0-only

//! The TLS tunnel listener against raw TLS clients: what it accepts, what it refuses at the
//! handshake, and how it treats framing violations and connections that never bind.

use aurix_common::framing::{encode_frame, MAX_FRAME_PAYLOAD};

use aurix_common::protocol::{MAX_PACKET_SIZE, TLS_TUNNEL_ALPN};
use aurix_common::quic::tunnel_tls_config;
use aurix_common::types::SessionId;
use aurix_media::cert::MediaCert;
use aurix_media::tls::{TlsLink, TlsTunnelOptions, TlsTunnelServer};
use parking_lot::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio_rustls::rustls::pki_types::ServerName;
use tokio_rustls::TlsConnector;

type Client = tokio_rustls::client::TlsStream<TcpStream>;
type Packets = Arc<Mutex<Vec<(u64, Vec<u8>)>>>;

struct Harness {
    server: Arc<TlsTunnelServer>,
    packets: Packets,
    closed: Arc<AtomicUsize>,
}

async fn start(options: TlsTunnelOptions) -> Harness {
    let cert = MediaCert::self_signed("aurix-test").unwrap();
    let server = TlsTunnelServer::bind(
        "127.0.0.1:0".parse().unwrap(),
        &["127.0.0.1:0".parse().unwrap()],
        options,
        &cert,
    )
    .await
    .unwrap();
    let packets = Arc::new(Mutex::new(Vec::new()));
    let closed = Arc::new(AtomicUsize::new(0));
    let on_packet = {
        let packets = packets.clone();
        move |link: Arc<TlsLink>, data: Vec<u8>| {
            packets.lock().push((link.id(), data));
            Box::pin(async {}) as std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>>
        }
    };
    let on_closed = {
        let closed = closed.clone();
        move |_link: Arc<TlsLink>| {
            closed.fetch_add(1, Ordering::SeqCst);
        }
    };
    server.clone().run_accept_loop(on_packet, on_closed);
    Harness {
        server,
        packets,
        closed,
    }
}

async fn connect(h: &Harness) -> Client {
    connect_with(h, tunnel_tls_config(&h.server.info().cert_sha256).unwrap())
        .await
        .unwrap()
}

async fn connect_with(
    h: &Harness,
    tls: Arc<tokio_rustls::rustls::ClientConfig>,
) -> std::io::Result<Client> {
    let tcp = TcpStream::connect(h.server.local_addr()).await?;
    let name = ServerName::try_from(h.server.info().server_name.clone()).unwrap();
    TlsConnector::from(tls).connect(name, tcp).await
}

/// Reads until the peer closes; `Ok(bytes read)` on a clean EOF, `Err` on a reset.
async fn read_to_close(stream: &mut Client) -> std::io::Result<usize> {
    let mut total = 0;
    let mut buf = [0u8; 256];
    loop {
        match tokio::time::timeout(Duration::from_secs(3), stream.read(&mut buf)).await {
            Ok(Ok(0)) => return Ok(total),
            Ok(Ok(n)) => total += n,
            Ok(Err(e)) => return Err(e),
            Err(_) => panic!("server did not close the connection"),
        }
    }
}

async fn wait_for(mut cond: impl FnMut() -> bool) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    while !cond() {
        assert!(tokio::time::Instant::now() < deadline, "condition not met");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

fn quick() -> TlsTunnelOptions {
    TlsTunnelOptions {
        enabled: true,
        port: 0,
        queue_packets: 8,
        bind_timeout: Duration::from_millis(400),
        idle_timeout: Duration::from_millis(400),
        ..TlsTunnelOptions::default()
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn advertises_the_bound_port_and_the_certificate_pin() {
    let h = start(quick()).await;
    let info = h.server.info();
    assert_eq!(
        info.addrs,
        vec![h.server.local_addr().to_string()],
        "fallback hosts get the port the listener actually got"
    );
    assert_eq!(info.server_name, "aurix-test");
    assert_eq!(info.cert_sha256.len(), 64);
    assert!(info.cert_sha256.chars().all(|c| c.is_ascii_hexdigit()));

    let explicit = TlsTunnelOptions {
        advertise: vec!["voice.example.net:443".into()],
        ..quick()
    };
    let h2 = start(explicit).await;
    assert_eq!(h2.server.info().addrs, vec!["voice.example.net:443"]);
    h.server.shutdown();
    h2.server.shutdown();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn frames_are_delivered_whole_regardless_of_tcp_segmentation() {
    let h = start(TlsTunnelOptions {
        bind_timeout: Duration::from_secs(5),
        idle_timeout: Duration::from_secs(5),
        ..quick()
    })
    .await;
    let mut c = connect(&h).await;
    assert_eq!(c.get_ref().1.alpn_protocol(), Some(TLS_TUNNEL_ALPN));
    wait_for(|| h.server.connection_count() == 1).await;

    let small = encode_frame(&[1u8; 40]).unwrap();
    let full = encode_frame(&vec![2u8; MAX_PACKET_SIZE]).unwrap();
    let mut wire = Vec::new();
    wire.extend_from_slice(&small);
    wire.extend_from_slice(&full);
    wire.extend_from_slice(&small);
    // One byte, then the rest in odd chunks: the decoder must reassemble across reads.
    c.write_all(&wire[..1]).await.unwrap();
    c.flush().await.unwrap();
    tokio::time::sleep(Duration::from_millis(20)).await;
    for chunk in wire[1..].chunks(333) {
        c.write_all(chunk).await.unwrap();
        c.flush().await.unwrap();
    }
    wait_for(|| h.packets.lock().len() == 3).await;
    let got = h.packets.lock().clone();
    assert_eq!(got[0].1, vec![1u8; 40]);
    assert_eq!(got[1].1.len(), MAX_PACKET_SIZE);
    assert_eq!(got[2].1, vec![1u8; 40]);
    assert!(got.iter().all(|(id, _)| *id == got[0].0));
    assert_eq!(h.server.connection_count(), 1, "framing intact, still open");
    drop(c);
    wait_for(|| h.server.connection_count() == 0).await;
    assert_eq!(h.closed.load(Ordering::SeqCst), 1);
    h.server.shutdown();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn empty_frame_closes_the_connection() {
    let h = start(quick()).await;
    let mut c = connect(&h).await;
    c.write_all(&encode_frame(&[7u8; 8]).unwrap())
        .await
        .unwrap();
    c.write_all(&[0, 0]).await.unwrap();
    c.flush().await.unwrap();
    let _ = read_to_close(&mut c).await;
    wait_for(|| h.server.connection_count() == 0).await;
    assert_eq!(
        h.packets.lock().len(),
        1,
        "the valid frame before it was delivered"
    );
    assert_eq!(h.closed.load(Ordering::SeqCst), 1);
    h.server.shutdown();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn oversized_length_closes_before_any_body_is_read() {
    let h = start(quick()).await;
    let mut c = connect(&h).await;
    let len = (MAX_FRAME_PAYLOAD + 1) as u16;
    c.write_all(&len.to_be_bytes()).await.unwrap();
    c.flush().await.unwrap();
    let _ = read_to_close(&mut c).await;
    wait_for(|| h.server.connection_count() == 0).await;
    assert!(h.packets.lock().is_empty());

    let mut c = connect(&h).await;
    c.write_all(&u16::MAX.to_be_bytes()).await.unwrap();
    c.flush().await.unwrap();
    let _ = read_to_close(&mut c).await;
    wait_for(|| h.server.connection_count() == 0).await;
    assert!(h.packets.lock().is_empty());
    assert_eq!(h.closed.load(Ordering::SeqCst), 2);
    h.server.shutdown();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn truncated_frame_at_eof_is_dropped_not_delivered() {
    let h = start(quick()).await;
    let mut c = connect(&h).await;
    let frame = encode_frame(&[3u8; 100]).unwrap();
    c.write_all(&frame[..frame.len() - 1]).await.unwrap();
    c.shutdown().await.unwrap();
    wait_for(|| h.server.connection_count() == 0).await;
    assert!(
        h.packets.lock().is_empty(),
        "a partial frame is never a packet"
    );

    // A lone length byte at EOF is the same violation.
    let mut c = connect(&h).await;
    c.write_all(&[0]).await.unwrap();
    c.shutdown().await.unwrap();
    wait_for(|| h.server.connection_count() == 0).await;
    assert!(h.packets.lock().is_empty());
    h.server.shutdown();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unbound_connection_is_closed_at_the_bind_timeout() {
    let h = start(quick()).await;
    let mut c = connect(&h).await;
    wait_for(|| h.server.connection_count() == 1).await;
    let started = tokio::time::Instant::now();
    // Traffic without a bind does not extend the grace period.
    c.write_all(&encode_frame(&[5u8; 40]).unwrap())
        .await
        .unwrap();
    c.flush().await.unwrap();
    let _ = read_to_close(&mut c).await;
    let elapsed = started.elapsed();
    assert!(elapsed >= Duration::from_millis(300), "{elapsed:?}");
    assert!(elapsed < Duration::from_secs(2), "{elapsed:?}");
    wait_for(|| h.server.connection_count() == 0).await;
    h.server.shutdown();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn wrong_alpn_or_wrong_pin_is_refused_at_the_handshake() {
    let h = start(quick()).await;

    let mut no_alpn = (*tunnel_tls_config(&h.server.info().cert_sha256).unwrap()).clone();
    no_alpn.alpn_protocols = vec![b"h2".to_vec()];
    let res = connect_with(&h, Arc::new(no_alpn)).await;
    match res {
        Err(_) => {}
        Ok(mut stream) => {
            assert_ne!(stream.get_ref().1.alpn_protocol(), Some(TLS_TUNNEL_ALPN));
            let _ = read_to_close(&mut stream).await;
        }
    }
    wait_for(|| h.server.connection_count() == 0).await;

    let wrong_pin = tunnel_tls_config(&"ab".repeat(32)).unwrap();
    assert!(connect_with(&h, wrong_pin).await.is_err());
    wait_for(|| h.server.connection_count() == 0).await;
    assert!(h.packets.lock().is_empty());
    assert_eq!(
        h.closed.load(Ordering::SeqCst),
        0,
        "refused connections never became links"
    );
    h.server.shutdown();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn downlink_queue_overflow_closes_the_link() {
    let h = start(TlsTunnelOptions {
        bind_timeout: Duration::from_secs(5),
        idle_timeout: Duration::from_secs(5),
        ..quick()
    })
    .await;
    let link_slot: Arc<Mutex<Option<Arc<TlsLink>>>> = Arc::new(Mutex::new(None));
    let cert = MediaCert::self_signed("aurix-test").unwrap();
    let server = TlsTunnelServer::bind(
        "127.0.0.1:0".parse().unwrap(),
        &[],
        h.server.options().clone(),
        &cert,
    )
    .await
    .unwrap();
    let on_packet = {
        let slot = link_slot.clone();
        move |link: Arc<TlsLink>, _data: Vec<u8>| {
            *slot.lock() = Some(link);
            Box::pin(async {}) as std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>>
        }
    };
    server.clone().run_accept_loop(on_packet, |_link| {});

    let tcp = TcpStream::connect(server.local_addr()).await.unwrap();
    let name = ServerName::try_from(server.info().server_name.clone()).unwrap();
    let tls = tunnel_tls_config(&server.info().cert_sha256).unwrap();
    let mut c = TlsConnector::from(tls).connect(name, tcp).await.unwrap();
    c.write_all(&encode_frame(&[1u8; 10]).unwrap())
        .await
        .unwrap();
    c.flush().await.unwrap();
    wait_for(|| link_slot.lock().is_some()).await;
    let link = link_slot.lock().clone().unwrap();
    assert!(link.claim(SessionId::new()));

    // The client reads what the node sends: frames come back whole.
    assert!(link.send(vec![9u8; 30]));
    let mut hdr = [0u8; 2];
    c.read_exact(&mut hdr).await.unwrap();
    assert_eq!(u16::from_be_bytes(hdr), 30);
    let mut body = vec![0u8; 30];
    c.read_exact(&mut body).await.unwrap();
    assert_eq!(body, vec![9u8; 30]);

    // A peer that stops reading cannot make the node buffer without bound: sends beyond the
    // queue are dropped and counted, the link stays consistent.
    let mut accepted = 0;
    for _ in 0..1000 {
        if link.send(vec![0u8; MAX_PACKET_SIZE]) {
            accepted += 1;
        }
    }
    assert!(accepted < 1000, "queue is bounded");
    assert!(link.packets_dropped() > 0);
    assert_eq!(link.packets_sent(), accepted + 1);
    link.close("test");
    assert!(!link.is_open());
    let _ = read_to_close(&mut c).await;
    wait_for(|| server.connection_count() == 0).await;
    server.shutdown();
    h.server.shutdown();
}
