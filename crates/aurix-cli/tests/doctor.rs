//! `aurix doctor` against a temporary node configuration: preflight vs running-node listener
//! semantics, certificate checks, exit codes and — above all — that no credential from the
//! configuration ever reaches stdout/stderr, even inside connection errors.

use std::net::{SocketAddr, TcpListener};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use axum::routing::get;
use axum::{Json, Router};
use serde_json::{json, Value};

const JWT_SECRET: &str = "doctor-test-jwt-secret-value-that-must-never-print-0123456789";
const DB_PASSWORD: &str = "pg-s3cret-DoctorTestPassword";
const REDIS_PASSWORD: &str = "redis-s3cret-DoctorTestPassword";
const CASCADE_SECRET: &str = "cascade-s3cret-DoctorTestSharedKey";

struct Ports {
    api: u16,
    ws: u16,
    metrics: u16,
    media: u16,
    db: u16,
    redis: u16,
}

/// The node layers a configuration file over `configs/default.toml` in its working directory;
/// doctor does the same, so it runs from the workspace root here.
fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

impl Ports {
    fn pick() -> Ports {
        Ports {
            api: free_port(),
            ws: free_port(),
            metrics: free_port(),
            media: free_port(),
            db: free_port(),
            redis: free_port(),
        }
    }
}

struct Env {
    dir: PathBuf,
    ports: Ports,
}

impl Env {
    fn new(name: &str) -> Env {
        let dir =
            std::env::temp_dir().join(format!("aurix-doctor-test-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Env {
            dir,
            ports: Ports::pick(),
        }
    }

    fn write(&self, name: &str, contents: &str) -> PathBuf {
        let p = self.dir.join(name);
        std::fs::write(&p, contents).unwrap();
        p
    }

    /// A node configuration whose dependencies point at closed local ports.
    fn config(&self, media_extra: &str) -> PathBuf {
        let p = &self.ports;
        self.write(
            "node.toml",
            &format!(
                r#"
[server]
host = "127.0.0.1"
api_port = {api}
ws_port = {ws}
[metrics]
port = {metrics}
[auth]
jwt_secret = "{JWT_SECRET}"
[database]
url = "postgres://aurix:{DB_PASSWORD}@127.0.0.1:{db}/aurix"
[redis]
url = "redis://:{REDIS_PASSWORD}@127.0.0.1:{redis}"
[media]
host = "127.0.0.1"
port = {media}
cascade_secret = "{CASCADE_SECRET}"
{media_extra}
"#,
                api = p.api,
                ws = p.ws,
                metrics = p.metrics,
                db = p.db,
                redis = p.redis,
                media = p.media,
            ),
        )
    }

    fn run(&self, config: &Path, args: &[&str]) -> Output {
        let mut c = Command::new(env!("CARGO_BIN_EXE_aurix"));
        c.env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("HOME", &self.dir)
            .env("AURIX_CONFIG", self.dir.join("cli.toml"))
            .current_dir(workspace_root())
            .args(["-o", "json", "doctor", "-c", config.to_str().unwrap()])
            .args(args);
        c.output().unwrap()
    }
}

fn text(o: &Output) -> (String, String) {
    (
        String::from_utf8_lossy(&o.stdout).into_owned(),
        String::from_utf8_lossy(&o.stderr).into_owned(),
    )
}

fn assert_no_secrets(o: &Output) {
    let (out, errs) = text(o);
    for s in [JWT_SECRET, DB_PASSWORD, REDIS_PASSWORD, CASCADE_SECRET] {
        assert!(!out.contains(s), "secret leaked to stdout: {out}");
        assert!(!errs.contains(s), "secret leaked to stderr: {errs}");
    }
}

fn report(o: &Output) -> Value {
    let (out, errs) = text(o);
    serde_json::from_str(&out).unwrap_or_else(|e| panic!("not JSON ({e}): {out}\n{errs}"))
}

fn check<'a>(v: &'a Value, id: &str) -> &'a Value {
    v["checks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["id"] == id)
        .unwrap_or_else(|| panic!("no check {id} in {v}"))
}

fn hints(c: &Value) -> String {
    c["hints"]
        .as_array()
        .map(|h| {
            h.iter()
                .filter_map(Value::as_str)
                .collect::<Vec<_>>()
                .join(" | ")
        })
        .unwrap_or_default()
}

fn self_signed(names: &[&str], not_before: time::OffsetDateTime, days: i64) -> (String, String) {
    let mut params =
        rcgen::CertificateParams::new(names.iter().map(|n| n.to_string()).collect::<Vec<_>>())
            .unwrap();
    params.not_before = not_before;
    params.not_after = not_before + time::Duration::days(days);
    params
        .distinguished_name
        .push(rcgen::DnType::CommonName, names[0]);
    let key = rcgen::KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256).unwrap();
    let cert = params.self_signed(&key).unwrap();
    (cert.pem(), key.serialize_pem())
}

#[test]
fn preflight_reports_free_ports_unreachable_dependencies_and_hides_secrets() {
    let e = Env::new("preflight");
    let cfg = e.config("");
    let o = e.run(&cfg, &["--timeout", "2"]);
    assert_no_secrets(&o);
    assert_eq!(o.status.code(), Some(2), "{:?}", text(&o));
    let v = report(&o);
    assert_eq!(v["ok"], false);
    assert_eq!(v["mode"], "preflight");
    assert_eq!(check(&v, "config")["status"], "ok", "{v}");
    for id in ["port.api", "port.ws", "port.metrics", "port.media"] {
        assert_eq!(check(&v, id)["status"], "ok", "{id}: {v}");
    }
    assert_eq!(check(&v, "node")["status"], "skip");
    assert_eq!(check(&v, "redis")["status"], "fail");
    assert_eq!(check(&v, "postgres")["status"], "fail");
    assert_eq!(check(&v, "migrations")["status"], "skip");
    assert_eq!(check(&v, "probe.quic")["status"], "skip");
    let pg = check(&v, "postgres")["summary"].as_str().unwrap();
    assert!(pg.contains("postgres://aurix:***@127.0.0.1"), "{pg}");

    let o = e.run(&cfg, &["--skip-remote", "--skip-probes"]);
    assert_no_secrets(&o);
    assert_eq!(o.status.code(), Some(0), "{:?}", text(&o));
    let v = report(&o);
    assert_eq!(v["ok"], true);
    assert_eq!(check(&v, "redis")["status"], "skip");
    assert_eq!(check(&v, "postgres")["status"], "skip");

    // Warnings (an unset external IP is one) become failures under --strict.
    let o = e.run(&cfg, &["--skip-remote", "--skip-probes", "--strict"]);
    assert_eq!(o.status.code(), Some(2), "{:?}", text(&o));
    let v = report(&o);
    assert_eq!(check(&v, "endpoints")["status"], "warn", "{v}");
}

#[test]
fn pretty_output_is_human_readable_and_hides_secrets() {
    let e = Env::new("pretty");
    let cfg = e.config("");
    let mut c = Command::new(env!("CARGO_BIN_EXE_aurix"));
    let o = c
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("HOME", &e.dir)
        .env("AURIX_CONFIG", e.dir.join("cli.toml"))
        .current_dir(workspace_root())
        .args(["doctor", "-c", cfg.to_str().unwrap(), "--timeout", "2"])
        .output()
        .unwrap();
    assert_no_secrets(&o);
    assert_eq!(o.status.code(), Some(2), "{:?}", text(&o));
    let (out, _) = text(&o);
    assert!(out.starts_with("aurix doctor "), "{out}");
    assert!(out.contains("  FAIL  redis"), "{out}");
    assert!(out.contains("  FAIL  postgres"), "{out}");
    assert!(out.contains("  OK    port.api"), "{out}");
    assert!(out.lines().last().unwrap().starts_with("FAIL: "), "{out}");
}

#[test]
fn missing_or_invalid_config_is_a_failure() {
    let e = Env::new("config");
    let o = e.run(&e.dir.join("nope.toml"), &[]);
    assert_eq!(o.status.code(), Some(2), "{:?}", text(&o));
    let v = report(&o);
    assert_eq!(check(&v, "config")["status"], "fail");
    assert!(check(&v, "config")["summary"]
        .as_str()
        .unwrap()
        .contains("not found"));

    let partial = e.write("partial.toml", "[server]\napi_port = 18080\n");
    let mut c = Command::new(env!("CARGO_BIN_EXE_aurix"));
    let o = c
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("HOME", &e.dir)
        .env("AURIX_CONFIG", e.dir.join("cli.toml"))
        .current_dir(&e.dir)
        .args(["-o", "json", "doctor", "-c", partial.to_str().unwrap()])
        .output()
        .unwrap();
    assert_eq!(o.status.code(), Some(2), "{:?}", text(&o));
    let v = report(&o);
    let c = check(&v, "config");
    assert_eq!(c["status"], "fail", "{v}");
    assert!(hints(c).contains("configs/default"), "{c}");

    let bad = e.write(
        "bad.toml",
        &format!(
            "[auth]\njwt_secret = \"{JWT_SECRET}\"\n[server]\napi_port = 8080\nws_port = 8080\n"
        ),
    );
    let o = e.run(&bad, &["--skip-remote", "--skip-probes"]);
    assert_no_secrets(&o);
    assert_eq!(o.status.code(), Some(2), "{:?}", text(&o));
    let v = report(&o);
    assert_eq!(check(&v, "config")["status"], "ok", "{v}");
    let ws = check(&v, "port.ws");
    assert_eq!(ws["status"], "fail", "{v}");
    assert!(
        ws["summary"].as_str().unwrap().contains("configured twice"),
        "{ws}"
    );

    let bad = e.write(
        "bad2.toml",
        &format!("[auth]\njwt_secret = \"{JWT_SECRET}\"\n\n[server]\napi_port = \"not-a-port\"\n"),
    );
    let o = e.run(&bad, &["--skip-remote", "--skip-probes"]);
    assert_no_secrets(&o);
    assert_eq!(o.status.code(), Some(2), "{:?}", text(&o));
    let v = report(&o);
    assert_eq!(check(&v, "config")["status"], "fail", "{v}");
}

#[test]
fn occupied_port_fails_preflight() {
    let e = Env::new("conflict");
    let cfg = e.config("");
    let _holder = TcpListener::bind(("127.0.0.1", e.ports.ws)).unwrap();
    let o = e.run(&cfg, &["--skip-remote", "--skip-probes"]);
    assert_eq!(o.status.code(), Some(2), "{:?}", text(&o));
    let v = report(&o);
    let ws = check(&v, "port.ws");
    assert_eq!(ws["status"], "fail", "{v}");
    assert!(ws["summary"].as_str().unwrap().contains("in use"), "{ws}");
    assert_eq!(check(&v, "port.api")["status"], "ok");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn running_node_expects_every_configured_listener_bound() {
    let e = Env::new("running");
    let cfg = e.config("tls_tunnel_port = 0\n");
    let app = Router::new().route(
        "/health",
        get(|| async { Json(json!({ "status": "healthy", "version": "9.9.9-test" })) }),
    );
    let addr: SocketAddr = ([127, 0, 0, 1], e.ports.api).into();
    let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let _ws = TcpListener::bind(("127.0.0.1", e.ports.ws)).unwrap();

    let o = e.run(&cfg, &["--skip-remote", "--skip-probes"]);
    assert_eq!(o.status.code(), Some(2), "{:?}", text(&o));
    let v = report(&o);
    assert_eq!(v["mode"], "running", "{v}");
    assert_eq!(v["node"]["version"], "9.9.9-test");
    assert_eq!(check(&v, "node")["status"], "ok");
    assert_eq!(check(&v, "port.api")["status"], "ok", "{v}");
    assert_eq!(check(&v, "port.ws")["status"], "ok", "{v}");
    let media = check(&v, "port.media");
    assert_eq!(media["status"], "fail", "{v}");
    assert!(
        media["summary"].as_str().unwrap().contains("did not start"),
        "{media}"
    );
}

#[test]
fn certificate_checks_cover_names_permissions_expiry_and_garbage() {
    let e = Env::new("certs");
    let now = time::OffsetDateTime::now_utc();

    let (crt, key) = self_signed(&["other.example"], now - time::Duration::hours(1), 365);
    let crt_path = e.write("other.crt", &crt);
    let key_path = e.write("other.key", &key);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&key_path, std::fs::Permissions::from_mode(0o644)).unwrap();
    }
    let cfg = e.config(&format!(
        "quic_cert_path = {:?}\nquic_key_path = {:?}\n",
        crt_path.to_str().unwrap(),
        key_path.to_str().unwrap()
    ));
    let o = e.run(&cfg, &["--skip-remote", "--skip-probes"]);
    let v = report(&o);
    let c = check(&v, "cert.media");
    assert_eq!(c["status"], "warn", "{v}");
    let h = hints(c);
    assert!(h.contains("'aurix-media' is not among"), "{h}");
    #[cfg(unix)]
    assert!(h.contains("mode 644"), "{h}");
    assert_eq!(c["details"]["sha256"].as_str().unwrap().len(), 64);
    assert_eq!(c["details"]["names"], json!(["other.example"]));

    let (crt, key) = self_signed(&["aurix-media"], now - time::Duration::days(30), 10);
    let crt_path = e.write("expired.crt", &crt);
    let key_path = e.write("expired.key", &key);
    let cfg = e.config(&format!(
        "quic_cert_path = {:?}\nquic_key_path = {:?}\n",
        crt_path.to_str().unwrap(),
        key_path.to_str().unwrap()
    ));
    let o = e.run(&cfg, &["--skip-remote", "--skip-probes"]);
    assert_eq!(o.status.code(), Some(2), "{:?}", text(&o));
    let v = report(&o);
    let c = check(&v, "cert.media");
    assert_eq!(c["status"], "fail", "{v}");
    assert!(hints(c).contains("expired"), "{c}");

    let (crt, _) = self_signed(&["aurix-media"], now - time::Duration::hours(1), 365);
    let crt_path = e.write("good.crt", &crt);
    let key_path = e.write("garbage.key", "not a key\n");
    let cfg = e.config(&format!(
        "quic_cert_path = {:?}\nquic_key_path = {:?}\n",
        crt_path.to_str().unwrap(),
        key_path.to_str().unwrap()
    ));
    let o = e.run(&cfg, &["--skip-remote", "--skip-probes"]);
    let v = report(&o);
    let c = check(&v, "cert.media");
    assert_eq!(c["status"], "fail", "{v}");
    assert!(hints(c).contains("not a PEM private key"), "{c}");
}

#[test]
fn webtransport_certificate_longer_than_browsers_accept_is_flagged() {
    let e = Env::new("wt");
    let now = time::OffsetDateTime::now_utc();
    let (crt, key) = self_signed(&["voice.example"], now - time::Duration::hours(1), 90);
    let crt_path = e.write("wt.crt", &crt);
    let key_path = e.write("wt.key", &key);
    let wt_port = free_port();
    let cfg = e.config(&format!(
        "webtransport_port = {wt_port}\nwebtransport_cert_path = {:?}\nwebtransport_key_path = {:?}\n",
        crt_path.to_str().unwrap(),
        key_path.to_str().unwrap()
    ));
    let o = e.run(&cfg, &["--skip-remote", "--skip-probes"]);
    let v = report(&o);
    let c = check(&v, "cert.webtransport");
    assert_eq!(c["status"], "warn", "{v}");
    assert!(hints(c).contains("serverCertificateHashes"), "{c}");
    assert_eq!(check(&v, "port.webtransport")["status"], "ok", "{v}");
}
