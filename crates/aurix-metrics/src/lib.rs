use once_cell::sync::Lazy;
use prometheus::{
    register_gauge, register_histogram, register_histogram_vec, register_int_counter,
    register_int_counter_vec, register_int_gauge, register_int_gauge_vec, Encoder, Gauge,
    Histogram, HistogramVec, IntCounter, IntCounterVec, IntGauge, IntGaugeVec, TextEncoder,
};

// ── Connection Metrics ──

pub static ACTIVE_SESSIONS: Lazy<IntGauge> = Lazy::new(|| {
    register_int_gauge!("aurix_active_sessions", "Number of active sessions").unwrap()
});

pub static ACTIVE_CHANNELS: Lazy<IntGauge> = Lazy::new(|| {
    register_int_gauge!("aurix_active_channels", "Number of active channels").unwrap()
});

pub static ACTIVE_PARTICIPANTS: Lazy<IntGauge> = Lazy::new(|| {
    register_int_gauge!("aurix_active_participants", "Total active participants").unwrap()
});

pub static SESSIONS_TOTAL: Lazy<IntCounter> =
    Lazy::new(|| register_int_counter!("aurix_sessions_total", "Total sessions created").unwrap());

pub static SESSION_DURATION: Lazy<Histogram> = Lazy::new(|| {
    register_histogram!(
        "aurix_session_duration_seconds",
        "Session duration in seconds",
        vec![1.0, 5.0, 15.0, 30.0, 60.0, 120.0, 300.0, 600.0, 1800.0, 3600.0]
    )
    .unwrap()
});

// ── Media Metrics ──

pub static PACKETS_RECEIVED: Lazy<IntCounter> = Lazy::new(|| {
    register_int_counter!("aurix_packets_received_total", "Total packets received").unwrap()
});

pub static PACKETS_SENT: Lazy<IntCounter> =
    Lazy::new(|| register_int_counter!("aurix_packets_sent_total", "Total packets sent").unwrap());

pub static PACKETS_DROPPED: Lazy<IntCounter> = Lazy::new(|| {
    register_int_counter!("aurix_packets_dropped_total", "Total packets dropped").unwrap()
});

pub static BYTES_RECEIVED: Lazy<IntCounter> = Lazy::new(|| {
    register_int_counter!("aurix_bytes_received_total", "Total bytes received").unwrap()
});

pub static BYTES_SENT: Lazy<IntCounter> =
    Lazy::new(|| register_int_counter!("aurix_bytes_sent_total", "Total bytes sent").unwrap());

pub static PACKET_LOSS_RATE: Lazy<Gauge> =
    Lazy::new(|| register_gauge!("aurix_packet_loss_rate", "Current packet loss rate").unwrap());

pub static RTT_MS: Lazy<Histogram> = Lazy::new(|| {
    register_histogram!(
        "aurix_rtt_milliseconds",
        "Round-trip time in milliseconds",
        vec![5.0, 10.0, 20.0, 50.0, 100.0, 200.0, 500.0, 1000.0]
    )
    .unwrap()
});

pub static JITTER_MS: Lazy<Histogram> = Lazy::new(|| {
    register_histogram!(
        "aurix_jitter_milliseconds",
        "Jitter in milliseconds",
        vec![1.0, 2.0, 5.0, 10.0, 20.0, 50.0, 100.0]
    )
    .unwrap()
});

// ── Session quality (E-model) ──

/// One observation per rated session per `media.quality_interval_ms`: the merged E-model MOS.
/// `histogram_quantile(0.5, rate(aurix_session_mos_bucket[5m]))` is the fleet median MOS;
/// `_sum / _count` the mean.
pub static SESSION_MOS: Lazy<Histogram> = Lazy::new(|| {
    register_histogram!(
        "aurix_session_mos",
        "E-model MOS of rated sessions, one observation per session per quality period",
        vec![1.5, 2.0, 2.5, 2.8, 3.1, 3.4, 3.6, 3.8, 4.0, 4.2, 4.3, 4.4]
    )
    .unwrap()
});

/// Server-measured uplink packet loss per rated session per quality period (percent).
pub static UPLINK_LOSS_PERCENT: Lazy<Histogram> = Lazy::new(|| {
    register_histogram!(
        "aurix_uplink_loss_percent",
        "Uplink packet loss per rated session per quality period, percent",
        vec![0.5, 1.0, 2.0, 3.0, 5.0, 10.0, 20.0, 40.0]
    )
    .unwrap()
});

/// Server-measured uplink jitter per rated session per quality period (RFC 3550, ms).
pub static UPLINK_JITTER_MS: Lazy<Histogram> = Lazy::new(|| {
    register_histogram!(
        "aurix_uplink_jitter_milliseconds",
        "Uplink inter-arrival jitter per rated session per quality period, milliseconds",
        vec![1.0, 2.0, 5.0, 10.0, 20.0, 30.0, 50.0, 100.0]
    )
    .unwrap()
});

/// Rated sessions on this node by their current bar count (`bars` = `1`..`5`).
pub static SESSIONS_BY_BARS: Lazy<IntGaugeVec> = Lazy::new(|| {
    register_int_gauge_vec!(
        "aurix_sessions_by_bars",
        "Rated sessions by current network-quality bars",
        &["bars"]
    )
    .unwrap()
});

/// Sessions currently in the MOS alert state (`quality.alert {metric: "mos"}` raised, not yet
/// recovered).
pub static SESSIONS_MOS_DEGRADED: Lazy<IntGauge> = Lazy::new(|| {
    register_int_gauge!(
        "aurix_sessions_mos_degraded",
        "Sessions whose MOS is below quality.mos_alert_threshold (debounced)"
    )
    .unwrap()
});

/// `metric` is `packet_loss`, `uplink_packet_loss` or `mos`; `event` is `alert` or
/// `recovered` (only `mos` recovers).
pub static QUALITY_EVENTS: Lazy<IntCounterVec> = Lazy::new(|| {
    register_int_counter_vec!(
        "aurix_quality_events_total",
        "quality.alert / quality.recovered events published",
        &["metric", "event"]
    )
    .unwrap()
});

// ── API Metrics ──

pub static API_REQUESTS_TOTAL: Lazy<IntCounterVec> = Lazy::new(|| {
    register_int_counter_vec!(
        "aurix_api_requests_total",
        "Total API requests",
        &["method", "path", "status"]
    )
    .unwrap()
});

pub static API_REQUEST_DURATION: Lazy<HistogramVec> = Lazy::new(|| {
    register_histogram_vec!(
        "aurix_api_request_duration_seconds",
        "API request duration",
        &["method", "path"],
        vec![0.001, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 5.0]
    )
    .unwrap()
});

// ── Node Metrics ──

pub static NODE_CPU_USAGE: Lazy<Gauge> =
    Lazy::new(|| register_gauge!("aurix_node_cpu_usage", "Node CPU usage percentage").unwrap());

pub static NODE_MEMORY_USAGE: Lazy<Gauge> = Lazy::new(|| {
    register_gauge!("aurix_node_memory_usage", "Node memory usage percentage").unwrap()
});

pub static NODE_BANDWIDTH_IN: Lazy<Gauge> = Lazy::new(|| {
    register_gauge!("aurix_node_bandwidth_in_mbps", "Inbound bandwidth in Mbps").unwrap()
});

pub static NODE_BANDWIDTH_OUT: Lazy<Gauge> = Lazy::new(|| {
    register_gauge!(
        "aurix_node_bandwidth_out_mbps",
        "Outbound bandwidth in Mbps"
    )
    .unwrap()
});

// ── Moderation Metrics ──

pub static MODERATION_EVENTS: Lazy<IntCounterVec> = Lazy::new(|| {
    register_int_counter_vec!(
        "aurix_moderation_events_total",
        "Total moderation events",
        &["event_type"]
    )
    .unwrap()
});

pub static ACTIVE_BANS: Lazy<IntGauge> =
    Lazy::new(|| register_int_gauge!("aurix_active_bans", "Number of active bans").unwrap());

// ── TURN Metrics ──

pub static TURN_ALLOCATIONS: Lazy<IntGauge> =
    Lazy::new(|| register_int_gauge!("aurix_turn_allocations", "Active TURN allocations").unwrap());

pub static STUN_REQUESTS: Lazy<IntCounter> = Lazy::new(|| {
    register_int_counter!("aurix_stun_requests_total", "Total STUN requests").unwrap()
});

// ── Rate Limit Metrics ──

pub static WS_CONNECTIONS: Lazy<IntGauge> = Lazy::new(|| {
    register_int_gauge!(
        "aurix_ws_connections",
        "Open player WebSocket connections on this node"
    )
    .unwrap()
});

pub static WS_SESSIONS_DETACHED: Lazy<IntGauge> = Lazy::new(|| {
    register_int_gauge!(
        "aurix_ws_sessions_detached",
        "Player sessions whose WebSocket dropped and that are waiting for a resume"
    )
    .unwrap()
});

pub static WS_SESSIONS_RESUMED: Lazy<IntCounter> = Lazy::new(|| {
    register_int_counter!(
        "aurix_ws_sessions_resumed_total",
        "Player sessions successfully resumed after a WebSocket drop"
    )
    .unwrap()
});

pub static WS_SESSIONS_MIGRATED: Lazy<IntCounter> = Lazy::new(|| {
    register_int_counter!(
        "aurix_ws_sessions_migrated_total",
        "Player sessions adopted from another node (cross-node resume)"
    )
    .unwrap()
});

pub static WS_TAKEOVERS_REFUSED: Lazy<IntCounterVec> = Lazy::new(|| {
    register_int_counter_vec!(
        "aurix_ws_takeovers_refused_total",
        "Cross-node resume attempts that fell back to a fresh session",
        &["reason"]
    )
    .unwrap()
});

pub static NODES_REAPED: Lazy<IntCounter> = Lazy::new(|| {
    register_int_counter!(
        "aurix_nodes_reaped_total",
        "Media nodes declared lost by this node and cleaned up in the database"
    )
    .unwrap()
});

pub static REDIS_FAILOVERS: Lazy<IntCounter> = Lazy::new(|| {
    register_int_counter!(
        "aurix_redis_failovers_total",
        "Redis master switches followed via Sentinel"
    )
    .unwrap()
});

pub static RATE_LIMIT_HITS: Lazy<IntCounter> = Lazy::new(|| {
    register_int_counter!("aurix_rate_limit_hits_total", "Total rate limit hits").unwrap()
});

/// `scope` is the protected action (`api_ip`, `api_key`, `connect`, `join`, `block`, `report`,
/// `admin_login`); `backend` is `fleet` (shared Redis bucket) or `local` (this node only).
pub static RATE_LIMIT_HITS_BY_SCOPE: Lazy<IntCounterVec> = Lazy::new(|| {
    register_int_counter_vec!(
        "aurix_rate_limit_scope_hits_total",
        "Rate limit hits by scope and backend",
        &["scope", "backend"]
    )
    .unwrap()
});

/// Redis errors while checking a fleet-wide bucket (the node then falls back to local
/// buckets or rejects, per `rate_limiting.fail_closed`).
pub static RATE_LIMIT_BACKEND_ERRORS: Lazy<IntCounter> = Lazy::new(|| {
    register_int_counter!(
        "aurix_rate_limit_backend_errors_total",
        "Fleet rate limiter Redis errors"
    )
    .unwrap()
});

/// Connects / channel joins refused by a per-application quota (`max_concurrent_sessions`,
/// `monthly_participant_minutes`).
pub static QUOTA_REJECTIONS: Lazy<IntCounterVec> = Lazy::new(|| {
    register_int_counter_vec!(
        "aurix_quota_rejections_total",
        "Requests refused by a per-application quota",
        &["quota"]
    )
    .unwrap()
});

/// Metered usage deltas flushed to Postgres.
pub static USAGE_FLUSHED: Lazy<IntCounter> = Lazy::new(|| {
    register_int_counter!(
        "aurix_usage_deltas_flushed_total",
        "Usage counter deltas written to the database"
    )
    .unwrap()
});

// ── Webhook / event stream Metrics ──

/// `result` is `delivered`, `retry` (attempt failed, rescheduled), `failed` (gave up),
/// `dropped` (queue full or bus lag).
pub static WEBHOOK_DELIVERIES: Lazy<IntCounterVec> = Lazy::new(|| {
    register_int_counter_vec!(
        "aurix_webhook_deliveries_total",
        "Webhook delivery attempts by outcome",
        &["result"]
    )
    .unwrap()
});

pub static WEBHOOK_PENDING: Lazy<IntGauge> = Lazy::new(|| {
    register_int_gauge!(
        "aurix_webhook_deliveries_leased",
        "Webhook deliveries currently in flight on this node"
    )
    .unwrap()
});

pub static SSE_CLIENTS: Lazy<IntGauge> = Lazy::new(|| {
    register_int_gauge!(
        "aurix_event_stream_clients",
        "Open GET /v1/events server-sent-event streams"
    )
    .unwrap()
});

// ── Content safety Metrics ──

/// `source` is `voice` or `text`; `outcome` is `clean`, `incident`, `blocked` (text rejected)
/// or `error` (classifier unavailable).
pub static SAFETY_CHECKS: Lazy<IntCounterVec> = Lazy::new(|| {
    register_int_counter_vec!(
        "aurix_safety_checks_total",
        "Transcripts and chat messages run through the safety pipeline, by outcome",
        &["source", "outcome"]
    )
    .unwrap()
});

/// `action` is `mute` or `kick`.
pub static SAFETY_ACTIONS: Lazy<IntCounterVec> = Lazy::new(|| {
    register_int_counter_vec!(
        "aurix_safety_actions_total",
        "Automatic moderation actions taken by the safety pipeline",
        &["action"]
    )
    .unwrap()
});

/// `direction` is `uplink` (μ-law → Opus) or `downlink` (Opus → μ-law); `outcome` is `ok` or
/// `error`.
pub static PCMU_FRAMES: Lazy<IntCounterVec> = Lazy::new(|| {
    register_int_counter_vec!(
        "aurix_pcmu_frames_total",
        "Audio frames transcoded for sessions that negotiated the PCMU fallback codec",
        &["direction", "outcome"]
    )
    .unwrap()
});

// ── Live translation Metrics ──

/// `outcome` is `ok` (provider answered), `cached`, `error` (provider failed or timed out),
/// `busy` (concurrency cap) or `skipped` (segment too long / too many languages).
pub static TRANSLATIONS: Lazy<IntCounterVec> = Lazy::new(|| {
    register_int_counter_vec!(
        "aurix_translations_total",
        "Transcript segments translated for listeners, by outcome",
        &["outcome"]
    )
    .unwrap()
});

pub static TRANSLATION_LATENCY: Lazy<Histogram> = Lazy::new(|| {
    register_histogram!(
        "aurix_translation_latency_seconds",
        "Round trip of one machine-translation request",
        vec![0.1, 0.25, 0.5, 1.0, 2.0, 4.0, 8.0, 15.0]
    )
    .unwrap()
});

pub static PCMU_SESSIONS: Lazy<IntGauge> = Lazy::new(|| {
    register_int_gauge!(
        "aurix_pcmu_sessions",
        "Sessions currently using the PCMU fallback codec"
    )
    .unwrap()
});

/// `direction` is `uplink` (client → node over the WebSocket) or `downlink`; `outcome` is
/// `sent`/`received`, `dropped` (downlink queue full or connection gone) or `rejected`
/// (uplink frame that failed decoding or authentication).
pub static TUNNEL_PACKETS: Lazy<IntCounterVec> = Lazy::new(|| {
    register_int_counter_vec!(
        "aurix_tunnel_packets_total",
        "AURX packets carried over the WebSocket media tunnel (UDP fallback)",
        &["direction", "outcome"]
    )
    .unwrap()
});

pub static TUNNEL_SESSIONS: Lazy<IntGauge> = Lazy::new(|| {
    register_int_gauge!(
        "aurix_tunnel_sessions",
        "Native sessions whose media is currently bound through the WebSocket tunnel"
    )
    .unwrap()
});

/// `kind` is `shared` (one mix for every uniform receiver of a channel) or `private`.
pub static DOWNLINK_MIXERS: Lazy<IntGaugeVec> = Lazy::new(|| {
    register_int_gauge_vec!(
        "aurix_downlink_mixers",
        "Live server-side channel mixers serving native receivers in mixed downlink mode",
        &["kind"]
    )
    .unwrap()
});

/// `outcome` is `sent` (one mixed frame delivered to one receiver), `dropped` or `failed`.
pub static DOWNLINK_MIX_FRAMES: Lazy<IntCounterVec> = Lazy::new(|| {
    register_int_counter_vec!(
        "aurix_downlink_mix_frames_total",
        "Server-mixed frames produced for native receivers in mixed downlink mode",
        &["outcome"]
    )
    .unwrap()
});

/// Per-speaker streams a receiver did not get because it was over its stream cap
/// (`ChannelConfig.audience.max_streams`).
pub static STREAMS_CAPPED: Lazy<IntCounter> = Lazy::new(|| {
    register_int_counter!(
        "aurix_streams_capped_total",
        "Downlink packets withheld by the per-receiver stream cap"
    )
    .unwrap()
});

/// `role` is `origin` (a locally received client packet sent to peers), `hub` (a relay
/// envelope from a peer re-forwarded along the relay tree) or `hop_limit` (an envelope that
/// had a forwarding rule but already travelled `MAX_RELAY_HOPS`, dropped).
pub static CASCADE_FORWARDED: Lazy<IntCounterVec> = Lazy::new(|| {
    register_int_counter_vec!(
        "aurix_cascade_forwarded_total",
        "Cascade relay envelopes sent to peer nodes, by the sending node's role",
        &["role"]
    )
    .unwrap()
});

/// Channels this node currently forwards for as a relay-tree hub (it re-forwards envelopes
/// between its region and other regions), including channels it does not host itself.
pub static CASCADE_HUB_CHANNELS: Lazy<IntGauge> = Lazy::new(|| {
    register_int_gauge!(
        "aurix_cascade_hub_channels",
        "Channels for which this node acts as a cascade relay-tree hub"
    )
    .unwrap()
});

pub fn gather_metrics() -> String {
    let _ = &*PCMU_FRAMES;
    let _ = &*PCMU_SESSIONS;
    let _ = &*TUNNEL_PACKETS;
    let _ = &*TUNNEL_SESSIONS;
    let _ = &*DOWNLINK_MIXERS;
    let _ = &*DOWNLINK_MIX_FRAMES;
    let _ = &*STREAMS_CAPPED;
    // Touch all lazy statics to ensure registration
    let _ = &*ACTIVE_SESSIONS;
    let _ = &*ACTIVE_CHANNELS;
    let _ = &*ACTIVE_PARTICIPANTS;
    let _ = &*SESSIONS_TOTAL;
    let _ = &*PACKETS_RECEIVED;
    let _ = &*PACKETS_SENT;
    let _ = &*BYTES_RECEIVED;
    let _ = &*BYTES_SENT;
    let _ = &*NODE_CPU_USAGE;
    let _ = &*NODE_MEMORY_USAGE;
    let _ = &*WEBHOOK_DELIVERIES;
    let _ = &*WEBHOOK_PENDING;
    let _ = &*SSE_CLIENTS;
    let _ = &*SAFETY_CHECKS;
    let _ = &*SAFETY_ACTIONS;

    let encoder = TextEncoder::new();
    let metric_families = prometheus::gather();
    let mut buffer = Vec::new();
    encoder.encode(&metric_families, &mut buffer).unwrap();
    String::from_utf8(buffer).unwrap()
}

pub async fn metrics_handler() -> String {
    gather_metrics()
}
