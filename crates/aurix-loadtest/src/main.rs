//! Aurix load generator.
//!
//! Creates `channels` channels, mints one player token per session, opens a WebSocket per
//! session, binds native AURX media over the chosen `--transport` (UDP, QUIC, the dedicated
//! TLS tunnel, WebTransport as a browser would, or the WebSocket tunnel), joins the channel and
//! then lets `speakers` players per channel stream 20 ms frames at `pps` for `duration`
//! seconds while every participant authenticates and counts what it receives. Sessions are
//! spread round-robin over every `--ws` node, so two nodes with shared channels exercise the
//! cascade. `--opus` sends real Opus frames (a pre-encoded bank of speech-like audio) so the
//! server-side paths that decode — `--mix` (one server-mixed stream per receiver) and
//! `--noise-suppression` (RNNoise on every speaker's uplink) — do real work.
//!
//! At the end it prints delivery ratio, one-way latency percentiles (send instants are kept
//! per frame timestamp, which the SFU preserves), setup timings and the delta of every
//! node's Prometheus counters, as JSON when `--json` is given.
//!
//! ```text
//! aurix-loadtest --api http://127.0.0.1:8080 --ws ws://127.0.0.1:8081 --api-key aurx_... \
//!     --transport tls --sessions 1000 --channels 100 --speakers 2 --duration 30
//! ```

use anyhow::{anyhow, Context, Result};
use aurix_client::media::{FrameKind, MediaTransport, QuicClientState};
use aurix_client::IncomingAudio;
use aurix_common::crypto::MediaKeys;
use aurix_common::protocol::{
    channel_id_hash, AurixPacket, ControlMessage, PacketFlags, PacketType, QuicInfo, TlsTunnelInfo,
    WebTransportInfo,
};
use aurix_common::types::{ChannelId, DownlinkMode, SessionId};
use base64::Engine;
use bytes::Bytes;
use clap::{Parser, ValueEnum};
use futures_util::{SinkExt, StreamExt};
use std::collections::{BTreeMap, HashMap};
use std::net::SocketAddr;
use std::sync::atomic::{AtomicI64, AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::net::UdpSocket;
use tokio::sync::{mpsc, Semaphore};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::Message;
use wtransport::tls::Sha256Digest;

/// Native media path every session binds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum, serde::Serialize)]
#[serde(rename_all = "snake_case")]
enum Transport {
    /// Raw AURX over UDP (the primary path).
    Udp,
    /// AURX datagrams over QUIC on the media port (`SessionInitAck.quic`).
    Quic,
    /// Length-prefixed AURX frames over the dedicated TLS tunnel (`media.tls_tunnel_port`).
    Tls,
    /// AURX datagrams over WebTransport, as a browser does (`media.webtransport_port`).
    Webtransport,
    /// AURX frames as binary messages on the control WebSocket.
    Tunnel,
}

#[derive(Parser, Debug, Clone)]
#[command(name = "aurix-loadtest", about = "Aurix voice platform load generator")]
struct Args {
    #[arg(long, default_value = "http://127.0.0.1:8080")]
    api: String,
    /// WebSocket base URL of a node; repeat to spread sessions round-robin over several nodes
    /// (channels are shared, so the nodes cascade).
    #[arg(long, default_value = "ws://127.0.0.1:8081")]
    ws: Vec<String>,
    /// App API key (`aurx_...`). Also read from AURIX_LOADTEST_API_KEY.
    #[arg(long, env = "AURIX_LOADTEST_API_KEY")]
    api_key: String,
    /// Prometheus endpoint(s) to scrape before/after, one per `--ws` node (optional).
    #[arg(long, default_value = "http://127.0.0.1:4040/metrics")]
    metrics: Vec<String>,
    /// Media path of every session.
    #[arg(long, value_enum, default_value_t = Transport::Udp)]
    transport: Transport,
    #[arg(long, default_value_t = 1000)]
    sessions: usize,
    #[arg(long, default_value_t = 100)]
    channels: usize,
    /// Speaking participants per channel (the rest only listen).
    #[arg(long, default_value_t = 2)]
    speakers: usize,
    /// Streaming phase length in seconds.
    #[arg(long, default_value_t = 30)]
    duration: u64,
    /// Packets per second per speaker (20 ms Opus frames = 50).
    #[arg(long, default_value_t = 50)]
    pps: u32,
    /// Synthetic payload size in bytes (ignored with `--opus`).
    #[arg(long, default_value_t = 80)]
    payload: usize,
    /// Send real Opus frames (speech-like audio encoded once at `--opus-bitrate`) instead of
    /// synthetic payloads. Implied by `--mix` and `--noise-suppression`.
    #[arg(long)]
    opus: bool,
    /// Bitrate of the Opus bank in bit/s.
    #[arg(long, default_value_t = 32_000)]
    opus_bitrate: i32,
    /// Every session asks for the server-mixed downlink (`SetDownlinkMode { mode: "mixed" }`):
    /// one stereo Opus stream per receiver instead of one per speaker.
    #[arg(long)]
    mix: bool,
    /// Every speaker asks the node to denoise its uplink (`SetNoiseSuppression`).
    #[arg(long)]
    noise_suppression: bool,
    /// Non-speaking members get a listen-only grant (`speak: false`) instead of a speaking one.
    /// Changes nothing on the per-stream path (listeners never send), but a server mix then
    /// serves them from one shared mixer per channel (audience shape) instead of a private
    /// mixer per receiver (team shape, the default: everyone may speak).
    #[arg(long)]
    listen_only: bool,
    /// Concurrent session setups in flight.
    #[arg(long, default_value_t = 64)]
    setup_concurrency: usize,
    /// Pause between the last join and the first frame, so multi-node runs let the cascade
    /// learn channel membership (`cascade_discovery_interval_ms`) before frames flow.
    #[arg(long, default_value_t = 1000)]
    warmup_ms: u64,
    /// Print the report as a single JSON object.
    #[arg(long)]
    json: bool,
}

const LAT_BUCKETS: usize = 5000; // 0.1 ms buckets up to 500 ms
/// Send instants remembered per speaker, indexed by frame number (wraps after ~160 s at 50 pps,
/// far beyond any latency worth measuring).
const SEND_LOG: usize = 8192;
/// Samples per 20 ms frame at 48 kHz.
const FRAME_SAMPLES: usize = 960;
/// Frames in the pre-encoded Opus bank (2 s of audio).
const OPUS_BANK_FRAMES: usize = 100;

#[derive(Default)]
struct Stats {
    sessions_ok: AtomicU64,
    sessions_failed: AtomicU64,
    packets_sent: AtomicU64,
    uplink_dropped: AtomicU64,
    packets_received: AtomicU64,
    mixed_received: AtomicU64,
    packets_bad_auth: AtomicU64,
    speaking_events: AtomicU64,
    ws_messages: AtomicU64,
    ws_closed: AtomicU64,
    mix_acked: AtomicU64,
    ns_enabled: AtomicU64,
    ns_refused: AtomicU64,
    setup_ms_total: AtomicU64,
    setup_ms_max: AtomicU64,
    latency_us_sum: AtomicU64,
    latency_us_max: AtomicU64,
    latency_hist: Vec<AtomicU64>,
}

impl Stats {
    fn new() -> Self {
        Self {
            latency_hist: (0..LAT_BUCKETS).map(|_| AtomicU64::new(0)).collect(),
            ..Default::default()
        }
    }

    fn record_latency(&self, us: u64) {
        self.latency_us_sum.fetch_add(us, Ordering::Relaxed);
        self.latency_us_max.fetch_max(us, Ordering::Relaxed);
        let bucket = ((us / 100) as usize).min(LAT_BUCKETS - 1);
        self.latency_hist[bucket].fetch_add(1, Ordering::Relaxed);
    }

    fn latency_samples(&self) -> u64 {
        self.latency_hist
            .iter()
            .map(|b| b.load(Ordering::Relaxed))
            .sum()
    }

    fn percentile(&self, p: f64) -> f64 {
        let total = self.latency_samples();
        if total == 0 {
            return 0.0;
        }
        let target = ((total as f64) * p).ceil() as u64;
        let mut acc = 0;
        for (i, b) in self.latency_hist.iter().enumerate() {
            acc += b.load(Ordering::Relaxed);
            if acc >= target {
                return (i as f64 + 0.5) * 0.1;
            }
        }
        LAT_BUCKETS as f64 * 0.1
    }

    /// One verified downlink audio frame from any link.
    fn on_audio(&self, sender_ssrc: u32, timestamp: u32, mixed: bool, log: &SendLog) {
        self.packets_received.fetch_add(1, Ordering::Relaxed);
        if mixed {
            self.mixed_received.fetch_add(1, Ordering::Relaxed);
            return;
        }
        if let Some(sent) = log.sent_at(sender_ssrc, timestamp) {
            let now = chrono::Utc::now().timestamp_micros();
            if now >= sent {
                self.record_latency((now - sent) as u64);
            }
        }
    }
}

/// Send instants of every speaker, by SSRC and frame timestamp. The SFU rewrites the sequence
/// of forwarded audio but keeps the timestamp, so a receiver can look up when the frame left
/// the speaker without anything travelling in the payload (which lets payloads be real Opus).
struct SendLog {
    rings: HashMap<u32, Vec<AtomicI64>>,
}

impl SendLog {
    fn new(speakers: impl Iterator<Item = u32>) -> Self {
        Self {
            rings: speakers
                .map(|ssrc| (ssrc, (0..SEND_LOG).map(|_| AtomicI64::new(0)).collect()))
                .collect(),
        }
    }

    fn slot(timestamp: u32) -> usize {
        (timestamp / FRAME_SAMPLES as u32) as usize % SEND_LOG
    }

    fn record(&self, ssrc: u32, timestamp: u32, unix_us: i64) {
        if let Some(ring) = self.rings.get(&ssrc) {
            ring[Self::slot(timestamp)].store(unix_us, Ordering::Relaxed);
        }
    }

    fn sent_at(&self, ssrc: u32, timestamp: u32) -> Option<i64> {
        let v = self.rings.get(&ssrc)?[Self::slot(timestamp)].load(Ordering::Relaxed);
        (v > 0).then_some(v)
    }
}

/// Pre-encoded Opus frames of speech-like audio, cycled by every speaker.
struct OpusBank {
    frames: Vec<Bytes>,
}

impl OpusBank {
    fn generate(bitrate: i32) -> Result<Self> {
        let mut enc = opus::Encoder::new(48_000, opus::Channels::Mono, opus::Application::Voip)
            .map_err(|e| anyhow!("opus encoder: {e}"))?;
        enc.set_bitrate(opus::Bitrate::Bits(bitrate))
            .map_err(|e| anyhow!("opus bitrate: {e}"))?;
        let mut frames = Vec::with_capacity(OPUS_BANK_FRAMES);
        let mut pcm = vec![0i16; FRAME_SAMPLES];
        let mut noise: u32 = 0x9E37_79B9;
        for frame in 0..OPUS_BANK_FRAMES {
            for (i, sample) in pcm.iter_mut().enumerate() {
                let t = (frame * FRAME_SAMPLES + i) as f32 / 48_000.0;
                // Voiced "speech": a 140 Hz fundamental with vibrato and six harmonics whose
                // weights drift, under a 4 Hz syllabic envelope; plus a floor of white noise
                // that gives a noise suppressor something to remove.
                let f0 = 140.0 + 6.0 * (2.0 * std::f32::consts::PI * 5.0 * t).sin();
                let phase = 2.0 * std::f32::consts::PI * f0 * t;
                let mut voiced = 0.0f32;
                for h in 1..=6u32 {
                    let weight = 1.0 / h as f32
                        * (0.6 + 0.4 * (2.0 * std::f32::consts::PI * 0.7 * t + h as f32).sin());
                    voiced += weight * (phase * h as f32).sin();
                }
                let envelope = 0.55 + 0.45 * (2.0 * std::f32::consts::PI * 4.0 * t).sin();
                noise ^= noise << 13;
                noise ^= noise >> 17;
                noise ^= noise << 5;
                let white = (noise as f32 / u32::MAX as f32) * 2.0 - 1.0;
                let s = 0.25 * envelope * voiced + 0.02 * white;
                *sample = (s.clamp(-1.0, 1.0) * i16::MAX as f32) as i16;
            }
            let data = enc
                .encode_vec(&pcm, 1275)
                .map_err(|e| anyhow!("opus encode: {e}"))?;
            frames.push(Bytes::from(data));
        }
        Ok(Self { frames })
    }

    fn frame(&self, index: u64) -> &Bytes {
        &self.frames[index as usize % self.frames.len()]
    }

    fn avg_bytes(&self) -> f64 {
        self.frames.iter().map(|f| f.len()).sum::<usize>() as f64 / self.frames.len() as f64
    }
}

enum Payload {
    Synthetic(Bytes),
    Opus(Arc<OpusBank>),
}

impl Payload {
    fn frame(&self, index: u64) -> Bytes {
        match self {
            Payload::Synthetic(b) => b.clone(),
            Payload::Opus(bank) => bank.frame(index).clone(),
        }
    }
}

type WsStream =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

/// Bound media link of one session.
enum Link {
    Udp {
        socket: Arc<UdpSocket>,
        server: SocketAddr,
    },
    WebTransport {
        conn: wtransport::Connection,
    },
    /// QUIC, TLS tunnel or WebSocket tunnel through the native client core.
    Native(Arc<MediaTransport>),
}

struct Session {
    ws: WsStream,
    ssrc: u32,
    keys: MediaKeys,
    seq: Arc<AtomicU32>,
    link: Link,
    /// Sealed uplink frames of a WebSocket-tunnelled session, to be written as binary messages.
    tunnel_uplink: Option<mpsc::Receiver<Vec<u8>>>,
}

impl Link {
    /// Keeps the media session alive on the node (`session_timeout_secs`) the way a client
    /// does every `heartbeat_interval_ms`.
    fn send_heartbeat(&self, seq: &AtomicU32, ssrc: u32, keys: &MediaKeys) {
        match self {
            Link::Native(mt) => mt.send_heartbeat(),
            Link::Udp { socket, server } => {
                let seq = seq.fetch_add(1, Ordering::Relaxed);
                let mut p = AurixPacket::heartbeat(ssrc, seq.wrapping_mul(FRAME_SAMPLES as u32));
                p.header.sequence = seq;
                let _ = socket.try_send_to(&p.seal(keys), *server);
            }
            Link::WebTransport { conn } => {
                let seq = seq.fetch_add(1, Ordering::Relaxed);
                let mut p = AurixPacket::heartbeat(ssrc, seq.wrapping_mul(FRAME_SAMPLES as u32));
                p.header.sequence = seq;
                let _ = conn.send_datagram(p.seal(keys));
            }
        }
    }

    fn send_audio(
        &self,
        seq: &AtomicU32,
        ssrc: u32,
        keys: &MediaKeys,
        channel_hash: u32,
        timestamp: u32,
        payload: Bytes,
    ) -> bool {
        match self {
            Link::Native(mt) => {
                let before = mt.stats().uplink_dropped;
                mt.send_audio(channel_hash, timestamp, None, &payload);
                mt.stats().uplink_dropped == before
            }
            Link::Udp { socket, server } => {
                let seq = seq.fetch_add(1, Ordering::Relaxed);
                let wire =
                    AurixPacket::audio(seq, timestamp, ssrc, channel_hash, payload).seal(keys);
                socket.try_send_to(&wire, *server).is_ok()
            }
            Link::WebTransport { conn } => {
                let seq = seq.fetch_add(1, Ordering::Relaxed);
                let wire =
                    AurixPacket::audio(seq, timestamp, ssrc, channel_hash, payload).seal(keys);
                conn.send_datagram(wire).is_ok()
            }
        }
    }
}

async fn post_json(
    http: &reqwest::Client,
    url: &str,
    api_key: &str,
    body: serde_json::Value,
) -> Result<serde_json::Value> {
    let mut delay = Duration::from_millis(50);
    for _ in 0..12 {
        let resp = http
            .post(url)
            .header("x-api-key", api_key)
            .json(&body)
            .send()
            .await?;
        if resp.status() == reqwest::StatusCode::TOO_MANY_REQUESTS {
            tokio::time::sleep(delay).await;
            delay = (delay * 2).min(Duration::from_secs(2));
            continue;
        }
        let status = resp.status();
        let text = resp.text().await?;
        if !status.is_success() {
            return Err(anyhow!("{url} -> {status}: {text}"));
        }
        return Ok(serde_json::from_str(&text)?);
    }
    Err(anyhow!("{url}: rate limited for too long"))
}

/// Next control message; binary frames go to `tunnel` (the WebSocket media tunnel) and are
/// reported as `Err(frame kind)` in `Ok(Err(..))` so a bind handshake can wait for its ack.
async fn recv_frame(
    ws: &mut WsStream,
    tunnel: Option<&MediaTransport>,
    timeout: Duration,
) -> Result<std::result::Result<ControlMessage, FrameKind>> {
    loop {
        let m = tokio::time::timeout(timeout, ws.next())
            .await
            .context("ws timeout")?
            .ok_or_else(|| anyhow!("ws closed"))??;
        match m {
            Message::Text(t) => return Ok(Ok(serde_json::from_str(&t)?)),
            Message::Binary(b) => {
                if let Some(mt) = tunnel {
                    return Ok(Err(mt.handle_frame(&b)));
                }
            }
            Message::Ping(_) | Message::Pong(_) => continue,
            Message::Close(_) => return Err(anyhow!("ws closed by server")),
            _ => continue,
        }
    }
}

async fn recv_control(ws: &mut WsStream, timeout: Duration) -> Result<ControlMessage> {
    match recv_frame(ws, None, timeout).await? {
        Ok(m) => Ok(m),
        Err(_) => Err(anyhow!("unexpected binary frame")),
    }
}

/// Waits for `pred` to accept a control message, failing on `Error` and on the deadline.
async fn wait_for<T>(
    ws: &mut WsStream,
    tunnel: Option<&MediaTransport>,
    what: &str,
    mut pred: impl FnMut(std::result::Result<ControlMessage, FrameKind>) -> Option<T>,
) -> Result<T> {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        let frame = recv_frame(ws, tunnel, remaining).await?;
        if let Ok(ControlMessage::Error { code, message, .. }) = &frame {
            return Err(anyhow!("{what} rejected: {code} {message}"));
        }
        if let Some(v) = pred(frame) {
            return Ok(v);
        }
    }
}

struct InitAck {
    session_id: SessionId,
    ssrc: u32,
    media_addr: SocketAddr,
    media_key: Vec<u8>,
    media_tunnel: bool,
    quic: Option<QuicInfo>,
    tls_tunnel: Option<TlsTunnelInfo>,
    webtransport: Option<WebTransportInfo>,
}

fn bind_packet(ack: &InitAck, keys: &MediaKeys) -> Vec<u8> {
    AurixPacket::session_bind(
        &ack.session_id,
        ack.ssrc,
        chrono::Utc::now().timestamp_millis(),
        rand::random(),
    )
    .encode_authenticated(keys)
    .to_vec()
}

fn is_bind_ack(data: &[u8], ssrc: u32, keys: &MediaKeys) -> bool {
    let Ok(mut p) = AurixPacket::decode(data) else {
        return false;
    };
    p.header.packet_type == PacketType::SessionBindAck && p.header.ssrc == ssrc && p.open(keys)
}

async fn bind_udp(ack: &InitAck, keys: &MediaKeys) -> Result<Link> {
    let socket = Arc::new(UdpSocket::bind("0.0.0.0:0").await?);
    let bind = bind_packet(ack, keys);
    let mut buf = vec![0u8; 2048];
    for _ in 0..5 {
        socket.send_to(&bind, ack.media_addr).await?;
        if let Ok(Ok((n, _))) =
            tokio::time::timeout(Duration::from_millis(500), socket.recv_from(&mut buf)).await
        {
            if is_bind_ack(&buf[..n], ack.ssrc, keys) {
                return Ok(Link::Udp {
                    socket,
                    server: ack.media_addr,
                });
            }
        }
    }
    Err(anyhow!("no SessionBindAck over UDP"))
}

async fn bind_webtransport(ack: &InitAck, keys: &MediaKeys) -> Result<Link> {
    let info = ack
        .webtransport
        .as_ref()
        .ok_or_else(|| anyhow!("node offers no WebTransport (media.webtransport_port)"))?;
    let url = info
        .urls
        .first()
        .ok_or_else(|| anyhow!("WebTransport info without URLs"))?;
    let mut digests = Vec::with_capacity(info.cert_sha256.len());
    for h in &info.cert_sha256 {
        let bytes: [u8; 32] = hex::decode(h)
            .ok()
            .and_then(|b| b.try_into().ok())
            .ok_or_else(|| anyhow!("bad WebTransport certificate hash"))?;
        digests.push(Sha256Digest::from(bytes));
    }
    let config = wtransport::ClientConfig::builder()
        .with_bind_default()
        .with_server_certificate_hashes(digests)
        .build();
    let endpoint = wtransport::Endpoint::client(config)?;
    let conn = tokio::time::timeout(Duration::from_secs(5), endpoint.connect(url))
        .await
        .context("WebTransport handshake timeout")??;
    let bind = bind_packet(ack, keys);
    for _ in 0..5 {
        conn.send_datagram(bind.clone())?;
        if let Ok(Ok(d)) =
            tokio::time::timeout(Duration::from_millis(500), conn.receive_datagram()).await
        {
            if is_bind_ack(&d.payload(), ack.ssrc, keys) {
                return Ok(Link::WebTransport { conn });
            }
        }
    }
    Err(anyhow!("no SessionBindAck over WebTransport"))
}

async fn bind_native(
    transport: Transport,
    ack: &InitAck,
    seq: Arc<AtomicU32>,
) -> Result<(Link, Option<mpsc::Receiver<Vec<u8>>>)> {
    let timeout = Duration::from_secs(3);
    match transport {
        Transport::Quic => {
            let info = ack
                .quic
                .as_ref()
                .ok_or_else(|| anyhow!("node offers no QUIC (media.quic)"))?;
            let state = QuicClientState::for_node(None, info)?;
            let mt = MediaTransport::bind_quic(
                ack.media_addr,
                info,
                &state,
                ack.session_id,
                ack.ssrc,
                &ack.media_key,
                seq,
                3,
                timeout,
                Duration::from_secs(20),
            )
            .await?;
            Ok((Link::Native(Arc::new(mt)), None))
        }
        Transport::Tls => {
            let info = ack
                .tls_tunnel
                .as_ref()
                .ok_or_else(|| anyhow!("node offers no TLS tunnel (media.tls_tunnel_port)"))?;
            let server: SocketAddr = info
                .addrs
                .first()
                .ok_or_else(|| anyhow!("TLS tunnel info without addresses"))?
                .parse()?;
            let mt = MediaTransport::bind_tls(
                server,
                info,
                ack.session_id,
                ack.ssrc,
                &ack.media_key,
                seq,
                3,
                timeout,
            )
            .await?;
            Ok((Link::Native(Arc::new(mt)), None))
        }
        Transport::Tunnel => {
            if !ack.media_tunnel {
                return Err(anyhow!("node offers no WebSocket media tunnel"));
            }
            let (tx, rx) = mpsc::channel(256);
            let mt = MediaTransport::tunnel(ack.session_id, ack.ssrc, &ack.media_key, seq, tx);
            Ok((Link::Native(Arc::new(mt)), Some(rx)))
        }
        Transport::Udp | Transport::Webtransport => unreachable!("raw links bind elsewhere"),
    }
}

async fn setup_session(
    args: &Args,
    ws_url: &str,
    token: String,
    channel_id: ChannelId,
    is_speaker: bool,
    stats: &Stats,
) -> Result<Session> {
    let mut req = format!("{ws_url}/ws").into_client_request()?;
    req.headers_mut()
        .insert("authorization", format!("Bearer {token}").parse()?);
    let (mut ws, _) = tokio_tungstenite::connect_async(req)
        .await
        .context("ws connect")?;
    let ack = match recv_control(&mut ws, Duration::from_secs(10)).await? {
        ControlMessage::SessionInitAck {
            session_id,
            ssrc,
            media_addr,
            media_key,
            media_tunnel,
            quic,
            tls_tunnel,
            webtransport,
            ..
        } => InitAck {
            session_id,
            ssrc,
            media_addr: media_addr.parse()?,
            media_key: base64::engine::general_purpose::STANDARD.decode(media_key)?,
            media_tunnel,
            quic,
            tls_tunnel,
            webtransport,
        },
        other => return Err(anyhow!("expected SessionInitAck, got {other:?}")),
    };
    let keys = MediaKeys::derive(&ack.media_key);
    let seq = Arc::new(AtomicU32::new(1));

    let (link, tunnel_uplink) = match args.transport {
        Transport::Udp => (bind_udp(&ack, &keys).await?, None),
        Transport::Webtransport => (bind_webtransport(&ack, &keys).await?, None),
        Transport::Quic | Transport::Tls | Transport::Tunnel => {
            bind_native(args.transport, &ack, seq.clone()).await?
        }
    };
    if let (Link::Native(mt), Some(_)) = (&link, &tunnel_uplink) {
        // WebSocket tunnel: the bind travels as a binary message and the ack comes back the
        // same way, through `MediaTransport::handle_frame`.
        ws.send(Message::Binary(mt.bind_packet())).await?;
        wait_for(&mut ws, Some(mt), "tunnel bind", |f| {
            matches!(f, Err(FrameKind::BindAck)).then_some(())
        })
        .await?;
    }
    let tunnel = match (&link, &tunnel_uplink) {
        (Link::Native(mt), Some(_)) => Some(mt.as_ref()),
        _ => None,
    };

    ws.send(Message::Text(serde_json::to_string(
        &ControlMessage::ChannelJoin { channel_id, token },
    )?))
    .await?;
    wait_for(&mut ws, tunnel, "join", |f| {
        matches!(f, Ok(ControlMessage::ChannelJoinAck { .. })).then_some(())
    })
    .await?;

    if args.mix {
        ws.send(Message::Text(serde_json::to_string(
            &ControlMessage::SetDownlinkMode {
                mode: DownlinkMode::Mixed,
            },
        )?))
        .await?;
        wait_for(&mut ws, tunnel, "mixed downlink", |f| {
            matches!(
                f,
                Ok(ControlMessage::DownlinkModeChanged {
                    mode: DownlinkMode::Mixed
                })
            )
            .then_some(())
        })
        .await?;
        stats.mix_acked.fetch_add(1, Ordering::Relaxed);
    }
    if args.noise_suppression && is_speaker {
        ws.send(Message::Text(serde_json::to_string(
            &ControlMessage::SetNoiseSuppression { enabled: true },
        )?))
        .await?;
        // A full pool answers with an error; the session is still valid and keeps speaking
        // uncleaned, which is what the capacity number is about.
        match wait_for(&mut ws, tunnel, "noise suppression", |f| {
            matches!(
                f,
                Ok(ControlMessage::NoiseSuppressionChanged { enabled: true })
            )
            .then_some(())
        })
        .await
        {
            Ok(()) => stats.ns_enabled.fetch_add(1, Ordering::Relaxed),
            Err(e) if e.to_string().contains("rejected") => {
                stats.ns_refused.fetch_add(1, Ordering::Relaxed)
            }
            Err(e) => return Err(e),
        };
    }
    Ok(Session {
        ws,
        ssrc: ack.ssrc,
        keys,
        seq,
        link,
        tunnel_uplink,
    })
}

/// One Prometheus text exposition parsed into `name -> (labels -> value)`; histograms are
/// skipped, `_total` counters and gauges are what the report needs.
fn parse_metrics(text: &str) -> HashMap<String, BTreeMap<String, f64>> {
    let mut out: HashMap<String, BTreeMap<String, f64>> = HashMap::new();
    for line in text.lines() {
        if line.starts_with('#') || line.is_empty() {
            continue;
        }
        let Some((head, value)) = line.rsplit_once(' ') else {
            continue;
        };
        let Ok(value) = value.parse::<f64>() else {
            continue;
        };
        let (name, labels) = match head.split_once('{') {
            Some((n, rest)) => (n, rest.strip_suffix('}').unwrap_or(rest)),
            None => (head, ""),
        };
        out.entry(name.to_string())
            .or_default()
            .insert(labels.to_string(), value);
    }
    out
}

fn family_sum(m: &HashMap<String, BTreeMap<String, f64>>, name: &str) -> f64 {
    m.get(name).map(|f| f.values().sum()).unwrap_or(0.0)
}

/// Per-label delta of a counter family between two scrapes, without all-zero entries.
fn family_delta(
    before: &HashMap<String, BTreeMap<String, f64>>,
    after: &HashMap<String, BTreeMap<String, f64>>,
    name: &str,
) -> serde_json::Value {
    let mut out = serde_json::Map::new();
    if let Some(a) = after.get(name) {
        for (labels, v) in a {
            let b = before.get(name).and_then(|f| f.get(labels)).unwrap_or(&0.0);
            let d = v - b;
            if d != 0.0 {
                let key = if labels.is_empty() { "total" } else { labels };
                out.insert(key.to_string(), serde_json::json!(d));
            }
        }
    }
    serde_json::Value::Object(out)
}

/// Counters whose per-label deltas the report lists per node.
const COUNTERS: &[&str] = &[
    "aurix_packets_received_total",
    "aurix_packets_sent_total",
    "aurix_packets_dropped_total",
    "aurix_quic_packets_total",
    "aurix_quic_handshakes_total",
    "aurix_tls_tunnel_packets_total",
    "aurix_tls_tunnel_handshakes_total",
    "aurix_webtransport_packets_total",
    "aurix_webtransport_handshakes_total",
    "aurix_tunnel_packets_total",
    "aurix_downlink_mix_frames_total",
    "aurix_noise_suppression_frames_total",
    "aurix_speaker_slot_events_total",
    "aurix_cascade_forwarded_total",
    "aurix_cascade_tcp_dropped_total",
];

/// Gauges sampled from the final scrape.
const GAUGES: &[&str] = &[
    "aurix_active_sessions",
    "aurix_quic_sessions",
    "aurix_tls_tunnel_sessions",
    "aurix_webtransport_sessions",
    "aurix_tunnel_sessions",
    "aurix_downlink_mixers",
    "aurix_noise_suppression_sessions",
    "aurix_cascade_links",
];

fn node_report(url: &str, before: &str, after: &str, duration_s: u64) -> serde_json::Value {
    let b = parse_metrics(before);
    let a = parse_metrics(after);
    let cpu =
        family_sum(&a, "process_cpu_seconds_total") - family_sum(&b, "process_cpu_seconds_total");
    let mut counters = serde_json::Map::new();
    for name in COUNTERS {
        let d = family_delta(&b, &a, name);
        if d.as_object().is_some_and(|o| !o.is_empty()) {
            counters.insert(name.to_string(), d);
        }
    }
    let mut gauges = serde_json::Map::new();
    for name in GAUGES {
        if let Some(f) = a.get(*name) {
            let mut labels = serde_json::Map::new();
            for (l, v) in f {
                if *v != 0.0 {
                    let key = if l.is_empty() { "total" } else { l };
                    labels.insert(key.to_string(), serde_json::json!(v));
                }
            }
            if !labels.is_empty() {
                gauges.insert(name.to_string(), serde_json::Value::Object(labels));
            }
        }
    }
    serde_json::json!({
        "metrics": url,
        "process_cpu_seconds_delta": cpu,
        "cpu_cores_avg": cpu / duration_s.max(1) as f64,
        "process_resident_memory_bytes": family_sum(&a, "process_resident_memory_bytes"),
        "process_open_fds": family_sum(&a, "process_open_fds"),
        "packets_received_delta": family_sum(&a, "aurix_packets_received_total") - family_sum(&b, "aurix_packets_received_total"),
        "packets_sent_delta": family_sum(&a, "aurix_packets_sent_total") - family_sum(&b, "aurix_packets_sent_total"),
        "packets_dropped_delta": family_sum(&a, "aurix_packets_dropped_total") - family_sum(&b, "aurix_packets_dropped_total"),
        "counters": counters,
        "gauges": gauges,
    })
}

fn rss_kib() -> Option<u64> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    status
        .lines()
        .find(|l| l.starts_with("VmRSS:"))?
        .split_whitespace()
        .nth(1)?
        .parse()
        .ok()
}

/// CPU time (user + system) this process has consumed so far, from `/proc/self/stat`.
fn self_cpu_seconds() -> Option<f64> {
    let stat = std::fs::read_to_string("/proc/self/stat").ok()?;
    let fields: Vec<&str> = stat.rsplit(')').next()?.split_whitespace().collect();
    let utime: f64 = fields.get(11)?.parse().ok()?;
    let stime: f64 = fields.get(12)?.parse().ok()?;
    Some((utime + stime) / 100.0)
}

/// Expected downlink frames for a channel with `members` participants of which `speakers`
/// stream `frames` each: per speaker one copy per other member with per-speaker streams, one
/// mixed frame per member that hears at least one other speaker with the server mix (every
/// member with two or more speakers, everyone but the speaker with one).
fn expected_deliveries(members: u64, speakers: u64, frames: u64, mix: bool) -> u64 {
    let speakers = speakers.min(members);
    if mix {
        let hearing = match speakers {
            0 => 0,
            1 => members - 1,
            _ => members,
        };
        hearing * frames
    } else {
        speakers * frames * members.saturating_sub(1)
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .with_writer(std::io::stderr)
        .init();
    let mut args = Args::parse();
    if args.channels == 0 || args.sessions == 0 || args.ws.is_empty() {
        return Err(anyhow!("--sessions, --channels and --ws must be set"));
    }
    if args.mix || args.noise_suppression {
        args.opus = true;
    }
    let http = reqwest::Client::builder()
        .pool_max_idle_per_host(64)
        .build()?;
    let run_id = uuid::Uuid::now_v7();

    let payload = if args.opus {
        let bank = OpusBank::generate(args.opus_bitrate)?;
        tracing::info!(
            "opus bank: {} frames at {} bit/s, {:.1} bytes avg",
            bank.frames.len(),
            args.opus_bitrate,
            bank.avg_bytes()
        );
        Payload::Opus(Arc::new(bank))
    } else {
        Payload::Synthetic(Bytes::from(vec![0xFCu8; args.payload.max(1)]))
    };
    let payload_desc = match &payload {
        Payload::Synthetic(b) => serde_json::json!({"kind": "synthetic", "bytes": b.len()}),
        Payload::Opus(bank) => serde_json::json!({
            "kind": "opus", "bitrate": args.opus_bitrate, "avg_bytes": bank.avg_bytes(),
        }),
    };

    let scrape = |http: reqwest::Client, url: String| async move {
        http.get(url).send().await.ok()?.text().await.ok()
    };
    let mut metrics_before = Vec::with_capacity(args.metrics.len());
    for url in &args.metrics {
        metrics_before.push(scrape(http.clone(), url.clone()).await);
    }

    // 1. Channels.
    let t0 = Instant::now();
    let mut channels = Vec::with_capacity(args.channels);
    for i in 0..args.channels {
        let ch = post_json(
            &http,
            &format!("{}/v1/channels", args.api),
            &args.api_key,
            serde_json::json!({"name": format!("load-{run_id}-{i}"), "max_participants": 1024}),
        )
        .await?;
        channels.push(ChannelId::from_uuid(ch["id"].as_str().unwrap().parse()?));
    }
    let channels_ms = t0.elapsed().as_millis();

    // 2. Tokens (one per session; rate-limit aware).
    let t0 = Instant::now();
    let sem = Arc::new(Semaphore::new(args.setup_concurrency.min(16)));
    let mut token_tasks = Vec::with_capacity(args.sessions);
    for i in 0..args.sessions {
        let ch = channels[i % args.channels];
        let speak = !args.listen_only || i / args.channels < args.speakers;
        let http = http.clone();
        let api = args.api.clone();
        let key = args.api_key.clone();
        let sem = sem.clone();
        token_tasks.push(tokio::spawn(async move {
            let _permit = sem.acquire_owned().await.unwrap();
            let r = post_json(
                &http,
                &format!("{api}/v1/tokens"),
                &key,
                serde_json::json!({
                    "external_id": format!("load:{run_id}:{i}"),
                    "display_name": format!("Bot {i}"),
                    "channels": [{"channel_id": ch, "join": true, "speak": speak, "receive": true, "moderate": false}]
                }),
            )
            .await?;
            Ok::<(String, ChannelId), anyhow::Error>((
                r["token"].as_str().unwrap().to_string(),
                ch,
            ))
        }));
    }
    let mut tokens = Vec::with_capacity(args.sessions);
    for t in token_tasks {
        tokens.push(t.await??);
    }
    let tokens_ms = t0.elapsed().as_millis();
    tracing::info!(
        "{} channels in {} ms, {} tokens in {} ms",
        args.channels,
        channels_ms,
        tokens.len(),
        tokens_ms
    );

    // 3. Sessions: WS + media bind + join (+ mixed downlink / noise suppression). Session i
    // is member i / channels of its channel and goes to node (i / channels) % nodes, so every
    // channel spans all nodes (cascade traffic); the first `speakers` members speak.
    let stats = Arc::new(Stats::new());
    let t0 = Instant::now();
    let sem = Arc::new(Semaphore::new(args.setup_concurrency));
    let mut setup_tasks = Vec::with_capacity(args.sessions);
    let mut speaker_slot = HashMap::<ChannelId, usize>::new();
    for (i, (token, ch)) in tokens.into_iter().enumerate() {
        let slot = speaker_slot.entry(ch).or_default();
        let is_speaker = *slot < args.speakers;
        *slot += 1;
        let args = args.clone();
        let sem = sem.clone();
        let stats = stats.clone();
        setup_tasks.push(tokio::spawn(async move {
            let _permit = sem.acquire_owned().await.unwrap();
            let started = Instant::now();
            let ws_url = &args.ws[(i / args.channels) % args.ws.len()];
            match setup_session(&args, ws_url, token, ch, is_speaker, &stats).await {
                Ok(s) => {
                    let ms = started.elapsed().as_millis() as u64;
                    stats.setup_ms_total.fetch_add(ms, Ordering::Relaxed);
                    stats.setup_ms_max.fetch_max(ms, Ordering::Relaxed);
                    stats.sessions_ok.fetch_add(1, Ordering::Relaxed);
                    Some((ch, is_speaker, s))
                }
                Err(e) => {
                    stats.sessions_failed.fetch_add(1, Ordering::Relaxed);
                    tracing::warn!("session {i} ({ws_url}) failed: {e:#}");
                    None
                }
            }
        }));
    }
    let mut sessions = Vec::with_capacity(args.sessions);
    for t in setup_tasks {
        if let Some(s) = t.await? {
            sessions.push(s);
        }
    }
    let setup_ms = t0.elapsed().as_millis();
    let ok = stats.sessions_ok.load(Ordering::Relaxed);
    tracing::info!(
        "{} sessions up ({} failed) in {} ms",
        ok,
        stats.sessions_failed.load(Ordering::Relaxed),
        setup_ms
    );

    let mut members = HashMap::<ChannelId, u64>::new();
    let mut speakers = HashMap::<ChannelId, u64>::new();
    for (ch, is_speaker, _) in &sessions {
        *members.entry(*ch).or_default() += 1;
        if *is_speaker {
            *speakers.entry(*ch).or_default() += 1;
        }
    }
    let frames_per_speaker = args.pps as u64 * args.duration;
    let expected_rx: u64 = members
        .iter()
        .map(|(ch, m)| {
            expected_deliveries(
                *m,
                speakers.get(ch).copied().unwrap_or(0),
                frames_per_speaker,
                args.mix,
            )
        })
        .sum();
    let speaking: u64 = speakers.values().sum();
    let send_log = Arc::new(SendLog::new(
        sessions
            .iter()
            .filter(|(_, s, _)| *s)
            .map(|(_, _, s)| s.ssrc),
    ));

    // 4. Streaming phase.
    let stop = Arc::new(tokio::sync::Notify::new());
    let mut tasks = Vec::new();
    let mut native_links = Vec::new();
    for (ch, is_speaker, session) in sessions {
        let Session {
            mut ws,
            ssrc,
            keys,
            seq,
            link,
            tunnel_uplink,
        } = session;
        let link = Arc::new(link);

        // Receiver: authenticate every downlink audio frame, take its latency.
        match link.as_ref() {
            Link::Udp { socket, .. } => {
                let socket = socket.clone();
                let keys = keys.clone();
                let stats = stats.clone();
                let stop = stop.clone();
                let log = send_log.clone();
                tasks.push(tokio::spawn(async move {
                    let stopped = stop.notified();
                    tokio::pin!(stopped);
                    let mut buf = vec![0u8; 2048];
                    loop {
                        tokio::select! {
                            _ = &mut stopped => return,
                            r = socket.recv_from(&mut buf) => {
                                let Ok((n, _)) = r else { return };
                                on_raw_frame(&buf[..n], &keys, &stats, &log);
                            }
                        }
                    }
                }));
            }
            Link::WebTransport { conn } => {
                let conn = conn.clone();
                let keys = keys.clone();
                let stats = stats.clone();
                let stop = stop.clone();
                let log = send_log.clone();
                tasks.push(tokio::spawn(async move {
                    let stopped = stop.notified();
                    tokio::pin!(stopped);
                    loop {
                        tokio::select! {
                            _ = &mut stopped => return,
                            r = conn.receive_datagram() => {
                                let Ok(d) = r else { return };
                                on_raw_frame(&d.payload(), &keys, &stats, &log);
                            }
                        }
                    }
                }));
            }
            Link::Native(mt) => {
                let stats = stats.clone();
                let log = send_log.clone();
                mt.start(Arc::new(move |frame: IncomingAudio| {
                    stats.on_audio(frame.sender_ssrc, frame.timestamp, frame.mixed, &log);
                }));
                native_links.push(mt.clone());
            }
        }
        // WS pump: keep the control channel alive, count server events, carry the tunnel and
        // send the media heartbeat.
        {
            let stats = stats.clone();
            let stop = stop.clone();
            let link = link.clone();
            let keys = keys.clone();
            let seq = seq.clone();
            let mut uplink = tunnel_uplink;
            tasks.push(tokio::spawn(async move {
                let stopped = stop.notified();
                tokio::pin!(stopped);
                let mut heartbeat = tokio::time::interval(Duration::from_secs(5));
                heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
                loop {
                    tokio::select! {
                        _ = &mut stopped => {
                            let _ = ws.close(None).await;
                            return;
                        }
                        _ = heartbeat.tick() => link.send_heartbeat(&seq, ssrc, &keys),
                        frame = async {
                            match uplink.as_mut() {
                                Some(rx) => rx.recv().await,
                                None => std::future::pending().await,
                            }
                        } => {
                            let Some(frame) = frame else { return };
                            if ws.send(Message::Binary(frame)).await.is_err() {
                                stats.ws_closed.fetch_add(1, Ordering::Relaxed);
                                return;
                            }
                        }
                        m = ws.next() => {
                            match m {
                                Some(Ok(Message::Text(t))) => {
                                    stats.ws_messages.fetch_add(1, Ordering::Relaxed);
                                    if t.contains("SpeakingStateChanged") {
                                        stats.speaking_events.fetch_add(1, Ordering::Relaxed);
                                    }
                                }
                                Some(Ok(Message::Binary(b))) => {
                                    if let Link::Native(mt) = link.as_ref() {
                                        mt.handle_frame(&b);
                                    }
                                }
                                Some(Ok(Message::Ping(p))) => { let _ = ws.send(Message::Pong(p)).await; }
                                Some(Ok(_)) => {}
                                _ => {
                                    stats.ws_closed.fetch_add(1, Ordering::Relaxed);
                                    return;
                                }
                            }
                        }
                    }
                }
            }));
        }
        // Speaker: pps frames/s for the whole phase.
        if is_speaker {
            let hash = channel_id_hash(&ch);
            let stats = stats.clone();
            let log = send_log.clone();
            let warmup = Duration::from_millis(args.warmup_ms);
            let payload = match &payload {
                Payload::Synthetic(b) => Payload::Synthetic(b.clone()),
                Payload::Opus(bank) => Payload::Opus(bank.clone()),
            };
            let period = Duration::from_micros(1_000_000 / args.pps as u64);
            let bank_offset = u64::from(ssrc) % OPUS_BANK_FRAMES as u64;
            tasks.push(tokio::spawn(async move {
                tokio::time::sleep(warmup).await;
                let mut ticker = tokio::time::interval(period);
                ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Burst);
                for frame in 1..=frames_per_speaker {
                    ticker.tick().await;
                    let timestamp = (frame as u32).wrapping_mul(FRAME_SAMPLES as u32);
                    log.record(ssrc, timestamp, chrono::Utc::now().timestamp_micros());
                    let body = payload.frame(frame + bank_offset);
                    if link.send_audio(&seq, ssrc, &keys, hash, timestamp, body) {
                        stats.packets_sent.fetch_add(1, Ordering::Relaxed);
                    } else {
                        stats.uplink_dropped.fetch_add(1, Ordering::Relaxed);
                    }
                }
            }));
        }
    }
    tracing::info!(
        "streaming {} s over {:?}: {} speakers x {} pps, expecting ~{} deliveries",
        args.duration,
        args.transport,
        speaking,
        args.pps,
        expected_rx
    );
    tokio::time::sleep(
        Duration::from_millis(args.warmup_ms) + Duration::from_secs(args.duration + 2),
    )
    .await;
    // Every task pinned its `Notified` before its loop, so none can miss this wake-up.
    stop.notify_waiters();
    let _ = tokio::time::timeout(
        Duration::from_secs(10),
        futures_util::future::join_all(tasks),
    )
    .await;
    let mut native_bad_auth = 0u64;
    let mut native_replayed = 0u64;
    for mt in &native_links {
        let s = mt.stats();
        native_bad_auth += s.bad_auth;
        native_replayed += s.replayed;
        mt.stop();
    }
    let mut metrics_after = Vec::with_capacity(args.metrics.len());
    for url in &args.metrics {
        metrics_after.push(scrape(http.clone(), url.clone()).await);
    }

    // 5. Report.
    let sent = stats.packets_sent.load(Ordering::Relaxed);
    let received = stats.packets_received.load(Ordering::Relaxed);
    let delivery = if expected_rx > 0 {
        received as f64 / expected_rx as f64
    } else {
        0.0
    };
    let lat_count = stats.latency_samples();
    let bad_auth = stats.packets_bad_auth.load(Ordering::Relaxed) + native_bad_auth;
    let server: Vec<serde_json::Value> = args
        .metrics
        .iter()
        .zip(metrics_before.iter().zip(metrics_after.iter()))
        .filter_map(|(url, (b, a))| match (b, a) {
            (Some(b), Some(a)) => Some(node_report(url, b, a, args.duration)),
            _ => None,
        })
        .collect();
    let report = serde_json::json!({
        "run_id": run_id,
        "config": {
            "transport": args.transport, "nodes": args.ws, "sessions": args.sessions,
            "channels": args.channels, "speakers_per_channel": args.speakers,
            "duration_s": args.duration, "warmup_ms": args.warmup_ms, "pps": args.pps, "payload": payload_desc,
            "mix": args.mix, "noise_suppression": args.noise_suppression, "listen_only": args.listen_only,
        },
        "setup": {
            "channels_ms": channels_ms, "tokens_ms": tokens_ms, "sessions_ms": setup_ms,
            "sessions_ok": ok, "sessions_failed": stats.sessions_failed.load(Ordering::Relaxed),
            "session_setup_avg_ms": stats.setup_ms_total.load(Ordering::Relaxed).checked_div(ok).unwrap_or(0),
            "session_setup_max_ms": stats.setup_ms_max.load(Ordering::Relaxed),
            "speakers": speaking,
            "mix_acked": stats.mix_acked.load(Ordering::Relaxed),
            "noise_suppression_enabled": stats.ns_enabled.load(Ordering::Relaxed),
            "noise_suppression_refused": stats.ns_refused.load(Ordering::Relaxed),
        },
        "media": {
            "packets_sent": sent, "uplink_dropped": stats.uplink_dropped.load(Ordering::Relaxed),
            "deliveries_expected": expected_rx, "deliveries_received": received,
            "mixed_frames_received": stats.mixed_received.load(Ordering::Relaxed),
            "delivery_ratio": delivery, "bad_auth": bad_auth, "replayed": native_replayed,
            "sent_pps": sent as f64 / args.duration as f64,
            "received_pps": received as f64 / args.duration as f64,
            "latency_ms": {
                "samples": lat_count,
                "avg": if lat_count > 0 { stats.latency_us_sum.load(Ordering::Relaxed) as f64 / lat_count as f64 / 1000.0 } else { 0.0 },
                "p50": stats.percentile(0.50), "p95": stats.percentile(0.95), "p99": stats.percentile(0.99),
                "max": stats.latency_us_max.load(Ordering::Relaxed) as f64 / 1000.0,
            },
        },
        "control": {
            "ws_messages": stats.ws_messages.load(Ordering::Relaxed),
            "speaking_events": stats.speaking_events.load(Ordering::Relaxed),
            "ws_closed": stats.ws_closed.load(Ordering::Relaxed),
        },
        "server": server,
        "loadgen_rss_kib": rss_kib(),
        "loadgen_cpu_seconds": self_cpu_seconds(),
    });
    if args.json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        println!("== Aurix load test {run_id} ({:?}) ==", args.transport);
        println!(
            "sessions: {ok} ok / {} failed on {} node(s), setup {} ms total (avg {} ms, max {} ms per session); {} speakers, mix acked {}, noise suppression {} on / {} refused",
            report["setup"]["sessions_failed"],
            args.ws.len(),
            setup_ms,
            report["setup"]["session_setup_avg_ms"],
            report["setup"]["session_setup_max_ms"],
            speaking,
            report["setup"]["mix_acked"],
            report["setup"]["noise_suppression_enabled"],
            report["setup"]["noise_suppression_refused"],
        );
        println!(
            "media: sent {sent} ({:.0} pps, {} dropped at the uplink), delivered {received}/{expected_rx} ({:.2}%, {} mixed), bad auth {bad_auth}, replayed {native_replayed}",
            report["media"]["sent_pps"].as_f64().unwrap_or(0.0),
            report["media"]["uplink_dropped"],
            delivery * 100.0,
            report["media"]["mixed_frames_received"],
        );
        if lat_count > 0 {
            println!(
                "latency one-way ms: avg {:.2} p50 {:.1} p95 {:.1} p99 {:.1} max {:.1} ({} samples)",
                report["media"]["latency_ms"]["avg"].as_f64().unwrap_or(0.0),
                report["media"]["latency_ms"]["p50"].as_f64().unwrap_or(0.0),
                report["media"]["latency_ms"]["p95"].as_f64().unwrap_or(0.0),
                report["media"]["latency_ms"]["p99"].as_f64().unwrap_or(0.0),
                report["media"]["latency_ms"]["max"].as_f64().unwrap_or(0.0),
                lat_count
            );
        } else {
            println!("latency one-way: n/a (server-mixed frames carry the mixer's clock)");
        }
        println!(
            "control: {} ws messages, {} speaking events, {} ws closed",
            report["control"]["ws_messages"],
            report["control"]["speaking_events"],
            report["control"]["ws_closed"]
        );
        for node in &server {
            println!(
                "node {}: cpu {:.2} cores avg ({:.1} s), rss {:.0} MiB, fds {}, in {} / out {} / dropped {} packets",
                node["metrics"].as_str().unwrap_or(""),
                node["cpu_cores_avg"].as_f64().unwrap_or(0.0),
                node["process_cpu_seconds_delta"].as_f64().unwrap_or(0.0),
                node["process_resident_memory_bytes"].as_f64().unwrap_or(0.0) / 1_048_576.0,
                node["process_open_fds"],
                node["packets_received_delta"],
                node["packets_sent_delta"],
                node["packets_dropped_delta"],
            );
            println!("  counters: {}", node["counters"]);
            println!("  gauges: {}", node["gauges"]);
        }
        println!(
            "loadgen: rss {} KiB, cpu {} s",
            report["loadgen_rss_kib"], report["loadgen_cpu_seconds"]
        );
    }
    Ok(())
}

/// Authenticates one raw datagram (UDP / WebTransport) and counts it.
fn on_raw_frame(data: &[u8], keys: &MediaKeys, stats: &Stats, log: &SendLog) {
    let Ok(mut p) = AurixPacket::decode(data) else {
        return;
    };
    if !matches!(
        p.header.packet_type,
        PacketType::Audio | PacketType::AudioFec
    ) {
        return;
    }
    if !p.open(keys) {
        stats.packets_bad_auth.fetch_add(1, Ordering::Relaxed);
        return;
    }
    stats.on_audio(
        p.header.ssrc,
        p.header.timestamp,
        p.header.has_flag(PacketFlags::Mixed),
        log,
    );
}
