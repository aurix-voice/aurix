//! Aurix load generator.
//!
//! Creates `channels` channels, mints one player token per session, opens a WebSocket per
//! session, binds native AURX media over UDP, joins the channel and then lets `speakers` players
//! per channel stream Opus-sized packets at 50 pps for `duration` seconds while every participant
//! counts what it receives. At the end it prints delivery ratio, one-way latency percentiles
//! (the send timestamp travels in the payload), setup timings and the server's Prometheus
//! counters, as JSON when `--json` is given.
//!
//! ```text
//! aurix-loadtest --api http://127.0.0.1:8080 --ws ws://127.0.0.1:8081 --api-key aurx_... \
//!     --sessions 1000 --channels 100 --speakers 2 --duration 30
//! ```

use anyhow::{anyhow, Context, Result};
use aurix_common::crypto::MediaKeys;
use aurix_common::protocol::{channel_id_hash, AurixPacket, ControlMessage, PacketType};
use aurix_common::types::{ChannelId, SessionId};
use base64::Engine;
use bytes::Bytes;
use clap::Parser;
use futures_util::{SinkExt, StreamExt};
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::net::UdpSocket;
use tokio::sync::Semaphore;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::Message;

#[derive(Parser, Debug, Clone)]
#[command(name = "aurix-loadtest", about = "Aurix voice platform load generator")]
struct Args {
    #[arg(long, default_value = "http://127.0.0.1:8080")]
    api: String,
    #[arg(long, default_value = "ws://127.0.0.1:8081")]
    ws: String,
    /// App API key (`aurx_...`). Also read from AURIX_LOADTEST_API_KEY.
    #[arg(long, env = "AURIX_LOADTEST_API_KEY")]
    api_key: String,
    /// Prometheus endpoint to scrape before/after (optional).
    #[arg(long, default_value = "http://127.0.0.1:4040/metrics")]
    metrics: String,
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
    /// Opus-like payload size in bytes.
    #[arg(long, default_value_t = 80)]
    payload: usize,
    /// Concurrent session setups in flight.
    #[arg(long, default_value_t = 64)]
    setup_concurrency: usize,
    /// Print the report as a single JSON object.
    #[arg(long)]
    json: bool,
}

const LAT_BUCKETS: usize = 5000; // 0.1 ms buckets up to 500 ms

#[derive(Default)]
struct Stats {
    sessions_ok: AtomicU64,
    sessions_failed: AtomicU64,
    packets_sent: AtomicU64,
    packets_received: AtomicU64,
    packets_bad_auth: AtomicU64,
    speaking_events: AtomicU64,
    ws_messages: AtomicU64,
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
    fn percentile(&self, p: f64) -> f64 {
        let total: u64 = self
            .latency_hist
            .iter()
            .map(|b| b.load(Ordering::Relaxed))
            .sum();
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
}

struct Session {
    ws: tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
    session_id: SessionId,
    ssrc: u32,
    keys: MediaKeys,
    media_addr: SocketAddr,
    udp: Arc<UdpSocket>,
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

async fn recv_control(session: &mut Session, timeout: Duration) -> Result<ControlMessage> {
    loop {
        let m = tokio::time::timeout(timeout, session.ws.next())
            .await
            .context("ws timeout")?
            .ok_or_else(|| anyhow!("ws closed"))??;
        match m {
            Message::Text(t) => return Ok(serde_json::from_str(&t)?),
            Message::Ping(_) | Message::Pong(_) => continue,
            Message::Close(_) => return Err(anyhow!("ws closed by server")),
            _ => continue,
        }
    }
}

async fn setup_session(args: &Args, token: String, channel_id: ChannelId) -> Result<Session> {
    let mut req = format!("{}/ws", args.ws).into_client_request()?;
    req.headers_mut()
        .insert("authorization", format!("Bearer {token}").parse()?);
    let (ws, _) = tokio_tungstenite::connect_async(req)
        .await
        .context("ws connect")?;
    let udp = Arc::new(UdpSocket::bind("0.0.0.0:0").await?);
    let mut s = Session {
        ws,
        session_id: SessionId::new(),
        ssrc: 0,
        keys: MediaKeys::derive(&[0u8; 32]),
        media_addr: "0.0.0.0:0".parse()?,
        udp,
    };
    match recv_control(&mut s, Duration::from_secs(10)).await? {
        ControlMessage::SessionInitAck {
            session_id,
            ssrc,
            media_addr,
            media_key,
        } => {
            s.session_id = session_id;
            s.ssrc = ssrc;
            s.media_addr = media_addr.parse()?;
            let key = base64::engine::general_purpose::STANDARD.decode(media_key)?;
            s.keys = MediaKeys::derive(&key);
        }
        other => return Err(anyhow!("expected SessionInitAck, got {other:?}")),
    }
    // Bind media (authenticated with the per-session key), wait for the UDP ack.
    let bind = AurixPacket::session_bind(
        &s.session_id,
        s.ssrc,
        chrono::Utc::now().timestamp_millis(),
        rand::random(),
    );
    let mut buf = vec![0u8; 2048];
    let mut bound = false;
    for _ in 0..5 {
        s.udp
            .send_to(&bind.encode_authenticated(&s.keys), s.media_addr)
            .await?;
        if let Ok(Ok((n, _))) =
            tokio::time::timeout(Duration::from_millis(500), s.udp.recv_from(&mut buf)).await
        {
            let mut p = AurixPacket::decode(&buf[..n])?;
            if p.header.packet_type == PacketType::SessionBindAck && p.open(&s.keys) {
                bound = true;
                break;
            }
        }
    }
    if !bound {
        return Err(anyhow!("no SessionBindAck"));
    }
    s.ws.send(Message::Text(serde_json::to_string(
        &ControlMessage::ChannelJoin { channel_id, token },
    )?))
    .await?;
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        match recv_control(&mut s, remaining).await? {
            ControlMessage::ChannelJoinAck { .. } => return Ok(s),
            ControlMessage::Error { code, message } => {
                return Err(anyhow!("join rejected: {code} {message}"))
            }
            _ => continue,
        }
    }
}

fn scrape_counter(metrics: &str, name: &str) -> f64 {
    metrics
        .lines()
        .filter(|l| l.starts_with(name) && !l.starts_with('#'))
        .filter_map(|l| l.rsplit(' ').next()?.parse::<f64>().ok())
        .sum()
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

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();
    let args = Args::parse();
    if args.channels == 0 || args.sessions == 0 {
        return Err(anyhow!("--sessions and --channels must be > 0"));
    }
    let http = reqwest::Client::builder()
        .pool_max_idle_per_host(64)
        .build()?;
    let run_id = uuid::Uuid::now_v7();

    let scrape = |http: reqwest::Client, url: String| async move {
        http.get(url).send().await.ok()?.text().await.ok()
    };
    let metrics_before = scrape(http.clone(), args.metrics.clone()).await;

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
                    "channels": [{"channel_id": ch, "join": true, "speak": true, "receive": true, "moderate": false}]
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

    // 3. Sessions: WS + UDP bind + join.
    let stats = Arc::new(Stats::new());
    let t0 = Instant::now();
    let sem = Arc::new(Semaphore::new(args.setup_concurrency));
    let mut setup_tasks = Vec::with_capacity(args.sessions);
    for (i, (token, ch)) in tokens.into_iter().enumerate() {
        let args = args.clone();
        let sem = sem.clone();
        let stats = stats.clone();
        setup_tasks.push(tokio::spawn(async move {
            let _permit = sem.acquire_owned().await.unwrap();
            let started = Instant::now();
            match setup_session(&args, token, ch).await {
                Ok(s) => {
                    let ms = started.elapsed().as_millis() as u64;
                    stats.setup_ms_total.fetch_add(ms, Ordering::Relaxed);
                    stats.setup_ms_max.fetch_max(ms, Ordering::Relaxed);
                    stats.sessions_ok.fetch_add(1, Ordering::Relaxed);
                    Some((i, ch, s))
                }
                Err(e) => {
                    stats.sessions_failed.fetch_add(1, Ordering::Relaxed);
                    tracing::warn!("session {i} failed: {e:#}");
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

    // Expected receive count: for every channel, each packet reaches (members - 1) peers.
    let mut members_per_channel = std::collections::HashMap::<ChannelId, u64>::new();
    for (_, ch, _) in &sessions {
        *members_per_channel.entry(*ch).or_default() += 1;
    }

    // 4. Streaming phase.
    let stream_for = Duration::from_secs(args.duration);
    let stop = Arc::new(tokio::sync::Notify::new());
    let mut tasks = Vec::new();
    let mut expected_rx: u64 = 0;
    let frames_per_speaker = args.pps as u64 * args.duration;
    let mut speaker_slot = std::collections::HashMap::<ChannelId, usize>::new();
    for (_, ch, session) in sessions {
        let Session {
            mut ws,
            ssrc,
            keys,
            media_addr,
            udp,
            ..
        } = session;
        let slot = speaker_slot.entry(ch).or_default();
        let is_speaker = *slot < args.speakers;
        *slot += 1;
        if is_speaker {
            expected_rx += frames_per_speaker * (members_per_channel[&ch] - 1);
        }

        // Receiver: count authenticated audio packets and their latency.
        {
            let udp = udp.clone();
            let key = keys.clone();
            let stats = stats.clone();
            let stop = stop.clone();
            tasks.push(tokio::spawn(async move {
                let mut buf = vec![0u8; 2048];
                loop {
                    tokio::select! {
                        _ = stop.notified() => return,
                        r = udp.recv_from(&mut buf) => {
                            let Ok((n, _)) = r else { return };
                            let Ok(mut p) = AurixPacket::decode(&buf[..n]) else { continue };
                            if p.header.packet_type != PacketType::Audio { continue; }
                            if !p.open(&key) {
                                stats.packets_bad_auth.fetch_add(1, Ordering::Relaxed);
                                continue;
                            }
                            stats.packets_received.fetch_add(1, Ordering::Relaxed);
                            if p.payload.len() >= 9 {
                                let sent = i64::from_be_bytes(p.payload[1..9].try_into().unwrap());
                                let now = chrono::Utc::now().timestamp_micros();
                                if now >= sent {
                                    stats.record_latency((now - sent) as u64);
                                }
                            }
                        }
                    }
                }
            }));
        }
        // WS pump: keep the control channel alive and count server events.
        {
            let stats = stats.clone();
            let stop = stop.clone();
            tasks.push(tokio::spawn(async move {
                loop {
                    tokio::select! {
                        _ = stop.notified() => {
                            let _ = ws.close(None).await;
                            return;
                        }
                        m = ws.next() => {
                            match m {
                                Some(Ok(Message::Text(t))) => {
                                    stats.ws_messages.fetch_add(1, Ordering::Relaxed);
                                    if t.contains("SpeakingStateChanged") {
                                        stats.speaking_events.fetch_add(1, Ordering::Relaxed);
                                    }
                                }
                                Some(Ok(Message::Ping(p))) => { let _ = ws.send(Message::Pong(p)).await; }
                                Some(Ok(_)) => {}
                                _ => return,
                            }
                        }
                    }
                }
            }));
        }
        // Speaker: pps packets/s for the whole phase.
        if is_speaker {
            let udp = udp.clone();
            let key = keys.clone();
            let addr = media_addr;
            let hash = channel_id_hash(&ch);
            let stats = stats.clone();
            let payload_len = args.payload.max(9);
            let period = Duration::from_micros(1_000_000 / args.pps as u64);
            tasks.push(tokio::spawn(async move {
                let mut ticker = tokio::time::interval(period);
                ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Burst);
                let mut body = vec![0xFCu8; payload_len];
                for seq in 1..=frames_per_speaker as u32 {
                    ticker.tick().await;
                    body[1..9]
                        .copy_from_slice(&chrono::Utc::now().timestamp_micros().to_be_bytes());
                    let pkt = AurixPacket::audio(
                        seq,
                        seq * 960,
                        ssrc,
                        hash,
                        Bytes::copy_from_slice(&body),
                    );
                    if udp.send_to(&pkt.seal(&key), addr).await.is_ok() {
                        stats.packets_sent.fetch_add(1, Ordering::Relaxed);
                    }
                }
            }));
        }
    }
    tracing::info!(
        "streaming {} s: {} speakers x {} pps, expecting ~{} deliveries",
        args.duration,
        speaker_slot
            .values()
            .map(|s| (*s).min(args.speakers))
            .sum::<usize>(),
        args.pps,
        expected_rx
    );
    tokio::time::sleep(stream_for + Duration::from_secs(2)).await;
    stop.notify_waiters();
    for t in tasks {
        let _ = tokio::time::timeout(Duration::from_secs(5), t).await;
    }
    let metrics_after = scrape(http.clone(), args.metrics.clone()).await;

    // 5. Report.
    let sent = stats.packets_sent.load(Ordering::Relaxed);
    let received = stats.packets_received.load(Ordering::Relaxed);
    let delivery = if expected_rx > 0 {
        received as f64 / expected_rx as f64
    } else {
        0.0
    };
    let lat_count: u64 = stats
        .latency_hist
        .iter()
        .map(|b| b.load(Ordering::Relaxed))
        .sum();
    let server = match (&metrics_before, &metrics_after) {
        (Some(b), Some(a)) => serde_json::json!({
            "packets_received_delta": scrape_counter(a, "aurix_packets_received_total") - scrape_counter(b, "aurix_packets_received_total"),
            "packets_sent_delta": scrape_counter(a, "aurix_packets_sent_total") - scrape_counter(b, "aurix_packets_sent_total"),
            "packets_dropped_delta": scrape_counter(a, "aurix_packets_dropped_total") - scrape_counter(b, "aurix_packets_dropped_total"),
            "active_sessions_peak_sample": scrape_counter(a, "aurix_active_sessions"),
            "process_cpu_seconds_delta": scrape_counter(a, "process_cpu_seconds_total") - scrape_counter(b, "process_cpu_seconds_total"),
            "process_resident_memory_bytes": scrape_counter(a, "process_resident_memory_bytes"),
        }),
        _ => serde_json::Value::Null,
    };
    let report = serde_json::json!({
        "run_id": run_id,
        "config": {
            "sessions": args.sessions, "channels": args.channels, "speakers_per_channel": args.speakers,
            "duration_s": args.duration, "pps": args.pps, "payload_bytes": args.payload,
        },
        "setup": {
            "channels_ms": channels_ms, "tokens_ms": tokens_ms, "sessions_ms": setup_ms,
            "sessions_ok": ok, "sessions_failed": stats.sessions_failed.load(Ordering::Relaxed),
            "session_setup_avg_ms": stats.setup_ms_total.load(Ordering::Relaxed).checked_div(ok).unwrap_or(0),
            "session_setup_max_ms": stats.setup_ms_max.load(Ordering::Relaxed),
        },
        "media": {
            "packets_sent": sent, "deliveries_expected": expected_rx, "deliveries_received": received,
            "delivery_ratio": delivery, "bad_auth": stats.packets_bad_auth.load(Ordering::Relaxed),
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
        },
        "server": server,
        "loadgen_rss_kib": rss_kib(),
    });
    if args.json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        println!("== Aurix load test {run_id} ==");
        println!(
            "sessions: {ok} ok / {} failed, setup {} ms total (avg {} ms, max {} ms per session)",
            report["setup"]["sessions_failed"],
            setup_ms,
            report["setup"]["session_setup_avg_ms"],
            report["setup"]["session_setup_max_ms"]
        );
        println!(
            "media: sent {sent} ({:.0} pps), delivered {received}/{expected_rx} ({:.2}%), bad auth {}",
            report["media"]["sent_pps"].as_f64().unwrap_or(0.0),
            delivery * 100.0,
            report["media"]["bad_auth"]
        );
        println!(
            "latency one-way ms: avg {:.2} p50 {:.1} p95 {:.1} p99 {:.1} max {:.1} ({} samples)",
            report["media"]["latency_ms"]["avg"].as_f64().unwrap_or(0.0),
            report["media"]["latency_ms"]["p50"].as_f64().unwrap_or(0.0),
            report["media"]["latency_ms"]["p95"].as_f64().unwrap_or(0.0),
            report["media"]["latency_ms"]["p99"].as_f64().unwrap_or(0.0),
            report["media"]["latency_ms"]["max"].as_f64().unwrap_or(0.0),
            lat_count
        );
        println!(
            "control: {} ws messages, {} speaking events",
            report["control"]["ws_messages"], report["control"]["speaking_events"]
        );
        if !server.is_null() {
            println!("server: {server}");
        }
    }
    Ok(())
}
