//! Aurix soak harness.
//!
//! Keeps a fleet of native bots (the real `aurix-client` core: WebSocket control plane, AURX
//! media with automatic path fallback, reconnect/resume/failover) talking in a few channels for
//! hours while operator-supplied chaos hooks (node kill, Redis failover, PostgreSQL restart, UDP
//! blackhole, …) run on a schedule. Every `--interval` it snapshots what the bots hear and lose,
//! scrapes the nodes' Prometheus endpoints, appends a JSONL row to `--report`, and at the end
//! judges the run:
//!
//! * steady state (outside chaos windows, after warm-up): every bot bound and joined, hearing
//!   its channel ≥ `--min-hearing` of the time, loss ≤ `--max-loss`, no hard reconnects;
//! * after each chaos step: every bot healthy again within `--recover-within`;
//! * no leaks: node RSS / open fds and the harness's own RSS do not grow past
//!   `--max-rss-growth` between the first and last quarter of steady intervals, and no
//!   sessions/participants remain on the nodes once the bots have left.
//!
//! The exit status is non-zero when any budget is exceeded, so the same binary drives the
//! nightly CI job and a 24–72 h run on operator hardware (`tools/soak/run.sh`).

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, bail, Context, Result};
use aurix_client::audio::{FRAME_SAMPLES, SAMPLE_RATE};
use aurix_client::{ChannelId, Client, ClientConfig, ConnectionState, DspConfig, Event, MediaPath};
use clap::Parser;
use parking_lot::Mutex;
use serde::Serialize;

#[derive(Parser, Debug, Clone)]
#[command(
    name = "aurix-soak",
    about = "Long-running multi-node soak with chaos hooks"
)]
struct Args {
    /// Node REST/WS base URLs, one pair per node (`--api URL --ws URL`, repeatable in order).
    #[arg(long, required = true)]
    api: Vec<String>,
    #[arg(long, required = true)]
    ws: Vec<String>,
    /// Application API key used to create channels and mint player tokens (first node).
    #[arg(long, env = "AURIX_SOAK_API_KEY")]
    api_key: String,
    /// Prometheus `/metrics` URL per node (same order as `--api`); optional.
    #[arg(long)]
    metrics: Vec<String>,
    /// Total run time (`90m`, `24h`, `1h30m`).
    #[arg(long, default_value = "1h", value_parser = parse_duration)]
    duration: Duration,
    /// Reporting/assertion interval.
    #[arg(long, default_value = "60s", value_parser = parse_duration)]
    interval: Duration,
    /// Intervals starting before this offset are not judged (JIT, buffers, first cascade).
    #[arg(long, default_value = "3m", value_parser = parse_duration)]
    warmup: Duration,
    /// Number of bots; spread round-robin over the nodes and the channels.
    #[arg(long, default_value_t = 16)]
    clients: usize,
    #[arg(long, default_value_t = 4)]
    channels: usize,
    /// Speakers per channel (the rest listen). Every bot must be able to hear someone, so
    /// this is at least 2 when a channel holds 2+ bots.
    #[arg(long, default_value_t = 2)]
    speakers: usize,
    /// `name=command` run through `sh -c`, in rotation, every `--chaos-every` after
    /// `--chaos-first-after`. Repeatable; omit for a pure endurance run.
    #[arg(long = "chaos")]
    chaos: Vec<String>,
    #[arg(long, default_value = "15m", value_parser = parse_duration)]
    chaos_every: Duration,
    #[arg(long, default_value = "5m", value_parser = parse_duration)]
    chaos_first_after: Duration,
    /// Intervals overlapping a chaos step plus this settle time are excluded from steady-state
    /// assertions.
    #[arg(long, default_value = "2m", value_parser = parse_duration)]
    chaos_settle: Duration,
    /// Every bot must be bound, joined and hearing again this long after a chaos step ends.
    #[arg(long, default_value = "120s", value_parser = parse_duration)]
    recover_within: Duration,
    /// Re-mint each bot's JWT this often (`Client::set_token`) so token expiry is exercised
    /// the way a game backend would.
    #[arg(long, default_value = "10m", value_parser = parse_duration)]
    token_refresh: Duration,
    /// Minimum fraction of 200 ms windows in a steady interval during which a bot heard audio
    /// (a window counts as heard when any of its 20 ms ticks mixed non-silent output).
    #[arg(long, default_value_t = 0.97)]
    min_hearing: f64,
    /// Maximum per-bot frame loss (%) in a steady interval.
    #[arg(long, default_value_t = 2.0)]
    max_loss: f32,
    /// Allowed growth of node RSS / fds / harness RSS between the first and last quarter of the
    /// steady intervals (0.25 = +25 %).
    #[arg(long, default_value_t = 0.25)]
    max_rss_growth: f64,
    /// JSONL: one row per interval plus chaos/violation rows.
    #[arg(long, default_value = "soak-report.jsonl")]
    report: PathBuf,
    /// Final verdict as JSON (also printed).
    #[arg(long)]
    summary: Option<PathBuf>,
}

fn parse_duration(s: &str) -> Result<Duration, String> {
    let s = s.trim();
    if s.is_empty() {
        return Err("empty duration".into());
    }
    let mut total = 0f64;
    let mut num = String::new();
    for c in s.chars() {
        if c.is_ascii_digit() || c == '.' {
            num.push(c);
            continue;
        }
        let n: f64 = num
            .parse()
            .map_err(|_| format!("bad number in duration {s:?}"))?;
        num.clear();
        total += match c {
            's' => n,
            'm' => n * 60.0,
            'h' => n * 3600.0,
            'd' => n * 86_400.0,
            _ => return Err(format!("unknown unit {c:?} in duration {s:?}")),
        };
    }
    if !num.is_empty() {
        total += num
            .parse::<f64>()
            .map_err(|_| format!("bad number in duration {s:?}"))?;
    }
    if total <= 0.0 {
        return Err("duration must be positive".into());
    }
    Ok(Duration::from_secs_f64(total))
}

fn now_unix() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64()
}

// ---------------------------------------------------------------------------------------------
// Bots

const HEARING_WINDOW_TICKS: u32 = 10;

/// Counters a bot bumps from its tick loop; the reporter takes and resets them per interval.
#[derive(Default)]
struct Counters {
    windows: AtomicU64,
    heard_windows: AtomicU64,
    recovering: AtomicU64,
    recovered: AtomicU64,
    recovery_ms_max: AtomicU64,
    resumed: AtomicU64,
    migrated: AtomicU64,
    failovers: AtomicU64,
    hard_reconnects: AtomicU64,
    request_failures: AtomicU64,
    server_errors: AtomicU64,
    path_changes: AtomicU64,
    token_refreshes: AtomicU64,
}

impl Counters {
    fn take(&self, c: &AtomicU64) -> u64 {
        c.swap(0, Ordering::AcqRel)
    }
}

/// Snapshot of the client's own cumulative statistics, sampled by the bot after each tick.
#[derive(Clone, Default)]
struct Gauges {
    state: Option<ConnectionState>,
    path: Option<MediaPath>,
    joined: bool,
    frames_received: u64,
    frames_lost: u64,
    frames_late: u64,
    underruns: u64,
    packets_received: u64,
    replayed: u64,
    bad_auth: u64,
    heartbeats_lost: u64,
    rtt_ms: f32,
    jitter_ms: f32,
    mos: f32,
    last_heard: Option<Instant>,
    endpoint: String,
}

struct Bot {
    index: usize,
    node: usize,
    channel: ChannelId,
    speaker: bool,
    /// Another bot speaks in this channel, so silence is a failure.
    expects_audio: bool,
    counters: Counters,
    gauges: Mutex<Gauges>,
    log: Mutex<Vec<String>>,
}

impl Bot {
    fn healthy(&self) -> bool {
        let g = self.gauges.lock();
        let bound = g.state == Some(ConnectionState::MediaBound);
        let hearing = !self.expects_audio
            || g.last_heard
                .is_some_and(|t| t.elapsed() < Duration::from_millis(1500));
        bound && g.joined && hearing
    }
}

struct Shared {
    args: Args,
    http: reqwest::Client,
    bots: Vec<Arc<Bot>>,
    stop: AtomicBool,
    run_id: String,
}

async fn post_json(
    shared: &Shared,
    path: &str,
    body: serde_json::Value,
) -> Result<serde_json::Value> {
    let url = format!("{}{}", shared.args.api[0], path);
    let resp = shared
        .http
        .post(&url)
        .header("x-api-key", &shared.args.api_key)
        .json(&body)
        .send()
        .await
        .with_context(|| format!("POST {url}"))?;
    let status = resp.status();
    let text = resp.text().await?;
    if !status.is_success() {
        bail!("POST {url} -> {status}: {text}");
    }
    serde_json::from_str(&text).with_context(|| format!("POST {url}: non-JSON body"))
}

async fn mint_token(shared: &Shared, bot: &Bot) -> Result<String> {
    let r = post_json(
        shared,
        "/v1/tokens",
        serde_json::json!({
            "external_id": format!("soak-{}-{}", shared.run_id, bot.index),
            "display_name": format!("soak bot {}", bot.index),
            "channels": [{
                "channel_id": bot.channel,
                "join": true,
                "speak": bot.speaker,
                "receive": true,
                "moderate": false,
            }],
        }),
    )
    .await?;
    r["token"]
        .as_str()
        .map(str::to_owned)
        .ok_or_else(|| anyhow!("token response without token"))
}

async fn mint_token_retrying(shared: &Shared, bot: &Bot) -> Option<String> {
    let mut delay = Duration::from_millis(500);
    while !shared.stop.load(Ordering::Acquire) {
        match mint_token(shared, bot).await {
            Ok(t) => return Some(t),
            Err(e) => {
                bot.log
                    .lock()
                    .push(format!("{:.0} token mint failed: {e:#}", now_unix()));
                tokio::time::sleep(delay).await;
                delay = (delay * 2).min(Duration::from_secs(10));
            }
        }
    }
    None
}

fn ws_url(base: &str) -> String {
    let base = base.trim_end_matches('/');
    if base.ends_with("/ws") {
        base.to_string()
    } else {
        format!("{base}/ws")
    }
}

async fn run_bot(shared: Arc<Shared>, bot: Arc<Bot>) {
    let Some(token) = mint_token_retrying(&shared, &bot).await else {
        return;
    };
    let mut cfg = ClientConfig::new(ws_url(&shared.args.ws[bot.node]), token);
    cfg.reconnect.max_attempts = u32::MAX;
    cfg.reconnect.max_delay = Duration::from_secs(5);
    cfg.dsp = DspConfig::BYPASS;
    let client = match Client::new(cfg) {
        Ok(c) => c,
        Err(e) => {
            bot.log.lock().push(format!("client init failed: {e}"));
            return;
        }
    };
    if let Err(e) = client.connect() {
        bot.log.lock().push(format!("connect failed: {e}"));
    }

    let mut phase = 0f32;
    let step =
        2.0 * std::f32::consts::PI * (330.0 + 55.0 * (bot.index % 6) as f32) / SAMPLE_RATE as f32;
    let mut pcm = vec![0f32; FRAME_SAMPLES];
    let mut out = vec![0f32; FRAME_SAMPLES];
    let mut ticker = tokio::time::interval(Duration::from_millis(20));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut tick = 0u64;
    let mut window_ticks = 0u32;
    let mut window_heard = false;
    let mut recovering_since: Option<Instant> = None;
    let mut next_token_refresh = Instant::now() + shared.args.token_refresh;
    let mut reconnect_at: Option<Instant> = None;
    let mut pending_reconnect: Option<tokio::task::JoinHandle<Option<String>>> = None;
    let mut pending_refresh: Option<tokio::task::JoinHandle<Result<String>>> = None;
    let mut joined = false;

    while !shared.stop.load(Ordering::Acquire) {
        ticker.tick().await;

        while let Some(ev) = client.poll_event() {
            match &ev {
                Event::SessionReady(info) => {
                    bot.gauges.lock().endpoint = info.endpoint.clone();
                    if !joined {
                        if let Err(e) = client.join_channel(bot.channel, None) {
                            bot.log
                                .lock()
                                .push(format!("{:.0} join failed: {e}", now_unix()));
                        }
                    }
                }
                Event::ChannelJoined { channel_id, .. } if *channel_id == bot.channel => {
                    joined = true;
                }
                Event::ChannelLeft { channel_id } if *channel_id == bot.channel => {
                    joined = false;
                }
                Event::MediaPathChanged { .. } => {
                    bot.counters.path_changes.fetch_add(1, Ordering::Relaxed);
                }
                Event::Recovering { cause, attempt, .. } => {
                    bot.counters.recovering.fetch_add(1, Ordering::Relaxed);
                    if *attempt == 1 {
                        recovering_since = Some(Instant::now());
                        bot.log
                            .lock()
                            .push(format!("{:.0} recovering: {cause}", now_unix()));
                    }
                }
                Event::Recovered { resumed, migrated } => {
                    bot.counters.recovered.fetch_add(1, Ordering::Relaxed);
                    if let Some(since) = recovering_since.take() {
                        let ms = since.elapsed().as_millis() as u64;
                        bot.counters
                            .recovery_ms_max
                            .fetch_max(ms, Ordering::Relaxed);
                        bot.log.lock().push(format!(
                            "{:.0} recovered in {ms} ms (resumed={resumed}, migrated={migrated})",
                            now_unix()
                        ));
                    }
                    if *resumed {
                        bot.counters.resumed.fetch_add(1, Ordering::Relaxed);
                    }
                    if *migrated {
                        bot.counters.migrated.fetch_add(1, Ordering::Relaxed);
                    }
                }
                Event::EndpointChanged { url } => {
                    bot.counters.failovers.fetch_add(1, Ordering::Relaxed);
                    bot.log
                        .lock()
                        .push(format!("{:.0} failover -> {url}", now_unix()));
                }
                Event::RequestFailed { .. } | Event::RejoinFailed { .. } => {
                    bot.counters
                        .request_failures
                        .fetch_add(1, Ordering::Relaxed);
                    bot.log.lock().push(format!("{:.0} {ev:?}", now_unix()));
                }
                Event::ServerError { code, message } => {
                    bot.counters.server_errors.fetch_add(1, Ordering::Relaxed);
                    bot.log
                        .lock()
                        .push(format!("{:.0} server error {code}: {message}", now_unix()));
                }
                Event::FailedToRecover { reason } | Event::Disconnected { reason } => {
                    joined = false;
                    bot.log
                        .lock()
                        .push(format!("{:.0} connection ended: {reason}", now_unix()));
                    if reconnect_at.is_none() {
                        reconnect_at = Some(Instant::now() + Duration::from_secs(1));
                    }
                }
                _ => {}
            }
        }

        // Token minting goes through the REST API, which may stall while PostgreSQL is being
        // restarted; it runs off-loop so a speaker keeps its 20 ms cadence meanwhile.
        if let Some(at) = reconnect_at {
            if Instant::now() >= at
                && pending_reconnect.is_none()
                && matches!(
                    client.state(),
                    ConnectionState::Disconnected | ConnectionState::Failed
                )
            {
                reconnect_at = None;
                let (s, b) = (shared.clone(), bot.clone());
                pending_reconnect = Some(tokio::spawn(
                    async move { mint_token_retrying(&s, &b).await },
                ));
            }
        }
        if pending_reconnect.as_ref().is_some_and(|h| h.is_finished()) {
            let handle = pending_reconnect.take().expect("checked above");
            if let Ok(Some(token)) = handle.await {
                let _ = client.set_token(&token);
                bot.counters.hard_reconnects.fetch_add(1, Ordering::Relaxed);
                if let Err(e) = client.connect() {
                    bot.log
                        .lock()
                        .push(format!("{:.0} reconnect failed: {e}", now_unix()));
                    reconnect_at = Some(Instant::now() + Duration::from_secs(2));
                }
            }
        }

        if Instant::now() >= next_token_refresh && pending_refresh.is_none() {
            next_token_refresh = Instant::now() + shared.args.token_refresh;
            let (s, b) = (shared.clone(), bot.clone());
            pending_refresh = Some(tokio::spawn(async move { mint_token(&s, &b).await }));
        }
        if pending_refresh.as_ref().is_some_and(|h| h.is_finished()) {
            let handle = pending_refresh.take().expect("checked above");
            match handle.await {
                Ok(Ok(token)) => {
                    if client.set_token(&token).is_ok() {
                        bot.counters.token_refreshes.fetch_add(1, Ordering::Relaxed);
                    }
                }
                Ok(Err(e)) => {
                    bot.log
                        .lock()
                        .push(format!("{:.0} token refresh failed: {e:#}", now_unix()));
                    next_token_refresh = Instant::now() + Duration::from_secs(5);
                }
                Err(_) => {}
            }
        }

        if bot.speaker && client.state() == ConnectionState::MediaBound {
            for s in pcm.iter_mut() {
                *s = 0.3 * phase.sin();
                phase += step;
                if phase > 2.0 * std::f32::consts::PI {
                    phase -= 2.0 * std::f32::consts::PI;
                }
            }
            client.push_capture_f32(&pcm, SAMPLE_RATE, 1);
        }

        let n = client.mix_output_f32(&mut out, 1);
        let heard = n > 0 && out[..n].iter().any(|s| s.abs() > 0.01);
        window_ticks += 1;
        window_heard |= heard;
        if window_ticks == HEARING_WINDOW_TICKS {
            bot.counters.windows.fetch_add(1, Ordering::Relaxed);
            if window_heard {
                bot.counters.heard_windows.fetch_add(1, Ordering::Relaxed);
            }
            window_ticks = 0;
            window_heard = false;
        }
        tick += 1;

        if tick.is_multiple_of(25) || heard {
            let stats = client.stats();
            let mut g = bot.gauges.lock();
            g.state = Some(client.state());
            g.path = stats.media_path;
            g.joined = joined;
            g.frames_received = stats.media.audio_frames_received;
            g.frames_lost = stats.frames_lost;
            g.frames_late = stats.frames_late;
            g.underruns = stats.underruns;
            g.packets_received = stats.media.packets_received;
            g.replayed = stats.media.replayed;
            g.bad_auth = stats.media.bad_auth;
            g.heartbeats_lost = stats.media.heartbeats_lost;
            g.rtt_ms = stats.media.rtt_ms;
            g.jitter_ms = stats.jitter_ms;
            g.mos = stats.mos;
            if heard {
                g.last_heard = Some(Instant::now());
            }
        }
    }
    client.disconnect();
}

// ---------------------------------------------------------------------------------------------
// Node metrics

#[derive(Clone, Debug, Default, Serialize)]
struct NodeSample {
    up: bool,
    rss_bytes: Option<f64>,
    open_fds: Option<f64>,
    cpu_seconds: Option<f64>,
    active_sessions: Option<f64>,
    active_channels: Option<f64>,
    active_participants: Option<f64>,
    packets_dropped_total: Option<f64>,
    cascade_links: BTreeMap<String, f64>,
}

fn metric_value(body: &str, name: &str) -> Option<f64> {
    let mut sum = None;
    for line in body.lines() {
        if !line.starts_with(name) {
            continue;
        }
        let rest = &line[name.len()..];
        let (labels_ok, tail) = if rest.starts_with('{') {
            match rest.find('}') {
                Some(i) => (true, &rest[i + 1..]),
                None => (false, rest),
            }
        } else if rest.starts_with(' ') {
            (true, rest)
        } else {
            (false, rest)
        };
        if !labels_ok {
            continue;
        }
        if let Some(v) = tail
            .split_whitespace()
            .next()
            .and_then(|v| v.parse::<f64>().ok())
        {
            *sum.get_or_insert(0.0) += v;
        }
    }
    sum
}

fn metric_labelled(body: &str, name: &str, label: &str) -> BTreeMap<String, f64> {
    let mut out = BTreeMap::new();
    for line in body.lines() {
        let Some(rest) = line.strip_prefix(name) else {
            continue;
        };
        let Some(rest) = rest.strip_prefix('{') else {
            continue;
        };
        let Some(end) = rest.find('}') else {
            continue;
        };
        let labels = &rest[..end];
        let value = rest[end + 1..]
            .split_whitespace()
            .next()
            .and_then(|v| v.parse::<f64>().ok());
        let key = labels.split(',').find_map(|kv| {
            let (k, v) = kv.split_once('=')?;
            (k.trim() == label).then(|| v.trim().trim_matches('"').to_string())
        });
        if let (Some(k), Some(v)) = (key, value) {
            *out.entry(k).or_insert(0.0) += v;
        }
    }
    out
}

async fn scrape(http: &reqwest::Client, url: &str) -> NodeSample {
    let body = match http
        .get(url)
        .timeout(Duration::from_secs(5))
        .send()
        .await
        .and_then(|r| r.error_for_status())
    {
        Ok(r) => match r.text().await {
            Ok(b) => b,
            Err(_) => return NodeSample::default(),
        },
        Err(_) => return NodeSample::default(),
    };
    NodeSample {
        up: true,
        rss_bytes: metric_value(&body, "process_resident_memory_bytes"),
        open_fds: metric_value(&body, "process_open_fds"),
        cpu_seconds: metric_value(&body, "process_cpu_seconds_total"),
        active_sessions: metric_value(&body, "aurix_active_sessions"),
        active_channels: metric_value(&body, "aurix_active_channels"),
        active_participants: metric_value(&body, "aurix_active_participants"),
        packets_dropped_total: metric_value(&body, "aurix_packets_dropped_total"),
        cascade_links: metric_labelled(&body, "aurix_cascade_links", "transport"),
    }
}

fn self_open_fds() -> Option<f64> {
    std::fs::read_dir("/proc/self/fd")
        .ok()
        .map(|d| d.count() as f64)
}

fn self_rss_bytes() -> Option<f64> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    let line = status.lines().find(|l| l.starts_with("VmRSS:"))?;
    let kib: f64 = line.split_whitespace().nth(1)?.parse().ok()?;
    Some(kib * 1024.0)
}

// ---------------------------------------------------------------------------------------------
// Reporting

#[derive(Clone, Debug, Serialize)]
struct BotRow {
    index: usize,
    node: usize,
    speaker: bool,
    state: Option<String>,
    path: Option<String>,
    joined: bool,
    endpoint: String,
    hearing: f64,
    frames_received: u64,
    frames_lost: u64,
    frames_late: u64,
    underruns: u64,
    loss_percent: f32,
    replayed: u64,
    bad_auth: u64,
    heartbeats_lost: u64,
    rtt_ms: f32,
    jitter_ms: f32,
    mos: f32,
    recovering: u64,
    recovered: u64,
    recovery_ms_max: u64,
    resumed: u64,
    migrated: u64,
    failovers: u64,
    hard_reconnects: u64,
    request_failures: u64,
    server_errors: u64,
    path_changes: u64,
    token_refreshes: u64,
}

#[derive(Clone, Debug, Serialize)]
struct IntervalRow {
    kind: &'static str,
    at: f64,
    elapsed_secs: f64,
    steady: bool,
    bots_bound: usize,
    bots_joined: usize,
    bots_hearing_ok: usize,
    hearing_min: f64,
    hearing_mean: f64,
    loss_max_percent: f32,
    rtt_max_ms: f32,
    mos_min: f32,
    frames_lost: u64,
    underruns: u64,
    replayed: u64,
    reconnects: u64,
    recovery_ms_max: u64,
    failovers: u64,
    hard_reconnects: u64,
    request_failures: u64,
    server_errors: u64,
    paths: BTreeMap<String, usize>,
    nodes: Vec<NodeSample>,
    self_rss_bytes: Option<f64>,
    self_open_fds: Option<f64>,
    bots: Vec<BotRow>,
    violations: Vec<String>,
}

#[derive(Clone, Debug, Serialize)]
struct ChaosRow {
    kind: &'static str,
    name: String,
    started_at: f64,
    ended_at: f64,
    exit_ok: bool,
    output_tail: String,
    recovered_in_secs: Option<f64>,
    recovered: bool,
    unhealthy_after: Vec<usize>,
}

#[derive(Debug, Default, Serialize)]
struct Summary {
    ok: bool,
    run_id: String,
    duration_secs: f64,
    intervals: usize,
    steady_intervals: usize,
    chaos_steps: usize,
    chaos_recovered: usize,
    chaos_recovery_secs_max: Option<f64>,
    reconnects: u64,
    recovery_ms_max: u64,
    failovers: u64,
    hard_reconnects: u64,
    frames_lost: u64,
    hearing_min_steady: Option<f64>,
    loss_max_steady_percent: Option<f32>,
    node_rss_growth: Vec<Option<f64>>,
    node_fds_growth: Vec<Option<f64>>,
    self_rss_growth: Option<f64>,
    self_fds_growth: Option<f64>,
    leftover_sessions: Option<f64>,
    leftover_participants: Option<f64>,
    violations: Vec<String>,
}

struct ChaosWindow {
    start: Instant,
    end: Option<Instant>,
}

struct State {
    windows: Vec<ChaosWindow>,
    intervals: Vec<IntervalRow>,
    chaos: Vec<ChaosRow>,
    violations: Vec<String>,
}

fn write_row(path: &PathBuf, row: &impl Serialize) {
    use std::io::Write;
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        let _ = serde_json::to_writer(&mut f, row);
        let _ = f.write_all(b"\n");
    }
}

fn snapshot(shared: &Shared, prev: &mut [Gauges]) -> Vec<BotRow> {
    let mut rows = Vec::with_capacity(shared.bots.len());
    for (bot, last) in shared.bots.iter().zip(prev.iter_mut()) {
        let c = &bot.counters;
        let windows = c.take(&c.windows);
        let heard = c.take(&c.heard_windows);
        let g = bot.gauges.lock().clone();
        let received = g.frames_received.saturating_sub(last.frames_received);
        let lost = g.frames_lost.saturating_sub(last.frames_lost);
        let denom = received + lost;
        rows.push(BotRow {
            index: bot.index,
            node: bot.node,
            speaker: bot.speaker,
            state: g.state.map(|s| format!("{s:?}")),
            path: g.path.map(|p| format!("{p:?}")),
            joined: g.joined,
            endpoint: g.endpoint.clone(),
            hearing: if windows == 0 {
                0.0
            } else {
                heard as f64 / windows as f64
            },
            frames_received: received,
            frames_lost: lost,
            frames_late: g.frames_late.saturating_sub(last.frames_late),
            underruns: g.underruns.saturating_sub(last.underruns),
            loss_percent: if denom == 0 {
                0.0
            } else {
                100.0 * lost as f32 / denom as f32
            },
            replayed: g.replayed.saturating_sub(last.replayed),
            bad_auth: g.bad_auth.saturating_sub(last.bad_auth),
            heartbeats_lost: g.heartbeats_lost.saturating_sub(last.heartbeats_lost),
            rtt_ms: g.rtt_ms,
            jitter_ms: g.jitter_ms,
            mos: g.mos,
            recovering: c.take(&c.recovering),
            recovered: c.take(&c.recovered),
            recovery_ms_max: c.take(&c.recovery_ms_max),
            resumed: c.take(&c.resumed),
            migrated: c.take(&c.migrated),
            failovers: c.take(&c.failovers),
            hard_reconnects: c.take(&c.hard_reconnects),
            request_failures: c.take(&c.request_failures),
            server_errors: c.take(&c.server_errors),
            path_changes: c.take(&c.path_changes),
            token_refreshes: c.take(&c.token_refreshes),
        });
        *last = g;
    }
    rows
}

fn overlaps_chaos(windows: &[ChaosWindow], from: Instant, to: Instant, settle: Duration) -> bool {
    windows.iter().any(|w| {
        let end = w.end.map(|e| e + settle).unwrap_or(to);
        w.start <= to && end >= from
    })
}

async fn report_interval(
    shared: &Shared,
    state: &Mutex<State>,
    started: Instant,
    from: Instant,
    prev: &mut [Gauges],
) {
    let args = &shared.args;
    let bots = snapshot(shared, prev);
    let mut nodes = Vec::with_capacity(args.metrics.len());
    for url in &args.metrics {
        nodes.push(scrape(&shared.http, url).await);
    }
    let to = Instant::now();
    let steady = from.duration_since(started) >= args.warmup
        && !overlaps_chaos(&state.lock().windows, from, to, args.chaos_settle);

    let mut violations = Vec::new();
    let mut paths = BTreeMap::new();
    let mut hearing_min = 1.0f64;
    let mut hearing_sum = 0.0;
    let mut hearing_n = 0usize;
    let mut hearing_ok = 0usize;
    let mut bound = 0;
    let mut joined = 0;
    let mut loss_max = 0f32;
    let mut rtt_max = 0f32;
    let mut mos_min = 5f32;
    let (mut lost, mut underruns, mut replayed) = (0, 0, 0);
    let (mut reconnects, mut failovers, mut hard, mut req_fail, mut srv_err) = (0, 0, 0, 0, 0);
    let mut recovery_ms_max = 0u64;
    for (row, bot) in bots.iter().zip(&shared.bots) {
        if let Some(p) = &row.path {
            *paths.entry(p.clone()).or_insert(0) += 1;
        }
        let is_bound = row.state.as_deref() == Some("MediaBound");
        bound += usize::from(is_bound);
        joined += usize::from(row.joined);
        if bot.expects_audio {
            hearing_min = hearing_min.min(row.hearing);
            hearing_sum += row.hearing;
            hearing_n += 1;
            hearing_ok += usize::from(row.hearing >= args.min_hearing);
        }
        loss_max = loss_max.max(row.loss_percent);
        rtt_max = rtt_max.max(row.rtt_ms);
        if is_bound && row.mos > 0.0 {
            mos_min = mos_min.min(row.mos);
        }
        lost += row.frames_lost;
        underruns += row.underruns;
        replayed += row.replayed;
        reconnects += row.recovered;
        recovery_ms_max = recovery_ms_max.max(row.recovery_ms_max);
        failovers += row.failovers;
        hard += row.hard_reconnects;
        req_fail += row.request_failures;
        srv_err += row.server_errors;
        if steady {
            if !is_bound {
                violations.push(format!(
                    "bot {} not MediaBound ({:?})",
                    row.index, row.state
                ));
            }
            if !row.joined {
                violations.push(format!("bot {} not joined", row.index));
            }
            if bot.expects_audio && row.hearing < args.min_hearing {
                violations.push(format!(
                    "bot {} heard audio {:.1}% of the interval (< {:.1}%)",
                    row.index,
                    100.0 * row.hearing,
                    100.0 * args.min_hearing
                ));
            }
            if row.loss_percent > args.max_loss {
                violations.push(format!(
                    "bot {} lost {:.2}% frames (> {:.2}%)",
                    row.index, row.loss_percent, args.max_loss
                ));
            }
            if row.hard_reconnects > 0 {
                violations.push(format!(
                    "bot {} needed {} hard reconnect(s)",
                    row.index, row.hard_reconnects
                ));
            }
            if row.bad_auth > 0 {
                violations.push(format!(
                    "bot {} saw {} packet auth failures",
                    row.index, row.bad_auth
                ));
            }
        }
    }
    if steady {
        for (i, n) in nodes.iter().enumerate() {
            if !n.up {
                violations.push(format!("node {i} metrics endpoint down"));
            }
        }
    }
    let row = IntervalRow {
        kind: "interval",
        at: now_unix(),
        elapsed_secs: to.duration_since(started).as_secs_f64(),
        steady,
        bots_bound: bound,
        bots_joined: joined,
        bots_hearing_ok: hearing_ok,
        hearing_min: if hearing_n == 0 { 1.0 } else { hearing_min },
        hearing_mean: if hearing_n == 0 {
            1.0
        } else {
            hearing_sum / hearing_n as f64
        },
        loss_max_percent: loss_max,
        rtt_max_ms: rtt_max,
        mos_min: if mos_min > 4.9 { 0.0 } else { mos_min },
        frames_lost: lost,
        underruns,
        replayed,
        reconnects,
        recovery_ms_max,
        failovers,
        hard_reconnects: hard,
        request_failures: req_fail,
        server_errors: srv_err,
        paths,
        nodes,
        self_rss_bytes: self_rss_bytes(),
        self_open_fds: self_open_fds(),
        bots,
        violations: violations.clone(),
    };
    let rss: Vec<String> = row
        .nodes
        .iter()
        .map(|n| match n.rss_bytes {
            Some(b) if n.up => format!("{:.0}M", b / 1_048_576.0),
            _ => "down".into(),
        })
        .collect();
    println!(
        "[{:>7.0}s] {} bound {}/{} joined {}/{} hearing min {:.1}% mean {:.1}% loss max {:.2}% rtt max {:.1}ms reconnects {} (max {} ms) failovers {} hard {} paths {:?} node rss [{}] self rss {:.0}M fds {:.0}{}",
        row.elapsed_secs,
        if steady { "steady" } else { "chaos " },
        row.bots_bound,
        shared.bots.len(),
        row.bots_joined,
        shared.bots.len(),
        100.0 * row.hearing_min,
        100.0 * row.hearing_mean,
        row.loss_max_percent,
        row.rtt_max_ms,
        row.reconnects,
        row.recovery_ms_max,
        row.failovers,
        row.hard_reconnects,
        row.paths,
        rss.join(", "),
        row.self_rss_bytes.unwrap_or(0.0) / 1_048_576.0,
        row.self_open_fds.unwrap_or(0.0),
        if violations.is_empty() {
            String::new()
        } else {
            format!(" VIOLATIONS: {}", violations.join("; "))
        }
    );
    write_row(&args.report, &row);
    let mut st = state.lock();
    for v in &violations {
        st.violations
            .push(format!("[{:.0}s] {v}", row.elapsed_secs));
    }
    st.intervals.push(row);
}

// ---------------------------------------------------------------------------------------------
// Chaos

async fn run_chaos_step(
    shared: &Shared,
    state: &Mutex<State>,
    started: Instant,
    name: &str,
    command: &str,
) {
    let idx = {
        let mut st = state.lock();
        st.windows.push(ChaosWindow {
            start: Instant::now(),
            end: None,
        });
        st.windows.len() - 1
    };
    let started_at = now_unix();
    println!(
        "[{:>7.0}s] chaos `{name}` start: {command}",
        started.elapsed().as_secs_f64()
    );
    let output = tokio::process::Command::new("sh")
        .arg("-c")
        .arg(command)
        .stdin(std::process::Stdio::null())
        .output()
        .await;
    let (exit_ok, tail) = match output {
        Ok(o) => {
            let mut text = String::from_utf8_lossy(&o.stdout).into_owned();
            text.push_str(&String::from_utf8_lossy(&o.stderr));
            let tail: Vec<&str> = text.lines().rev().take(20).collect();
            (
                o.status.success(),
                tail.into_iter().rev().collect::<Vec<_>>().join("\n"),
            )
        }
        Err(e) => (false, format!("spawn failed: {e}")),
    };
    let ended = Instant::now();
    state.lock().windows[idx].end = Some(ended);
    let ended_at = now_unix();

    let deadline = ended + shared.args.recover_within;
    let mut recovered_in = None;
    loop {
        if shared.bots.iter().all(|b| b.healthy()) {
            recovered_in = Some(ended.elapsed().as_secs_f64());
            break;
        }
        if Instant::now() >= deadline || shared.stop.load(Ordering::Acquire) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    let unhealthy: Vec<usize> = shared
        .bots
        .iter()
        .filter(|b| !b.healthy())
        .map(|b| b.index)
        .collect();
    let recovered = recovered_in.is_some();
    println!(
        "[{:>7.0}s] chaos `{name}` done in {:.0}s (exit {}), {}",
        started.elapsed().as_secs_f64(),
        ended_at - started_at,
        if exit_ok { "ok" } else { "FAILED" },
        match recovered_in {
            Some(s) => format!("all bots healthy after {s:.1}s"),
            None => format!(
                "NOT recovered within {:?}: bots {unhealthy:?}",
                shared.args.recover_within
            ),
        }
    );
    let row = ChaosRow {
        kind: "chaos",
        name: name.to_string(),
        started_at,
        ended_at,
        exit_ok,
        output_tail: tail,
        recovered_in_secs: recovered_in,
        recovered,
        unhealthy_after: unhealthy.clone(),
    };
    write_row(&shared.args.report, &row);
    let mut st = state.lock();
    if !exit_ok {
        st.violations
            .push(format!("chaos `{name}` hook exited non-zero"));
    }
    if !recovered {
        st.violations.push(format!(
            "chaos `{name}`: bots {unhealthy:?} not healthy within {:?}",
            shared.args.recover_within
        ));
    }
    st.chaos.push(row);
}

async fn chaos_loop(shared: Arc<Shared>, state: Arc<Mutex<State>>, started: Instant) {
    let steps: Vec<(String, String)> = shared
        .args
        .chaos
        .iter()
        .map(|s| match s.split_once('=') {
            Some((n, c)) => (n.trim().to_string(), c.trim().to_string()),
            None => (s.clone(), s.clone()),
        })
        .collect();
    if steps.is_empty() {
        return;
    }
    let end = started + shared.args.duration;
    let mut next = started + shared.args.chaos_first_after;
    let mut i = 0usize;
    loop {
        let now = Instant::now();
        if next >= end - shared.args.recover_within - shared.args.chaos_settle {
            return;
        }
        if next > now {
            tokio::select! {
                _ = tokio::time::sleep(next - now) => {}
                _ = wait_stop(&shared) => return,
            }
        }
        if shared.stop.load(Ordering::Acquire) {
            return;
        }
        let (name, cmd) = &steps[i % steps.len()];
        run_chaos_step(&shared, &state, started, name, cmd).await;
        i += 1;
        next = Instant::now() + shared.args.chaos_every;
    }
}

async fn wait_stop(shared: &Shared) {
    while !shared.stop.load(Ordering::Acquire) {
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

// ---------------------------------------------------------------------------------------------
// Verdict

fn median(mut v: Vec<f64>) -> Option<f64> {
    if v.is_empty() {
        return None;
    }
    v.sort_by(|a, b| a.total_cmp(b));
    Some(v[v.len() / 2])
}

/// Relative growth between the first and last quarter of the steady samples, or `None` when
/// there are too few samples to say anything.
fn growth(samples: &[f64]) -> Option<f64> {
    if samples.len() < 8 {
        return None;
    }
    let q = samples.len() / 4;
    let first = median(samples[..q].to_vec())?;
    let last = median(samples[samples.len() - q..].to_vec())?;
    if first <= 0.0 {
        return None;
    }
    Some(last / first - 1.0)
}

/// Steady intervals the resource (RSS / fd) growth is judged on. The first hook exercises code
/// paths that allocate lazily and then stay resident — TLS/QUIC client state, the failover
/// endpoint, a second node's session tables — so the baseline starts after it has ended. A run
/// too short to leave `growth` enough samples after that point reports no growth figure rather
/// than a spurious one.
fn resource_samples<'a>(steady: &[&'a IntervalRow], chaos: &[ChaosRow]) -> Vec<&'a IntervalRow> {
    let Some(first_hook_end) = chaos.iter().map(|c| c.ended_at).reduce(f64::min) else {
        return steady.to_vec();
    };
    steady
        .iter()
        .copied()
        .filter(|r| r.at >= first_hook_end)
        .collect()
}

fn verdict(
    shared: &Shared,
    state: &State,
    started: Instant,
    leftovers: Option<(f64, f64)>,
) -> Summary {
    let args = &shared.args;
    let mut violations = state.violations.clone();
    let steady: Vec<&IntervalRow> = state.intervals.iter().filter(|r| r.steady).collect();
    let resource = resource_samples(&steady, &state.chaos);
    let node_count = args.metrics.len();
    let mut node_rss_growth = Vec::with_capacity(node_count);
    let mut node_fds_growth = Vec::with_capacity(node_count);
    for i in 0..node_count {
        let rss: Vec<f64> = resource
            .iter()
            .filter_map(|r| r.nodes.get(i).filter(|n| n.up).and_then(|n| n.rss_bytes))
            .collect();
        let fds: Vec<f64> = resource
            .iter()
            .filter_map(|r| r.nodes.get(i).filter(|n| n.up).and_then(|n| n.open_fds))
            .collect();
        let g_rss = growth(&rss);
        let g_fds = growth(&fds);
        if let Some(g) = g_rss {
            if g > args.max_rss_growth {
                violations.push(format!(
                    "node {i} RSS grew {:.1}% between first and last quarter (> {:.0}%)",
                    100.0 * g,
                    100.0 * args.max_rss_growth
                ));
            }
        }
        if let Some(g) = g_fds {
            if g > args.max_rss_growth {
                violations.push(format!(
                    "node {i} open fds grew {:.1}% between first and last quarter (> {:.0}%)",
                    100.0 * g,
                    100.0 * args.max_rss_growth
                ));
            }
        }
        node_rss_growth.push(g_rss);
        node_fds_growth.push(g_fds);
    }
    let self_rss: Vec<f64> = resource.iter().filter_map(|r| r.self_rss_bytes).collect();
    let self_rss_growth = growth(&self_rss);
    let self_fds: Vec<f64> = resource.iter().filter_map(|r| r.self_open_fds).collect();
    let self_fds_growth = growth(&self_fds);
    if let Some(g) = self_fds_growth {
        if g > args.max_rss_growth {
            violations.push(format!(
                "harness open fds grew {:.1}% between first and last quarter (> {:.0}%)",
                100.0 * g,
                100.0 * args.max_rss_growth
            ));
        }
    }
    if let Some(g) = self_rss_growth {
        if g > args.max_rss_growth {
            violations.push(format!(
                "harness RSS grew {:.1}% between first and last quarter (> {:.0}%)",
                100.0 * g,
                100.0 * args.max_rss_growth
            ));
        }
    }
    if let Some((sessions, participants)) = leftovers {
        if sessions > 0.0 || participants > 0.0 {
            violations.push(format!(
                "nodes still report {sessions} session(s) / {participants} participant(s) after every bot left"
            ));
        }
    }
    if !args.chaos.is_empty() && state.chaos.is_empty() {
        violations.push("no chaos step ran (duration too short for --chaos-first-after)".into());
    }
    if steady.is_empty() {
        violations.push("no steady interval was judged (duration too short for --warmup)".into());
    }
    let recovery: Vec<f64> = state
        .chaos
        .iter()
        .filter_map(|c| c.recovered_in_secs)
        .collect();
    Summary {
        ok: violations.is_empty(),
        run_id: shared.run_id.clone(),
        duration_secs: started.elapsed().as_secs_f64(),
        intervals: state.intervals.len(),
        steady_intervals: steady.len(),
        chaos_steps: state.chaos.len(),
        chaos_recovered: state.chaos.iter().filter(|c| c.recovered).count(),
        chaos_recovery_secs_max: recovery
            .iter()
            .cloned()
            .fold(None, |m: Option<f64>, v| Some(m.map_or(v, |m| m.max(v)))),
        reconnects: state.intervals.iter().map(|r| r.reconnects).sum(),
        recovery_ms_max: state
            .intervals
            .iter()
            .map(|r| r.recovery_ms_max)
            .max()
            .unwrap_or(0),
        failovers: state.intervals.iter().map(|r| r.failovers).sum(),
        hard_reconnects: state.intervals.iter().map(|r| r.hard_reconnects).sum(),
        frames_lost: state.intervals.iter().map(|r| r.frames_lost).sum(),
        hearing_min_steady: steady
            .iter()
            .map(|r| r.hearing_min)
            .fold(None, |m: Option<f64>, v| Some(m.map_or(v, |m| m.min(v)))),
        loss_max_steady_percent: steady
            .iter()
            .map(|r| r.loss_max_percent)
            .fold(None, |m: Option<f32>, v| Some(m.map_or(v, |m| m.max(v)))),
        node_rss_growth,
        node_fds_growth,
        self_rss_growth,
        self_fds_growth,
        leftover_sessions: leftovers.map(|l| l.0),
        leftover_participants: leftovers.map(|l| l.1),
        violations,
    }
}

// ---------------------------------------------------------------------------------------------

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "warn".into()),
        )
        .with_writer(std::io::stderr)
        .init();
    let args = Args::parse();
    if args.api.len() != args.ws.len() {
        bail!("--api and --ws must be given the same number of times");
    }
    if !args.metrics.is_empty() && args.metrics.len() != args.api.len() {
        bail!("--metrics must be given once per node or not at all");
    }
    if args.clients == 0 || args.channels == 0 {
        bail!("--clients and --channels must be positive");
    }
    let per_channel = args.clients.div_ceil(args.channels);
    if per_channel >= 2 && args.speakers < 2 {
        bail!("--speakers must be at least 2 so every bot hears someone");
    }
    if args.interval < Duration::from_secs(5) {
        bail!("--interval must be at least 5s");
    }

    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(15))
        .build()?;
    let run_id = uuid::Uuid::now_v7().simple().to_string()[..8].to_string();
    let mut shared = Shared {
        args: args.clone(),
        http,
        bots: Vec::new(),
        stop: AtomicBool::new(false),
        run_id: run_id.clone(),
    };

    let mut channels = Vec::with_capacity(args.channels);
    for i in 0..args.channels {
        let ch = post_json(
            &shared,
            "/v1/channels",
            serde_json::json!({ "name": format!("soak-{run_id}-{i}"), "config": {} }),
        )
        .await
        .context("creating soak channel")?;
        let id: uuid::Uuid = ch["id"]
            .as_str()
            .ok_or_else(|| anyhow!("channel without id"))?
            .parse()?;
        channels.push(ChannelId::from_uuid(id));
    }
    let nodes = args.api.len();
    let mut bots = Vec::with_capacity(args.clients);
    for i in 0..args.clients {
        let channel_index = i % args.channels;
        let seat = i / args.channels;
        let speaker = seat < args.speakers;
        let mates = (0..args.clients)
            .filter(|j| *j != i && j % args.channels == channel_index)
            .count();
        let other_speakers = (0..args.clients)
            .filter(|j| {
                *j != i && j % args.channels == channel_index && j / args.channels < args.speakers
            })
            .count();
        bots.push(Arc::new(Bot {
            index: i,
            node: i % nodes,
            channel: channels[channel_index],
            speaker,
            expects_audio: mates > 0 && other_speakers > 0,
            counters: Counters::default(),
            gauges: Mutex::new(Gauges::default()),
            log: Mutex::new(Vec::new()),
        }));
    }
    shared.bots = bots;
    let shared = Arc::new(shared);
    println!(
        "soak {run_id}: {} bots over {} node(s) in {} channel(s) ({} speaker(s) each) for {:?}; report -> {}",
        args.clients,
        nodes,
        args.channels,
        args.speakers,
        args.duration,
        args.report.display()
    );
    write_row(
        &args.report,
        &serde_json::json!({
            "kind": "start",
            "at": now_unix(),
            "run_id": run_id,
            "nodes": args.api.len(),
            "clients": args.clients,
            "channels": args.channels,
            "speakers": args.speakers,
            "duration_secs": args.duration.as_secs_f64(),
            "chaos": args.chaos.iter().map(|c| c.split_once('=').map_or(c.as_str(), |(n, _)| n)).collect::<Vec<_>>(),
        }),
    );

    let started = Instant::now();
    let state = Arc::new(Mutex::new(State {
        windows: Vec::new(),
        intervals: Vec::new(),
        chaos: Vec::new(),
        violations: Vec::new(),
    }));
    let mut bot_tasks = Vec::with_capacity(shared.bots.len());
    for bot in &shared.bots {
        bot_tasks.push(tokio::spawn(run_bot(shared.clone(), bot.clone())));
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let chaos_task = tokio::spawn(chaos_loop(shared.clone(), state.clone(), started));

    let mut prev = vec![Gauges::default(); shared.bots.len()];
    let mut from = Instant::now();
    let end = started + args.duration;
    let mut ctrl_c = std::pin::pin!(tokio::signal::ctrl_c());
    let mut interrupted = false;
    loop {
        let next = (from + args.interval).min(end);
        tokio::select! {
            _ = tokio::time::sleep_until(tokio::time::Instant::from_std(next)) => {}
            _ = &mut ctrl_c => {
                println!("interrupted; judging what ran so far");
                interrupted = true;
            }
        }
        report_interval(&shared, &state, started, from, &mut prev).await;
        from = Instant::now();
        if interrupted || from >= end {
            break;
        }
    }

    shared.stop.store(true, Ordering::Release);
    chaos_task.abort();
    for t in bot_tasks {
        let _ = tokio::time::timeout(Duration::from_secs(15), t).await;
    }
    let leftovers = if args.metrics.is_empty() {
        None
    } else {
        tokio::time::sleep(Duration::from_secs(5)).await;
        let (mut sessions, mut participants) = (0.0, 0.0);
        for url in &args.metrics {
            let s = scrape(&shared.http, url).await;
            sessions += s.active_sessions.unwrap_or(0.0);
            participants += s.active_participants.unwrap_or(0.0);
        }
        Some((sessions, participants))
    };

    let summary = verdict(&shared, &state.lock(), started, leftovers);
    write_row(
        &args.report,
        &serde_json::json!({ "kind": "summary", "at": now_unix(), "summary": &summary }),
    );
    let text = serde_json::to_string_pretty(&summary)?;
    if let Some(path) = &args.summary {
        std::fs::write(path, &text).with_context(|| format!("writing {}", path.display()))?;
    }
    println!("{text}");
    for bot in &shared.bots {
        let log = bot.log.lock();
        if !log.is_empty() {
            eprintln!("bot {} log:", bot.index);
            for l in log.iter().rev().take(40).rev() {
                eprintln!("  {l}");
            }
        }
    }
    if summary.ok {
        Ok(())
    } else {
        eprintln!("SOAK FAILED: {} violation(s)", summary.violations.len());
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durations_parse() {
        assert_eq!(parse_duration("90s").unwrap(), Duration::from_secs(90));
        assert_eq!(parse_duration("1h30m").unwrap(), Duration::from_secs(5400));
        assert_eq!(parse_duration("2d").unwrap(), Duration::from_secs(172_800));
        assert_eq!(parse_duration("45").unwrap(), Duration::from_secs(45));
        assert!(parse_duration("").is_err());
        assert!(parse_duration("5x").is_err());
    }

    #[test]
    fn metric_parsing_sums_and_splits_labels() {
        let body = "# HELP x\naurix_cascade_links{transport=\"udp\"} 2\naurix_cascade_links{transport=\"tcp\"} 1\nprocess_resident_memory_bytes 1024\nprocess_resident_memory_bytes_other 5\n";
        assert_eq!(
            metric_value(body, "process_resident_memory_bytes"),
            Some(1024.0)
        );
        assert_eq!(metric_value(body, "aurix_cascade_links"), Some(3.0));
        assert_eq!(metric_value(body, "missing"), None);
        let links = metric_labelled(body, "aurix_cascade_links", "transport");
        assert_eq!(links.get("udp"), Some(&2.0));
        assert_eq!(links.get("tcp"), Some(&1.0));
    }

    #[test]
    fn growth_needs_enough_samples_and_compares_quarters() {
        assert_eq!(growth(&[1.0; 7]), None);
        let flat = vec![100.0; 12];
        assert!(growth(&flat).unwrap().abs() < 1e-9);
        let rising: Vec<f64> = (0..12).map(|i| 100.0 + 10.0 * i as f64).collect();
        assert!((growth(&rising).unwrap() - (200.0 / 110.0 - 1.0)).abs() < 1e-9);
    }

    fn interval(at: f64, rss: f64) -> IntervalRow {
        IntervalRow {
            kind: "interval",
            at,
            elapsed_secs: at,
            steady: true,
            bots_bound: 0,
            bots_joined: 0,
            bots_hearing_ok: 0,
            hearing_min: 1.0,
            hearing_mean: 1.0,
            loss_max_percent: 0.0,
            rtt_max_ms: 0.0,
            mos_min: 4.4,
            frames_lost: 0,
            underruns: 0,
            replayed: 0,
            reconnects: 0,
            recovery_ms_max: 0,
            failovers: 0,
            hard_reconnects: 0,
            request_failures: 0,
            server_errors: 0,
            paths: BTreeMap::new(),
            nodes: Vec::new(),
            self_rss_bytes: Some(rss),
            self_open_fds: Some(80.0),
            bots: Vec::new(),
            violations: Vec::new(),
        }
    }

    fn hook(ended_at: f64) -> ChaosRow {
        ChaosRow {
            kind: "chaos",
            name: "kill".into(),
            started_at: ended_at - 5.0,
            ended_at,
            exit_ok: true,
            output_tail: String::new(),
            recovered_in_secs: Some(1.0),
            recovered: true,
            unhealthy_after: Vec::new(),
        }
    }

    #[test]
    fn resource_baseline_starts_after_the_first_hook() {
        // 21 MB before the first hook, a one-off step to 33 MB right after it, flat afterwards.
        let rows: Vec<IntervalRow> = (0..24)
            .map(|i| interval(i as f64, if i < 9 { 21e6 } else { 33e6 }))
            .collect();
        let steady: Vec<&IntervalRow> = rows.iter().collect();
        let all: Vec<f64> = steady.iter().filter_map(|r| r.self_rss_bytes).collect();
        assert!(
            growth(&all).unwrap() > 0.25,
            "the step alone trips the budget"
        );

        let judged = resource_samples(&steady, &[hook(20.0), hook(8.5)]);
        assert_eq!(judged.len(), 15);
        assert!(judged.iter().all(|r| r.at >= 8.5));
        let rss: Vec<f64> = judged.iter().filter_map(|r| r.self_rss_bytes).collect();
        assert!(growth(&rss).unwrap().abs() < 1e-9);

        // No hook yet: every steady interval counts.
        assert_eq!(resource_samples(&steady, &[]).len(), 24);
        // Too short after the first hook: no verdict instead of a spurious one.
        let short = resource_samples(&steady, &[hook(20.0)]);
        let rss: Vec<f64> = short.iter().filter_map(|r| r.self_rss_bytes).collect();
        assert_eq!(growth(&rss), None);
    }
}
