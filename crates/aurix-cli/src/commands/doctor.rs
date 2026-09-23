//! `aurix doctor`: local operator preflight for one node. Loads the node configuration the
//! way the server does, then checks listeners, dependencies and certificates. Needs no API
//! profile or credential; `aurix diagnose` stays the remote, API-level counterpart.
//!
//! Two modes, chosen by whether `/health` answers on the configured API port:
//! * preflight (no node): every configured listener must be free to bind;
//! * running node: every configured listener must be in use, and the QUIC / TLS tunnel /
//!   WebTransport endpoints are handshaked so the served certificate can be compared with the
//!   one on disk (the fingerprint clients pin).

use std::collections::BTreeSet;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use aurix_common::config::{is_secret_key, redact_url_credentials, AurixConfig};
use aurix_common::net::parse_bind_addr;
use aurix_common::protocol::{QUIC_ALPN, TLS_TUNNEL_ALPN};
use aurix_common::quic::cert_fingerprint;
use aurix_common::redis_pool::RedisSource;
use aurix_db::migrations::{migration_report, MigrationReport};
use clap::Args as ClapArgs;
use serde::Serialize;
use serde_json::{json, Value};
use sqlx::{Connection, PgConnection};
use tokio_rustls::rustls;
use tokio_rustls::rustls::pki_types::pem::PemObject;
use tokio_rustls::rustls::pki_types::{CertificateDer, ServerName};

use crate::output::{Format, Printer};

const WEBTRANSPORT_ALPN: &[u8] = b"h3";
/// Chrome accepts `serverCertificateHashes` only for certificates valid ≤ 14 days.
const WEBTRANSPORT_MAX_VALIDITY: Duration = Duration::from_secs(14 * 24 * 3600);
const CERT_EXPIRY_WARN: Duration = Duration::from_secs(14 * 24 * 3600);

#[derive(ClapArgs)]
pub struct Args {
    /// Node configuration file (same `--config` the server takes; env `AURIX__*` applies).
    #[arg(long, short, default_value = "configs/default", value_name = "PATH")]
    config: String,
    /// Do not connect to Redis and PostgreSQL.
    #[arg(long)]
    skip_remote: bool,
    /// Do not handshake the node's QUIC / TLS tunnel / WebTransport endpoints.
    #[arg(long)]
    skip_probes: bool,
    /// Per-check network timeout in seconds.
    #[arg(long, default_value_t = 5)]
    timeout: u64,
    /// Exit non-zero on warnings too.
    #[arg(long)]
    strict: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "lowercase")]
enum Status {
    Skip,
    Ok,
    Warn,
    Fail,
}

#[derive(Debug, Serialize)]
struct Check {
    id: String,
    status: Status,
    summary: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    hints: Vec<String>,
    #[serde(skip_serializing_if = "Value::is_null")]
    details: Value,
}

impl Check {
    fn new(id: impl Into<String>, status: Status, summary: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            status,
            summary: summary.into(),
            hints: Vec::new(),
            details: Value::Null,
        }
    }

    fn hint(mut self, hint: impl Into<String>) -> Self {
        self.hints.push(hint.into());
        self
    }
}

/// Which of the node's listeners a check refers to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Proto {
    Tcp,
    Udp,
}

struct Listener {
    id: &'static str,
    proto: Proto,
    addr: SocketAddr,
    role: &'static str,
}

/// Whether a node built from this configuration is already up (its `/health` answers).
struct RunningNode {
    version: Option<String>,
    api_addr: SocketAddr,
}

pub async fn run(args: &Args, out: &Printer) -> anyhow::Result<()> {
    let timeout = Duration::from_secs(args.timeout.max(1));
    let mut checks = Vec::new();

    let config = match load_config(&args.config) {
        Ok((cfg, check)) => {
            checks.push(check);
            Some(cfg)
        }
        Err(check) => {
            checks.push(check);
            None
        }
    };

    let mut running = None;
    if let Some(cfg) = &config {
        checks.push(production_preview(cfg));
        checks.push(external_urls(cfg));
        let (node, health_check) = detect_running_node(cfg, timeout).await;
        checks.push(health_check);
        running = node;
        checks.extend(listener_checks(cfg, running.is_some()));
        checks.extend(certificate_checks(cfg));
        if !args.skip_probes {
            match &running {
                Some(_) => {
                    checks.push(probe_quic(cfg, timeout).await);
                    checks.extend(probe_tls_tunnel(cfg, timeout).await);
                    checks.extend(probe_webtransport(cfg, timeout).await);
                }
                None => {
                    let skipped = "no node is running; start it and re-run to handshake";
                    checks.push(Check::new("probe.quic", Status::Skip, skipped));
                    if cfg.media.tls_tunnel_port != 0 {
                        checks.push(Check::new("probe.tls_tunnel", Status::Skip, skipped));
                    }
                    if cfg.media.webtransport_port != 0 {
                        checks.push(Check::new("probe.webtransport", Status::Skip, skipped));
                    }
                }
            }
        }
        if args.skip_remote {
            checks.push(Check::new("redis", Status::Skip, "--skip-remote"));
            checks.push(Check::new("postgres", Status::Skip, "--skip-remote"));
            checks.push(Check::new("migrations", Status::Skip, "--skip-remote"));
        } else {
            checks.push(check_redis(cfg, timeout).await);
            checks.extend(check_postgres(cfg, timeout).await);
        }
    }

    let redactor = config.as_ref().map(Redactor::new).unwrap_or_default();
    let worst = checks.iter().map(|c| c.status).max().unwrap_or(Status::Ok);
    let ok = match worst {
        Status::Fail => false,
        Status::Warn => !args.strict,
        Status::Ok | Status::Skip => true,
    };
    let mut document = json!({
        "ok": ok,
        "status": worst,
        "mode": if running.is_some() { "running" } else { "preflight" },
        "config": args.config,
        "cli": env!("CARGO_PKG_VERSION"),
        "node": running.as_ref().map(|n| json!({
            "version": n.version,
            "api": n.api_addr.to_string(),
        })),
        "counts": {
            "ok": checks.iter().filter(|c| c.status == Status::Ok).count(),
            "warn": checks.iter().filter(|c| c.status == Status::Warn).count(),
            "fail": checks.iter().filter(|c| c.status == Status::Fail).count(),
            "skip": checks.iter().filter(|c| c.status == Status::Skip).count(),
        },
        "checks": checks,
    });
    redactor.scrub_value(&mut document);

    if out.format == Format::Pretty && out.field.is_none() {
        print_human(&document);
    } else {
        out.print(&document)?;
    }
    if !ok {
        std::process::exit(2);
    }
    Ok(())
}

fn print_human(doc: &Value) {
    let node = doc["node"]
        .as_object()
        .map(|n| {
            format!(
                "node {} at {}",
                n["version"].as_str().unwrap_or("?"),
                n["api"].as_str().unwrap_or("?")
            )
        })
        .unwrap_or_else(|| "no node running (preflight)".to_string());
    println!(
        "aurix doctor {} — {} — {}",
        doc["cli"].as_str().unwrap_or(""),
        doc["config"].as_str().unwrap_or(""),
        node
    );
    for check in doc["checks"].as_array().into_iter().flatten() {
        println!(
            "  {:<5} {:<24} {}",
            check["status"]
                .as_str()
                .map(str::to_ascii_uppercase)
                .unwrap_or_default(),
            check["id"].as_str().unwrap_or(""),
            check["summary"].as_str().unwrap_or("")
        );
        for hint in check["hints"].as_array().into_iter().flatten() {
            println!("        -> {}", hint.as_str().unwrap_or(""));
        }
    }
    let c = &doc["counts"];
    println!(
        "{}: {} ok, {} warn, {} fail, {} skipped",
        doc["status"]
            .as_str()
            .map(str::to_ascii_uppercase)
            .unwrap_or_default(),
        c["ok"],
        c["warn"],
        c["fail"],
        c["skip"]
    );
}

// ---------------------------------------------------------------------------------------------
// Configuration

fn load_config(path: &str) -> Result<(AurixConfig, Check), Check> {
    let file_present = ["", ".toml", ".yaml", ".yml", ".json"]
        .iter()
        .any(|ext| Path::new(&format!("{path}{ext}")).is_file());
    if !file_present {
        return Err(Check::new(
            "config",
            Status::Fail,
            format!("configuration file '{path}' not found"),
        )
        .hint("pass --config <path> (the server's --config value); AURIX__* env vars apply"));
    }
    match AurixConfig::load(Some(path)) {
        Ok(cfg) => {
            let mut check = Check::new(
                "config",
                Status::Ok,
                format!(
                    "loaded and valid (environment={}, region={}, node_id={})",
                    cfg.server.environment,
                    serde_json::to_value(cfg.server.region)
                        .ok()
                        .and_then(|v| v.as_str().map(str::to_string))
                        .unwrap_or_default(),
                    cfg.server.node_id.as_deref().unwrap_or("auto")
                ),
            );
            check.details = json!({
                "environment": cfg.server.environment,
                "quic": cfg.media.quic,
                "tls_tunnel_port": cfg.media.tls_tunnel_port,
                "webtransport_port": cfg.media.webtransport_port,
                "cascade": cfg.media.cascade_secret.is_some(),
                "turn": cfg.turn.enabled,
                "metrics": cfg.metrics.enabled,
                "run_migrations": cfg.database.run_migrations,
            });
            Ok((cfg, check))
        }
        Err(e) => {
            let mut check = Check::new(
                "config",
                Status::Fail,
                format!("invalid configuration: {e:#}"),
            );
            if !default_layer_present() {
                check = check.hint(
                    "the node layers this file over configs/default.toml in its working \
                     directory and none is here: run doctor from the node's working directory",
                );
            }
            Err(check)
        }
    }
}

fn default_layer_present() -> bool {
    [".toml", ".yaml", ".yml", ".json"]
        .iter()
        .any(|ext| Path::new(&format!("configs/default{ext}")).is_file())
}

/// A development configuration that would be rejected in production is worth knowing about
/// before the environment flag flips.
fn production_preview(cfg: &AurixConfig) -> Check {
    if cfg.is_production() {
        return Check::new(
            "production",
            Status::Ok,
            "environment is production; production rules applied above",
        );
    }
    let mut prod = cfg.clone();
    prod.server.environment = "production".to_string();
    match prod.validate() {
        Ok(()) => Check::new(
            "production",
            Status::Ok,
            "configuration would also pass production validation",
        ),
        Err(e) => Check::new(
            "production",
            Status::Warn,
            format!("would be rejected with environment=production: {e:#}"),
        )
        .hint("fine for development; fix before promoting this node"),
    }
}

fn external_urls(cfg: &AurixConfig) -> Check {
    let mut hints = Vec::new();
    let url = cfg.server.external_url.trim();
    if url.is_empty() {
        hints.push("server.external_url is empty: tokens and SDKs get no endpoint".to_string());
    } else if !(url.starts_with("http://") || url.starts_with("https://")) {
        hints.push(format!("server.external_url '{url}' is not an http(s) URL"));
    } else if url.starts_with("http://") && cfg.is_production() {
        hints.push("server.external_url uses http:// in production".to_string());
    }
    if let Some(ws) = cfg.server.external_ws_url.as_deref() {
        if !(ws.starts_with("ws://") || ws.starts_with("wss://")) {
            hints.push(format!("server.external_ws_url '{ws}' is not a ws(s) URL"));
        }
    }
    let external_ip = cfg.media.external_ip.as_deref().unwrap_or("");
    if external_ip.is_empty() && cfg.media.external_ipv6.is_none() {
        hints.push(
            "media.external_ip is unset: clients are told the bind address, which only works \
             on one host"
                .to_string(),
        );
    } else if let Ok(ip) = external_ip.parse::<IpAddr>() {
        if ip.is_loopback() || ip.is_unspecified() {
            hints.push(format!(
                "media.external_ip {ip} is not reachable from other machines"
            ));
        }
    }
    let status = if hints.is_empty() {
        Status::Ok
    } else {
        Status::Warn
    };
    let mut check = Check::new(
        "endpoints",
        status,
        format!(
            "external_url={} media.external_ip={}",
            if url.is_empty() { "-" } else { url },
            if external_ip.is_empty() {
                "-"
            } else {
                external_ip
            }
        ),
    );
    check.hints = hints;
    check
}

// ---------------------------------------------------------------------------------------------
// Listeners

/// Address to connect to for a listener bound on `bind`.
fn connect_addr(bind: SocketAddr) -> SocketAddr {
    match bind.ip() {
        IpAddr::V4(ip) if ip.is_unspecified() => {
            SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), bind.port())
        }
        IpAddr::V6(ip) if ip.is_unspecified() => {
            SocketAddr::new(IpAddr::V6(Ipv6Addr::LOCALHOST), bind.port())
        }
        _ => bind,
    }
}

async fn detect_running_node(cfg: &AurixConfig, timeout: Duration) -> (Option<RunningNode>, Check) {
    let bind = match parse_bind_addr(&cfg.server.host, cfg.server.api_port) {
        Ok(a) => a,
        Err(e) => {
            return (
                None,
                Check::new(
                    "node",
                    Status::Fail,
                    format!("server.host '{}' is not bindable: {e}", cfg.server.host),
                ),
            )
        }
    };
    let api_addr = connect_addr(bind);
    let client = match reqwest::Client::builder().timeout(timeout).build() {
        Ok(c) => c,
        Err(e) => {
            return (
                None,
                Check::new("node", Status::Fail, format!("http client: {e}")),
            )
        }
    };
    let url = format!("http://{api_addr}/health");
    match client.get(&url).send().await {
        Ok(resp) if resp.status().is_success() => {
            let body: Value = resp.json().await.unwrap_or(Value::Null);
            let version = body["version"].as_str().map(str::to_string);
            match version {
                Some(v) => (
                    Some(RunningNode {
                        version: Some(v.clone()),
                        api_addr,
                    }),
                    Check::new(
                        "node",
                        Status::Ok,
                        format!("node {v} is running at {api_addr}; checking the live listeners"),
                    ),
                ),
                None => (
                    None,
                    Check::new(
                        "node",
                        Status::Fail,
                        format!("{url} answered but is not an Aurix node"),
                    )
                    .hint("another service occupies server.api_port"),
                ),
            }
        }
        Ok(resp) => (
            None,
            Check::new(
                "node",
                Status::Fail,
                format!("{url} answered HTTP {}", resp.status()),
            )
            .hint("something else is listening on server.api_port, or the node is unhealthy"),
        ),
        Err(_) => (
            None,
            Check::new(
                "node",
                Status::Skip,
                format!("no node at {api_addr}; preflight mode (checking that ports are free)"),
            ),
        ),
    }
}

fn listeners(cfg: &AurixConfig) -> Result<Vec<Listener>, Check> {
    let mut out = Vec::new();
    let server = |port: u16| {
        parse_bind_addr(&cfg.server.host, port).map_err(|e| {
            Check::new(
                "ports",
                Status::Fail,
                format!("server.host '{}': {e}", cfg.server.host),
            )
        })
    };
    let media = |port: u16| {
        parse_bind_addr(&cfg.media.host, port).map_err(|e| {
            Check::new(
                "ports",
                Status::Fail,
                format!("media.host '{}': {e}", cfg.media.host),
            )
        })
    };
    out.push(Listener {
        id: "port.api",
        proto: Proto::Tcp,
        addr: server(cfg.server.api_port)?,
        role: "REST API",
    });
    out.push(Listener {
        id: "port.ws",
        proto: Proto::Tcp,
        addr: server(cfg.server.ws_port)?,
        role: "WebSocket signalling",
    });
    if cfg.metrics.enabled {
        out.push(Listener {
            id: "port.metrics",
            proto: Proto::Tcp,
            addr: server(cfg.metrics.port)?,
            role: "Prometheus metrics",
        });
    }
    let media_addr = media(cfg.media.port)?;
    out.push(Listener {
        id: "port.media",
        proto: Proto::Udp,
        addr: media_addr,
        role: "AURX/UDP, QUIC and WebRTC media",
    });
    if cfg.media.cascade_secret.is_some() {
        let cascade = SocketAddr::new(media_addr.ip(), media_addr.port().wrapping_add(1));
        out.push(Listener {
            id: "port.cascade",
            proto: Proto::Udp,
            addr: cascade,
            role: "cascade (node-to-node)",
        });
        if cfg.media.cascade_tcp_fallback {
            out.push(Listener {
                id: "port.cascade_tcp",
                proto: Proto::Tcp,
                addr: cascade,
                role: "cascade TCP fallback",
            });
        }
    }
    if cfg.media.tls_tunnel_port != 0 {
        out.push(Listener {
            id: "port.tls_tunnel",
            proto: Proto::Tcp,
            addr: media(cfg.media.tls_tunnel_port)?,
            role: "TLS media tunnel",
        });
    }
    if cfg.media.webtransport_port != 0 {
        out.push(Listener {
            id: "port.webtransport",
            proto: Proto::Udp,
            addr: media(cfg.media.webtransport_port)?,
            role: "WebTransport over HTTP/3",
        });
    }
    if cfg.turn.enabled {
        let turn = |port: u16| {
            parse_bind_addr(&cfg.turn.host, port).map_err(|e| {
                Check::new(
                    "ports",
                    Status::Fail,
                    format!("turn.host '{}': {e}", cfg.turn.host),
                )
            })
        };
        out.push(Listener {
            id: "port.turn_udp",
            proto: Proto::Udp,
            addr: turn(cfg.turn.udp_port)?,
            role: "TURN/UDP",
        });
        out.push(Listener {
            id: "port.turn_tcp",
            proto: Proto::Tcp,
            addr: turn(cfg.turn.tcp_port)?,
            role: "TURN/TCP",
        });
    }
    Ok(out)
}

fn listener_checks(cfg: &AurixConfig, running: bool) -> Vec<Check> {
    let listeners = match listeners(cfg) {
        Ok(l) => l,
        Err(check) => return vec![check],
    };
    let mut checks = Vec::new();
    let mut seen: BTreeSet<(Proto, SocketAddr)> = BTreeSet::new();
    for l in listeners {
        let proto = match l.proto {
            Proto::Tcp => "tcp",
            Proto::Udp => "udp",
        };
        let key = (l.proto, l.addr);
        if !seen.insert(key) {
            checks.push(Check::new(
                l.id,
                Status::Fail,
                format!("{}/{proto} is configured twice ({})", l.addr, l.role),
            ));
            continue;
        }
        let bind_result = match l.proto {
            Proto::Tcp => std::net::TcpListener::bind(l.addr).map(drop),
            Proto::Udp => std::net::UdpSocket::bind(l.addr).map(drop),
        };
        let in_use = matches!(&bind_result, Err(e) if e.kind() == std::io::ErrorKind::AddrInUse);
        let check = match (bind_result, running) {
            (Ok(()), false) => Check::new(
                l.id,
                Status::Ok,
                format!("{}/{proto} free ({})", l.addr, l.role),
            ),
            (Ok(()), true) => Check::new(
                l.id,
                Status::Fail,
                format!(
                    "{}/{proto} is free although a node is running ({} listener did not start)",
                    l.addr, l.role
                ),
            )
            .hint("check the node log for a bind error on this listener"),
            (Err(_), true) if in_use => Check::new(
                l.id,
                Status::Ok,
                format!("{}/{proto} in use ({})", l.addr, l.role),
            ),
            (Err(_), false) if in_use => Check::new(
                l.id,
                Status::Fail,
                format!(
                    "{}/{proto} is already in use by another process ({})",
                    l.addr, l.role
                ),
            )
            .hint("stop the other process or change the port in the configuration"),
            (Err(e), _) => Check::new(
                l.id,
                Status::Fail,
                format!("cannot bind {}/{proto} ({}): {e}", l.addr, l.role),
            )
            .hint(if l.addr.port() < 1024 {
                "ports below 1024 need CAP_NET_BIND_SERVICE or root"
            } else {
                "the configured host may not be an address of this machine"
            }),
        };
        checks.push(check);
    }
    checks
}

impl PartialOrd for Proto {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Proto {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        (*self as u8).cmp(&(*other as u8))
    }
}

// ---------------------------------------------------------------------------------------------
// Certificates

struct LocalCert {
    fingerprint: String,
    not_after: SystemTime,
    not_before: SystemTime,
    names: Vec<String>,
    chain_len: usize,
}

fn read_cert_chain(path: &Path) -> anyhow::Result<Vec<CertificateDer<'static>>> {
    let pem = std::fs::read(path)?;
    let certs = CertificateDer::pem_slice_iter(&pem)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| anyhow::anyhow!("not a PEM certificate: {e}"))?;
    if certs.is_empty() {
        anyhow::bail!("no CERTIFICATE block found");
    }
    Ok(certs)
}

fn parse_leaf(chain: &[CertificateDer<'static>]) -> anyhow::Result<LocalCert> {
    let leaf = chain
        .first()
        .ok_or_else(|| anyhow::anyhow!("empty chain"))?;
    let (_, parsed) = x509_parser::parse_x509_certificate(leaf.as_ref())
        .map_err(|e| anyhow::anyhow!("cannot parse certificate: {e}"))?;
    let to_time = |t: x509_parser::time::ASN1Time| {
        let secs = t.timestamp();
        if secs < 0 {
            UNIX_EPOCH
        } else {
            UNIX_EPOCH + Duration::from_secs(secs as u64)
        }
    };
    let mut names = Vec::new();
    if let Ok(Some(san)) = parsed.subject_alternative_name() {
        for name in &san.value.general_names {
            match name {
                x509_parser::extensions::GeneralName::DNSName(dns) => {
                    names.push(dns.to_string());
                }
                x509_parser::extensions::GeneralName::IPAddress(bytes) => match bytes.len() {
                    4 => names
                        .push(Ipv4Addr::new(bytes[0], bytes[1], bytes[2], bytes[3]).to_string()),
                    16 => {
                        let mut octets = [0u8; 16];
                        octets.copy_from_slice(bytes);
                        names.push(Ipv6Addr::from(octets).to_string());
                    }
                    _ => {}
                },
                _ => {}
            }
        }
    }
    for cn in parsed.subject().iter_common_name() {
        if let Ok(cn) = cn.as_str() {
            if !names.iter().any(|n| n == cn) {
                names.push(cn.to_string());
            }
        }
    }
    Ok(LocalCert {
        fingerprint: cert_fingerprint(leaf.as_ref()),
        not_after: to_time(parsed.validity().not_after),
        not_before: to_time(parsed.validity().not_before),
        names,
        chain_len: chain.len(),
    })
}

fn key_file_check(path: &Path) -> Option<String> {
    let meta = std::fs::metadata(path).ok()?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = meta.permissions().mode() & 0o777;
        if mode & 0o044 != 0 {
            return Some(format!(
                "private key {} is readable by group/others (mode {mode:o}); chmod 600",
                path.display()
            ));
        }
    }
    #[cfg(not(unix))]
    let _ = meta;
    None
}

fn name_matches(names: &[String], server_name: &str) -> bool {
    let want = server_name
        .trim()
        .trim_end_matches('.')
        .to_ascii_lowercase();
    names.iter().any(|n| {
        let n = n.trim_end_matches('.').to_ascii_lowercase();
        if n == want {
            return true;
        }
        match n.strip_prefix("*.") {
            Some(suffix) => want.split_once('.').is_some_and(|(_, rest)| rest == suffix),
            None => false,
        }
    })
}

fn describe_cert(
    id: &str,
    cert_path: &Path,
    key_path: Option<&Path>,
    expected_name: Option<&str>,
    max_validity: Option<Duration>,
) -> Check {
    let chain = match read_cert_chain(cert_path) {
        Ok(c) => c,
        Err(e) => return Check::new(id, Status::Fail, format!("{}: {e}", cert_path.display())),
    };
    let cert = match parse_leaf(&chain) {
        Ok(c) => c,
        Err(e) => return Check::new(id, Status::Fail, format!("{}: {e}", cert_path.display())),
    };
    let mut status = Status::Ok;
    let mut hints = Vec::new();
    let now = SystemTime::now();
    let remaining = cert.not_after.duration_since(now).ok();
    match remaining {
        None => {
            status = Status::Fail;
            hints.push("certificate has expired".to_string());
        }
        Some(left) if left < CERT_EXPIRY_WARN && max_validity.is_none() => {
            status = Status::Warn;
            hints.push(format!(
                "certificate expires in {} days",
                left.as_secs() / 86_400
            ));
        }
        Some(_) => {}
    }
    if now < cert.not_before {
        status = Status::Fail;
        hints.push("certificate is not valid yet (clock skew?)".to_string());
    }
    if let Some(limit) = max_validity {
        if let Ok(total) = cert.not_after.duration_since(cert.not_before) {
            if total > limit {
                status = status.max(Status::Warn);
                hints.push(format!(
                    "validity {} days exceeds the {} days browsers accept for \
                     serverCertificateHashes; the node generates its own short-lived certificate \
                     when webtransport_cert_path is unset",
                    total.as_secs() / 86_400,
                    limit.as_secs() / 86_400
                ));
            }
        }
    }
    if let Some(name) = expected_name {
        if !name_matches(&cert.names, name) {
            status = status.max(Status::Warn);
            hints.push(format!(
                "server name '{name}' is not among the certificate names {:?}; native clients \
                 pin the fingerprint so this only matters for non-pinning TLS clients",
                cert.names
            ));
        }
    }
    if let Some(key) = key_path {
        match std::fs::read(key) {
            Ok(pem) => {
                if rustls::pki_types::PrivateKeyDer::from_pem_slice(&pem).is_err() {
                    status = Status::Fail;
                    hints.push(format!("{} is not a PEM private key", key.display()));
                }
                if let Some(h) = key_file_check(key) {
                    status = status.max(Status::Warn);
                    hints.push(h);
                }
            }
            Err(e) => {
                status = Status::Fail;
                hints.push(format!("private key {}: {e}", key.display()));
            }
        }
    }
    let mut check = Check::new(
        id,
        status,
        format!(
            "{} sha256={} expires {}",
            cert_path.display(),
            &cert.fingerprint[..16],
            format_time(cert.not_after)
        ),
    );
    check.hints = hints;
    check.details = json!({
        "path": cert_path.display().to_string(),
        "sha256": cert.fingerprint,
        "not_before": format_time(cert.not_before),
        "not_after": format_time(cert.not_after),
        "names": cert.names,
        "chain_len": cert.chain_len,
    });
    check
}

fn format_time(t: SystemTime) -> String {
    let secs = t
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    chrono::DateTime::<chrono::Utc>::from_timestamp(secs, 0)
        .map(|d| d.format("%Y-%m-%dT%H:%M:%SZ").to_string())
        .unwrap_or_else(|| secs.to_string())
}

/// The certificate whose fingerprint native clients pin (QUIC and the TLS tunnel share it).
fn media_cert_paths(cfg: &AurixConfig) -> Option<(PathBuf, PathBuf)> {
    Some((
        cfg.media.quic_cert_path.clone()?,
        cfg.media.quic_key_path.clone()?,
    ))
}

fn local_media_fingerprint(cfg: &AurixConfig) -> Option<String> {
    let (cert, _) = media_cert_paths(cfg)?;
    read_cert_chain(&cert)
        .ok()
        .and_then(|chain| chain.first().map(|c| cert_fingerprint(c.as_ref())))
}

fn certificate_checks(cfg: &AurixConfig) -> Vec<Check> {
    let mut checks = Vec::new();
    let media_wanted = cfg.media.quic || cfg.media.tls_tunnel_port != 0;
    match media_cert_paths(cfg) {
        Some((cert, key)) => checks.push(describe_cert(
            "cert.media",
            &cert,
            Some(&key),
            Some(&cfg.media.quic_server_name),
            None,
        )),
        None if media_wanted => checks.push(
            Check::new(
                "cert.media",
                Status::Ok,
                "no media.quic_cert_path: the node generates a self-signed certificate at start \
                 (fingerprint changes on every restart)",
            )
            .hint(
                "set media.quic_cert_path/quic_key_path for a stable pin across restarts and \
                 multi-node fleets",
            ),
        ),
        None => checks.push(Check::new(
            "cert.media",
            Status::Skip,
            "QUIC and the TLS tunnel are disabled",
        )),
    }
    if cfg.media.webtransport_port != 0 {
        match (
            &cfg.media.webtransport_cert_path,
            &cfg.media.webtransport_key_path,
        ) {
            (Some(cert), Some(key)) => checks.push(describe_cert(
                "cert.webtransport",
                cert,
                Some(key),
                None,
                Some(WEBTRANSPORT_MAX_VALIDITY),
            )),
            (None, None) => checks.push(Check::new(
                "cert.webtransport",
                Status::Ok,
                "short-lived ECDSA certificate generated and rotated by the node",
            )),
            _ => checks.push(Check::new(
                "cert.webtransport",
                Status::Fail,
                "media.webtransport_cert_path and webtransport_key_path must be set together",
            )),
        }
    }
    match (&cfg.server.tls_cert_path, &cfg.server.tls_key_path) {
        (Some(cert), Some(key)) => checks.push(describe_cert(
            "cert.server",
            Path::new(cert),
            Some(Path::new(key)),
            None,
            None,
        )),
        (None, None) => {}
        _ => checks.push(Check::new(
            "cert.server",
            Status::Fail,
            "server.tls_cert_path and server.tls_key_path must be set together",
        )),
    }
    checks
}

// ---------------------------------------------------------------------------------------------
// Endpoint probes (running node only)

/// Accepts any server certificate: probes compare the presented fingerprint with the one on
/// disk afterwards, which is the property operators care about.
#[derive(Debug)]
struct AnyServerVerifier(Arc<rustls::crypto::CryptoProvider>);

impl rustls::client::danger::ServerCertVerifier for AnyServerVerifier {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: rustls::pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Err(rustls::Error::PeerIncompatible(
            rustls::PeerIncompatible::Tls12NotOffered,
        ))
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &self.0.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        self.0.signature_verification_algorithms.supported_schemes()
    }
}

fn probe_tls_config(alpn: &[u8]) -> anyhow::Result<Arc<rustls::ClientConfig>> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let mut tls = rustls::ClientConfig::builder_with_provider(provider.clone())
        .with_protocol_versions(&[&rustls::version::TLS13])?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(AnyServerVerifier(provider)))
        .with_no_client_auth();
    tls.alpn_protocols = vec![alpn.to_vec()];
    Ok(Arc::new(tls))
}

/// What a handshake told us about the far end.
struct Handshake {
    alpn: Option<Vec<u8>>,
    leaf: Option<CertificateDer<'static>>,
    rtt: Duration,
}

async fn resolve_endpoint(
    endpoint: &str,
    timeout: Duration,
) -> anyhow::Result<(String, SocketAddr)> {
    let (host, port) = aurix_common::addr::split_host_port(endpoint)
        .ok_or_else(|| anyhow::anyhow!("'{endpoint}' is not host:port"))?;
    let host = host.to_string();
    let addr = tokio::time::timeout(timeout, tokio::net::lookup_host((host.as_str(), port)))
        .await
        .map_err(|_| anyhow::anyhow!("DNS lookup of {host} timed out"))??
        .next()
        .ok_or_else(|| anyhow::anyhow!("{host} did not resolve"))?;
    Ok((host, addr))
}

async fn quic_handshake(
    addr: SocketAddr,
    server_name: &str,
    alpn: &[u8],
    timeout: Duration,
) -> anyhow::Result<Handshake> {
    let tls = probe_tls_config(alpn)?;
    let client_cfg = aurix_common::quic::client_config(tls, timeout, 8, None)
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    let local: SocketAddr = if addr.is_ipv4() {
        (Ipv4Addr::UNSPECIFIED, 0).into()
    } else {
        (Ipv6Addr::UNSPECIFIED, 0).into()
    };
    let socket = std::net::UdpSocket::bind(local)?;
    let endpoint = quinn::Endpoint::new(
        aurix_common::quic::endpoint_config(),
        None,
        socket,
        Arc::new(quinn::TokioRuntime),
    )?;
    let started = Instant::now();
    let name = ServerName::try_from(server_name.to_string())
        .map(|_| server_name.to_string())
        .unwrap_or_else(|_| "localhost".to_string());
    let connecting = endpoint.connect_with(client_cfg, addr, &name)?;
    let conn = tokio::time::timeout(timeout, connecting)
        .await
        .map_err(|_| anyhow::anyhow!("no QUIC answer from {addr} within {timeout:?}"))??;
    let rtt = started.elapsed();
    let alpn = conn
        .handshake_data()
        .and_then(|d| d.downcast::<quinn::crypto::rustls::HandshakeData>().ok())
        .and_then(|d| d.protocol);
    let leaf = conn
        .peer_identity()
        .and_then(|id| id.downcast::<Vec<CertificateDer<'static>>>().ok())
        .and_then(|certs| certs.first().cloned());
    conn.close(0u32.into(), b"doctor");
    endpoint.close(0u32.into(), b"doctor");
    Ok(Handshake { alpn, leaf, rtt })
}

async fn tls_handshake(
    addr: SocketAddr,
    server_name: &str,
    alpn: &[u8],
    timeout: Duration,
) -> anyhow::Result<Handshake> {
    let tls = probe_tls_config(alpn)?;
    let started = Instant::now();
    let tcp = tokio::time::timeout(timeout, tokio::net::TcpStream::connect(addr))
        .await
        .map_err(|_| anyhow::anyhow!("TCP connect to {addr} timed out"))??;
    let name = ServerName::try_from(server_name.to_string())
        .unwrap_or_else(|_| ServerName::IpAddress(addr.ip().into()));
    let connector = tokio_rustls::TlsConnector::from(tls);
    let stream = tokio::time::timeout(timeout, connector.connect(name, tcp))
        .await
        .map_err(|_| anyhow::anyhow!("TLS handshake with {addr} timed out"))??;
    let rtt = started.elapsed();
    let (_, session) = stream.get_ref();
    let alpn = session.alpn_protocol().map(<[u8]>::to_vec);
    let leaf = session.peer_certificates().and_then(|c| c.first().cloned());
    Ok(Handshake { alpn, leaf, rtt })
}

fn pin_verdict(local: Option<&str>, served: Option<&CertificateDer<'static>>) -> (Status, String) {
    let served_fp = served.map(|c| cert_fingerprint(c.as_ref()));
    match (local, served_fp) {
        (_, None) => (Status::Fail, "server sent no certificate".to_string()),
        (None, Some(fp)) => (
            Status::Ok,
            format!(
                "served sha256={} (no certificate file configured to compare)",
                &fp[..16]
            ),
        ),
        (Some(l), Some(fp)) if l == fp => (
            Status::Ok,
            format!(
                "served certificate matches the file on disk (sha256={})",
                &fp[..16]
            ),
        ),
        (Some(l), Some(fp)) => (
            Status::Fail,
            format!(
                "served sha256={} differs from the file on disk sha256={}: the node is still \
                 running an older certificate (restart it) or another process answers here",
                &fp[..16],
                &l[..16]
            ),
        ),
    }
}

fn alpn_verdict(expected: &[u8], got: Option<&[u8]>) -> Option<String> {
    match got {
        Some(p) if p == expected => None,
        Some(p) => Some(format!(
            "negotiated ALPN {:?} instead of {:?}",
            String::from_utf8_lossy(p),
            String::from_utf8_lossy(expected)
        )),
        None => Some(format!(
            "server did not negotiate ALPN {:?}",
            String::from_utf8_lossy(expected)
        )),
    }
}

/// QUIC handshake against the media port: proves UDP reaches the node and that the served
/// certificate is the one on disk (the fingerprint the node advertises to clients).
async fn probe_quic(cfg: &AurixConfig, timeout: Duration) -> Check {
    if !cfg.media.quic {
        return Check::new("probe.quic", Status::Skip, "media.quic is disabled");
    }
    let bind = match parse_bind_addr(&cfg.media.host, cfg.media.port) {
        Ok(b) => b,
        Err(e) => return Check::new("probe.quic", Status::Fail, format!("media.host: {e}")),
    };
    let addr = connect_addr(bind);
    match quic_handshake(addr, &cfg.media.quic_server_name, QUIC_ALPN, timeout).await {
        Ok(hs) => {
            let local = local_media_fingerprint(cfg);
            let (mut status, pin) = pin_verdict(local.as_deref(), hs.leaf.as_ref());
            let mut check = Check::new(
                "probe.quic",
                Status::Ok,
                format!(
                    "QUIC over UDP {addr} answered in {} ms; {pin}",
                    hs.rtt.as_millis()
                ),
            );
            if let Some(problem) = alpn_verdict(QUIC_ALPN, hs.alpn.as_deref()) {
                status = Status::Fail;
                check = check.hint(problem);
            }
            check.status = status;
            check.details = json!({
                "addr": addr.to_string(),
                "rtt_ms": hs.rtt.as_millis() as u64,
                "served_sha256": hs.leaf.as_ref().map(|c| cert_fingerprint(c.as_ref())),
                "local_sha256": local,
            });
            check
        }
        Err(e) => Check::new(
            "probe.quic",
            Status::Fail,
            format!("QUIC handshake with {addr} failed: {e}"),
        )
        .hint("UDP to media.port is blocked or the node's QUIC listener is not up"),
    }
}

async fn probe_tls_tunnel(cfg: &AurixConfig, timeout: Duration) -> Vec<Check> {
    if cfg.media.tls_tunnel_port == 0 {
        return Vec::new();
    }
    let mut targets = cfg.media.tls_tunnel_endpoints();
    if let Ok(bind) = parse_bind_addr(&cfg.media.host, cfg.media.tls_tunnel_port) {
        targets.insert(0, connect_addr(bind).to_string());
    }
    targets.dedup();
    let local = local_media_fingerprint(cfg);
    let mut checks = Vec::new();
    for (i, target) in targets.iter().enumerate() {
        let id = if i == 0 {
            "probe.tls_tunnel".to_string()
        } else {
            format!("probe.tls_tunnel.{i}")
        };
        let (host, addr) = match resolve_endpoint(target, timeout).await {
            Ok(r) => r,
            Err(e) => {
                checks.push(Check::new(id, Status::Fail, format!("{target}: {e}")));
                continue;
            }
        };
        let name = if host.parse::<IpAddr>().is_ok() {
            cfg.media.quic_server_name.clone()
        } else {
            host.clone()
        };
        match tls_handshake(addr, &name, TLS_TUNNEL_ALPN, timeout).await {
            Ok(hs) => {
                let (mut status, pin) = pin_verdict(local.as_deref(), hs.leaf.as_ref());
                let mut check = Check::new(
                    id,
                    Status::Ok,
                    format!(
                        "TLS tunnel {target} answered in {} ms; {pin}",
                        hs.rtt.as_millis()
                    ),
                );
                if let Some(problem) = alpn_verdict(TLS_TUNNEL_ALPN, hs.alpn.as_deref()) {
                    status = Status::Fail;
                    check = check.hint(problem);
                }
                check.status = status;
                check.details = json!({
                    "endpoint": target,
                    "addr": addr.to_string(),
                    "rtt_ms": hs.rtt.as_millis() as u64,
                    "served_sha256": hs.leaf.as_ref().map(|c| cert_fingerprint(c.as_ref())),
                    "local_sha256": local,
                });
                checks.push(check);
            }
            Err(e) => checks.push(
                Check::new(id, Status::Fail, format!("TLS tunnel {target}: {e}")).hint(
                    "advertised tls_tunnel endpoints must be reachable from the internet on TCP",
                ),
            ),
        }
    }
    checks
}

async fn probe_webtransport(cfg: &AurixConfig, timeout: Duration) -> Vec<Check> {
    if cfg.media.webtransport_port == 0 {
        return Vec::new();
    }
    let mut targets = cfg.media.webtransport_endpoints();
    if let Ok(bind) = parse_bind_addr(&cfg.media.host, cfg.media.webtransport_port) {
        targets.insert(0, connect_addr(bind).to_string());
    }
    targets.dedup();
    let mut checks = Vec::new();
    for (i, target) in targets.iter().enumerate() {
        let id = if i == 0 {
            "probe.webtransport".to_string()
        } else {
            format!("probe.webtransport.{i}")
        };
        let (host, addr) = match resolve_endpoint(target, timeout).await {
            Ok(r) => r,
            Err(e) => {
                checks.push(Check::new(id, Status::Fail, format!("{target}: {e}")));
                continue;
            }
        };
        match quic_handshake(addr, &host, WEBTRANSPORT_ALPN, timeout).await {
            Ok(hs) => {
                let mut status = Status::Ok;
                let mut hints = Vec::new();
                if let Some(problem) = alpn_verdict(WEBTRANSPORT_ALPN, hs.alpn.as_deref()) {
                    status = Status::Fail;
                    hints.push(problem);
                }
                let mut served = None;
                match hs.leaf.as_ref() {
                    None => {
                        status = Status::Fail;
                        hints.push("server sent no certificate".to_string());
                    }
                    Some(leaf) => {
                        served = Some(cert_fingerprint(leaf.as_ref()));
                        if let Ok(parsed) = parse_leaf(std::slice::from_ref(leaf)) {
                            let total = parsed
                                .not_after
                                .duration_since(parsed.not_before)
                                .unwrap_or_default();
                            if total > WEBTRANSPORT_MAX_VALIDITY {
                                status = Status::Fail;
                                hints.push(format!(
                                    "served certificate is valid for {} days; browsers accept \
                                     serverCertificateHashes only up to 14 days",
                                    total.as_secs() / 86_400
                                ));
                            }
                            if SystemTime::now() > parsed.not_after {
                                status = Status::Fail;
                                hints.push("served certificate has expired".to_string());
                            }
                        }
                    }
                }
                let mut check = Check::new(
                    id,
                    status,
                    format!(
                        "WebTransport (HTTP/3) {target} answered in {} ms{}",
                        hs.rtt.as_millis(),
                        served
                            .as_deref()
                            .map(|fp| format!("; served sha256={}", &fp[..16]))
                            .unwrap_or_default()
                    ),
                );
                check.hints = hints;
                check.details = json!({
                    "endpoint": target,
                    "addr": addr.to_string(),
                    "rtt_ms": hs.rtt.as_millis() as u64,
                    "served_sha256": served,
                });
                checks.push(check);
            }
            Err(e) => checks.push(
                Check::new(id, Status::Fail, format!("WebTransport {target}: {e}")).hint(
                    "advertised webtransport endpoints must be reachable from the internet on UDP",
                ),
            ),
        }
    }
    checks
}

// ---------------------------------------------------------------------------------------------
// Redis / PostgreSQL

async fn check_redis(cfg: &AurixConfig, timeout: Duration) -> Check {
    let configured_mode = if !cfg.redis.cluster.is_empty() {
        "cluster"
    } else if !cfg.redis.sentinels.is_empty() {
        "sentinel"
    } else {
        "direct"
    };
    let opened = tokio::time::timeout(
        timeout.max(Duration::from_secs(6)),
        RedisSource::open(&cfg.redis),
    )
    .await;
    match opened {
        Ok(Ok(source)) => {
            let mode = if source.is_cluster() {
                "cluster"
            } else if source.is_sentinel() {
                "sentinel"
            } else {
                "direct"
            };
            let mut check = Check::new(
                "redis",
                Status::Ok,
                format!("PING ok; mode={mode} ({})", source.describe()),
            );
            if mode == "direct" && cfg.is_production() {
                check.status = Status::Warn;
                check = check.hint(
                    "a single Redis is a single point of failure for the fleet; see the HA \
                     chapter for Sentinel or Cluster",
                );
            }
            if source.is_cluster() && !source.sharded_pubsub() {
                check = check.hint(
                    "redis.sharded_pubsub=false: cross-node events use classic Pub/Sub, which \
                     Redis Cluster broadcasts to every node",
                );
            }
            check.details = json!({
                "mode": mode,
                "configured_mode": configured_mode,
                "endpoint": source.describe(),
                "pool_size": cfg.redis.pool_size,
                "sentinels": cfg.redis.sentinels.iter().map(|s| redact_url_credentials(s)).collect::<Vec<_>>(),
                "sharded_pubsub": source.sharded_pubsub(),
            });
            check
        }
        Ok(Err(e)) => {
            let mut check = Check::new(
                "redis",
                Status::Fail,
                format!(
                    "mode={configured_mode}: {}",
                    redact_url_credentials(&e.to_string())
                ),
            )
            .hint("the node refuses to start without Redis");
            check = match configured_mode {
                "cluster" => check.hint(
                    "CLUSTER INFO on a seed must report cluster_state:ok and every seed in \
                     redis.cluster must be reachable from this host",
                ),
                "sentinel" => check.hint(
                    "SENTINEL get-master-addr-by-name must know redis.sentinel_master, and the \
                     master address it returns must be reachable from this host",
                ),
                _ => check,
            };
            check
        }
        Err(_) => Check::new(
            "redis",
            Status::Fail,
            format!("mode={configured_mode}: no answer within {timeout:?}"),
        ),
    }
}

async fn check_postgres(cfg: &AurixConfig, timeout: Duration) -> Vec<Check> {
    let redacted = redact_url_credentials(&cfg.database.url);
    let connected = tokio::time::timeout(timeout, PgConnection::connect(&cfg.database.url)).await;
    let mut conn = match connected {
        Ok(Ok(c)) => c,
        Ok(Err(e)) => {
            return vec![
                Check::new(
                    "postgres",
                    Status::Fail,
                    format!("{redacted}: {}", redact_url_credentials(&e.to_string())),
                )
                .hint("the node refuses to start without PostgreSQL"),
                Check::new("migrations", Status::Skip, "PostgreSQL unreachable"),
            ]
        }
        Err(_) => {
            return vec![
                Check::new(
                    "postgres",
                    Status::Fail,
                    format!("{redacted}: no answer within {timeout:?}"),
                ),
                Check::new("migrations", Status::Skip, "PostgreSQL unreachable"),
            ]
        }
    };
    let mut checks = Vec::new();
    let version: Result<(String,), _> = sqlx::query_as("SELECT version()")
        .fetch_one(&mut conn)
        .await;
    let version = match version {
        Ok((v,)) => v,
        Err(e) => {
            checks.push(Check::new(
                "postgres",
                Status::Fail,
                format!("{redacted}: SELECT version() failed: {e}"),
            ));
            checks.push(Check::new(
                "migrations",
                Status::Skip,
                "PostgreSQL query failed",
            ));
            return checks;
        }
    };
    let short_version = version
        .split_whitespace()
        .take(2)
        .collect::<Vec<_>>()
        .join(" ");
    let major: Option<u32> = version
        .split_whitespace()
        .nth(1)
        .and_then(|v| v.split('.').next())
        .and_then(|m| m.parse().ok());
    let mut pg = Check::new(
        "postgres",
        Status::Ok,
        format!("{redacted}: connected ({short_version})"),
    );
    if let Some(m) = major {
        if m < 14 {
            pg.status = Status::Warn;
            pg = pg.hint("Aurix is tested on PostgreSQL 14+");
        }
    }
    pg.details = json!({
        "url": redacted,
        "server_version": short_version,
        "max_connections": cfg.database.max_connections,
    });
    checks.push(pg);
    checks.push(migrations_check(cfg, migration_report(&mut conn).await));
    let _ = conn.close().await;
    checks
}

fn migrations_check(cfg: &AurixConfig, report: Result<MigrationReport, sqlx::Error>) -> Check {
    let report = match report {
        Ok(r) => r,
        Err(e) => {
            return Check::new(
                "migrations",
                Status::Fail,
                format!("cannot read _sqlx_migrations: {e}"),
            )
        }
    };
    let embedded = MigrationReport::embedded_latest();
    let applied_latest = report.applied.last().copied().unwrap_or(0);
    let mut status = Status::Ok;
    let mut hints = Vec::new();
    if !report.failed.is_empty() {
        status = Status::Fail;
        hints.push(format!(
            "migrations {:?} are recorded as failed; the node will not start until the rows are \
             repaired (see the backup/restore chapter)",
            report.failed
        ));
    }
    if !report.checksum_mismatch.is_empty() {
        status = Status::Fail;
        hints.push(format!(
            "migrations {:?} were edited after being applied (checksum mismatch); sqlx refuses \
             to run further migrations",
            report.checksum_mismatch
        ));
    }
    if !report.unknown.is_empty() {
        status = status.max(Status::Warn);
        hints.push(format!(
            "database has migrations {:?} unknown to this binary: it was migrated by a newer \
             node; do not start an older node against it",
            report.unknown
        ));
    }
    if !report.pending.is_empty() {
        let list: Vec<String> = report
            .pending
            .iter()
            .map(|(v, d)| format!("{v} {d}"))
            .collect();
        if cfg.database.run_migrations {
            status = status.max(Status::Warn);
            hints.push(format!(
                "{} pending migration(s) will run at node start-up: {}; take a backup first",
                list.len(),
                list.join(", ")
            ));
        } else {
            status = Status::Fail;
            hints.push(format!(
                "{} pending migration(s) and database.run_migrations=false: {}; run \
                 `aurix-server --migrate-only`",
                list.len(),
                list.join(", ")
            ));
        }
    }
    let summary = if report.applied.is_empty() {
        format!(
            "database is empty; {} embedded migrations pending (latest {embedded})",
            report.pending.len()
        )
    } else if report.is_current() {
        format!(
            "current: {} applied, latest {applied_latest} = embedded {embedded}",
            report.applied.len()
        )
    } else {
        format!(
            "{} applied (latest {applied_latest}), embedded latest {embedded}, {} pending",
            report.applied.len(),
            report.pending.len()
        )
    };
    let mut check = Check::new("migrations", status, summary);
    check.hints = hints;
    check.details = serde_json::to_value(&report).unwrap_or(Value::Null);
    if let Value::Object(map) = &mut check.details {
        map.insert("embedded_latest".into(), json!(embedded));
        map.insert("run_migrations".into(), json!(cfg.database.run_migrations));
    }
    check
}

// ---------------------------------------------------------------------------------------------
// Redaction

/// Every credential the configuration holds, so no error message or endpoint description can
/// echo one back.
#[derive(Default)]
struct Redactor {
    secrets: Vec<String>,
}

impl Redactor {
    fn new(cfg: &AurixConfig) -> Self {
        let mut secrets = Vec::new();
        if let Ok(value) = serde_json::to_value(cfg) {
            collect_secrets(&value, None, &mut secrets);
        }
        secrets.retain(|s| s.len() >= 8);
        secrets.sort_by_key(|s| std::cmp::Reverse(s.len()));
        secrets.dedup();
        Self { secrets }
    }

    fn scrub(&self, s: &str) -> String {
        let mut out = redact_url_credentials(s);
        for secret in &self.secrets {
            if out.contains(secret.as_str()) {
                out = out.replace(secret.as_str(), "***");
            }
        }
        out
    }

    fn scrub_value(&self, v: &mut Value) {
        match v {
            Value::String(s) => *s = self.scrub(s),
            Value::Array(items) => items.iter_mut().for_each(|i| self.scrub_value(i)),
            Value::Object(map) => map.values_mut().for_each(|i| self.scrub_value(i)),
            _ => {}
        }
    }
}

fn collect_secrets(v: &Value, key: Option<&str>, out: &mut Vec<String>) {
    match v {
        Value::String(s) => {
            if key.is_some_and(is_secret_key) {
                out.push(s.clone());
            }
            if let Some(password) = url_password(s) {
                out.push(password);
            }
        }
        Value::Array(items) => items.iter().for_each(|i| collect_secrets(i, key, out)),
        Value::Object(map) => map
            .iter()
            .for_each(|(k, i)| collect_secrets(i, Some(k), out)),
        _ => {}
    }
}

fn url_password(s: &str) -> Option<String> {
    let scheme_end = s.find("://")?;
    let authority = &s[scheme_end + 3..];
    let authority = &authority[..authority.find(['/', '?', '#']).unwrap_or(authority.len())];
    let at = authority.rfind('@')?;
    let userinfo = &authority[..at];
    let colon = userinfo.find(':')?;
    let password = &userinfo[colon + 1..];
    (!password.is_empty()).then(|| password.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn url_password_is_extracted_and_scrubbed() {
        assert_eq!(
            url_password("postgres://aurix:s3cret-pw@db:5432/aurix").as_deref(),
            Some("s3cret-pw")
        );
        assert_eq!(url_password("redis://db:6379"), None);
        let r = Redactor {
            secrets: vec!["s3cret-pw".into(), "jwt-topsecret".into()],
        };
        assert_eq!(
            r.scrub("error connecting with s3cret-pw and jwt-topsecret"),
            "error connecting with *** and ***"
        );
        assert_eq!(
            r.scrub("postgres://aurix:other@db/aurix"),
            "postgres://aurix:***@db/aurix"
        );
    }

    #[test]
    fn names_match_exact_and_wildcard() {
        let names = vec!["voice.example".to_string(), "*.eu.example".to_string()];
        assert!(name_matches(&names, "voice.example"));
        assert!(name_matches(&names, "node1.eu.example"));
        assert!(!name_matches(&names, "a.b.eu.example"));
        assert!(!name_matches(&names, "other.example"));
    }

    #[test]
    fn pin_verdict_distinguishes_match_and_drift() {
        let cert = CertificateDer::from(vec![1u8, 2, 3]);
        let fp = cert_fingerprint(cert.as_ref());
        assert_eq!(pin_verdict(Some(&fp), Some(&cert)).0, Status::Ok);
        let other = "0".repeat(64);
        assert_eq!(pin_verdict(Some(&other), Some(&cert)).0, Status::Fail);
        assert_eq!(pin_verdict(None, Some(&cert)).0, Status::Ok);
        assert_eq!(pin_verdict(Some(&fp), None).0, Status::Fail);
    }

    #[test]
    fn connect_addr_maps_wildcards_to_loopback() {
        assert_eq!(
            connect_addr("0.0.0.0:8080".parse().unwrap()),
            "127.0.0.1:8080".parse::<SocketAddr>().unwrap()
        );
        assert_eq!(
            connect_addr("[::]:8080".parse().unwrap()),
            "[::1]:8080".parse::<SocketAddr>().unwrap()
        );
        assert_eq!(
            connect_addr("10.0.0.5:1".parse().unwrap()),
            "10.0.0.5:1".parse::<SocketAddr>().unwrap()
        );
    }
}
