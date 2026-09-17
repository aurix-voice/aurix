use once_cell::sync::Lazy;
use prometheus::{
    register_gauge, register_histogram, register_histogram_vec,
    register_int_counter, register_int_counter_vec, register_int_gauge,
    Encoder, Gauge, Histogram, HistogramVec, IntCounter, IntCounterVec, IntGauge,
    TextEncoder,
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

pub static SESSIONS_TOTAL: Lazy<IntCounter> = Lazy::new(|| {
    register_int_counter!("aurix_sessions_total", "Total sessions created").unwrap()
});

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

pub static PACKETS_SENT: Lazy<IntCounter> = Lazy::new(|| {
    register_int_counter!("aurix_packets_sent_total", "Total packets sent").unwrap()
});

pub static PACKETS_DROPPED: Lazy<IntCounter> = Lazy::new(|| {
    register_int_counter!("aurix_packets_dropped_total", "Total packets dropped").unwrap()
});

pub static BYTES_RECEIVED: Lazy<IntCounter> = Lazy::new(|| {
    register_int_counter!("aurix_bytes_received_total", "Total bytes received").unwrap()
});

pub static BYTES_SENT: Lazy<IntCounter> = Lazy::new(|| {
    register_int_counter!("aurix_bytes_sent_total", "Total bytes sent").unwrap()
});

pub static PACKET_LOSS_RATE: Lazy<Gauge> = Lazy::new(|| {
    register_gauge!("aurix_packet_loss_rate", "Current packet loss rate").unwrap()
});

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

pub static NODE_CPU_USAGE: Lazy<Gauge> = Lazy::new(|| {
    register_gauge!("aurix_node_cpu_usage", "Node CPU usage percentage").unwrap()
});

pub static NODE_MEMORY_USAGE: Lazy<Gauge> = Lazy::new(|| {
    register_gauge!("aurix_node_memory_usage", "Node memory usage percentage").unwrap()
});

pub static NODE_BANDWIDTH_IN: Lazy<Gauge> = Lazy::new(|| {
    register_gauge!("aurix_node_bandwidth_in_mbps", "Inbound bandwidth in Mbps").unwrap()
});

pub static NODE_BANDWIDTH_OUT: Lazy<Gauge> = Lazy::new(|| {
    register_gauge!("aurix_node_bandwidth_out_mbps", "Outbound bandwidth in Mbps").unwrap()
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

pub static ACTIVE_BANS: Lazy<IntGauge> = Lazy::new(|| {
    register_int_gauge!("aurix_active_bans", "Number of active bans").unwrap()
});

// ── TURN Metrics ──

pub static TURN_ALLOCATIONS: Lazy<IntGauge> = Lazy::new(|| {
    register_int_gauge!("aurix_turn_allocations", "Active TURN allocations").unwrap()
});

pub static STUN_REQUESTS: Lazy<IntCounter> = Lazy::new(|| {
    register_int_counter!("aurix_stun_requests_total", "Total STUN requests").unwrap()
});

// ── Rate Limit Metrics ──

pub static WS_CONNECTIONS: Lazy<IntGauge> = Lazy::new(|| {
    register_int_gauge!("aurix_ws_connections", "Open player WebSocket connections on this node").unwrap()
});

pub static RATE_LIMIT_HITS: Lazy<IntCounter> = Lazy::new(|| {
    register_int_counter!(
        "aurix_rate_limit_hits_total",
        "Total rate limit hits"
    )
    .unwrap()
});

pub fn gather_metrics() -> String {
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

    let encoder = TextEncoder::new();
    let metric_families = prometheus::gather();
    let mut buffer = Vec::new();
    encoder.encode(&metric_families, &mut buffer).unwrap();
    String::from_utf8(buffer).unwrap()
}

pub async fn metrics_handler() -> String {
    gather_metrics()
}