//! End-to-end tests of the `aurix` binary against a fake node: auth headers, path/query
//! encoding, error mapping and — above all — that no credential ever reaches stdout/stderr.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::{Arc, Mutex};

use axum::body::Bytes;
use axum::extract::{Path as AxPath, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::{json, Value};

const API_KEY: &str = "ak_live_SECRET_KEY_do_not_print_9f8e7d6c";
const ADMIN_TOKEN: &str = "eyJhbGciOi.ADMIN_SECRET_TOKEN.sig";
const PLAYER_TOKEN: &str = "eyJhbGciOi.PLAYER_TOKEN_VALUE.sig";

#[derive(Clone, Debug)]
struct Seen {
    method: String,
    path: String,
    query: HashMap<String, String>,
    headers: HashMap<String, String>,
    body: Value,
}

type Log = Arc<Mutex<Vec<Seen>>>;

fn record(
    log: &Log,
    method: &str,
    path: String,
    q: HashMap<String, String>,
    h: &HeaderMap,
    body: &Bytes,
) {
    let headers = h
        .iter()
        .map(|(k, v)| {
            (
                k.as_str().to_ascii_lowercase(),
                v.to_str().unwrap_or("").to_string(),
            )
        })
        .collect();
    let body = if body.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(body).unwrap_or(Value::Null)
    };
    log.lock().unwrap().push(Seen {
        method: method.to_string(),
        path,
        query: q,
        headers,
        body,
    });
}

fn err(status: StatusCode, code: &str, msg: &str) -> Response {
    (
        status,
        [("x-request-id", "req-42")],
        Json(json!({ "error": { "code": code, "message": msg } })),
    )
        .into_response()
}

fn needs_api_key(h: &HeaderMap) -> Option<Response> {
    match h.get("x-api-key").and_then(|v| v.to_str().ok()) {
        Some(k) if k == API_KEY => None,
        _ => Some(err(
            StatusCode::UNAUTHORIZED,
            "AUTH_FAILED",
            "Missing API key",
        )),
    }
}

fn needs_admin(h: &HeaderMap) -> Option<Response> {
    match h.get("authorization").and_then(|v| v.to_str().ok()) {
        Some(a) if a == format!("Bearer {ADMIN_TOKEN}") => None,
        _ => Some(err(
            StatusCode::UNAUTHORIZED,
            "AUTH_FAILED",
            "Missing bearer token",
        )),
    }
}

async fn start_fake() -> (SocketAddr, Log) {
    let log: Log = Arc::default();
    let app = Router::new()
        .route(
            "/health",
            get(|State(l): State<Log>, h: HeaderMap| async move {
                record(&l, "GET", "/health".into(), HashMap::new(), &h, &Bytes::new());
                Json(json!({ "status": "healthy", "node_id": "n1", "version": "1.2.0",
                             "active_channels": 0, "active_sessions": 0, "timestamp": "2026-01-01T00:00:00Z" }))
            }),
        )
        .route(
            "/v1/tokens",
            post(|State(l): State<Log>, h: HeaderMap, body: Bytes| async move {
                record(&l, "POST", "/v1/tokens".into(), HashMap::new(), &h, &body);
                if let Some(e) = needs_api_key(&h) {
                    return e;
                }
                Json(json!({ "token": PLAYER_TOKEN, "user_id": "u-1", "expires_at": "2026-01-01T01:00:00Z",
                             "channels": [], "endpoint": { "region": "eu_west", "ws_url": "ws://fake/ws",
                             "probe_url": "http://fake/health", "node_id": "n1", "nodes": 1, "load_factor": 0.1 } }))
                .into_response()
            }),
        )
        .route(
            "/v1/channels",
            get(|State(l): State<Log>, Query(q): Query<HashMap<String, String>>, h: HeaderMap| async move {
                record(&l, "GET", "/v1/channels".into(), q, &h, &Bytes::new());
                if let Some(e) = needs_api_key(&h) {
                    return e;
                }
                Json(json!({ "data": [], "page": 1, "per_page": 1, "total": 0 })).into_response()
            }),
        )
        .route(
            "/v1/channels/:id",
            get(|State(l): State<Log>, AxPath(id): AxPath<String>, h: HeaderMap| async move {
                record(&l, "GET", format!("/v1/channels/{id}"), HashMap::new(), &h, &Bytes::new());
                if let Some(e) = needs_api_key(&h) {
                    return e;
                }
                err(StatusCode::NOT_FOUND, "CHANNEL_NOT_FOUND", &format!("Channel not found: {id}"))
            }),
        )
        .route(
            "/v1/nodes",
            get(|State(l): State<Log>, h: HeaderMap| async move {
                record(&l, "GET", "/v1/nodes".into(), HashMap::new(), &h, &Bytes::new());
                if let Some(e) = needs_admin(&h) {
                    return e;
                }
                Json(json!([{ "id": "n1", "healthy": true }])).into_response()
            }),
        )
        .route(
            "/admin/login",
            post(|State(l): State<Log>, h: HeaderMap, body: Bytes| async move {
                record(&l, "POST", "/admin/login".into(), HashMap::new(), &h, &body);
                Json(json!({ "token": ADMIN_TOKEN, "expires_at": "2026-01-02T00:00:00Z",
                             "admin": { "id": "a1", "email": "ops@example.com", "role": "superadmin" } }))
            }),
        )
        .route(
            "/admin/me",
            get(|State(l): State<Log>, h: HeaderMap| async move {
                record(&l, "GET", "/admin/me".into(), HashMap::new(), &h, &Bytes::new());
                if let Some(e) = needs_admin(&h) {
                    return e;
                }
                Json(json!({ "id": "a1", "email": "ops@example.com", "role": "superadmin" })).into_response()
            }),
        )
        .with_state(log.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (addr, log)
}

struct Env {
    dir: PathBuf,
    server: String,
    log: Log,
}

impl Env {
    async fn new(name: &str) -> Env {
        let (addr, log) = start_fake().await;
        let dir =
            std::env::temp_dir().join(format!("aurix-cli-test-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Env {
            dir,
            server: format!("http://{addr}"),
            log,
        }
    }

    fn secret_file(&self, name: &str, contents: &str) -> PathBuf {
        let p = self.dir.join(name);
        std::fs::write(&p, format!("{contents}\n")).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
        p
    }

    fn cmd(&self) -> Command {
        let mut c = Command::new(env!("CARGO_BIN_EXE_aurix"));
        c.env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("HOME", &self.dir)
            .env("AURIX_CONFIG", self.dir.join("config.toml"))
            .arg("--server")
            .arg(&self.server);
        c
    }

    fn run(&self, args: &[&str], stdin: Option<&str>) -> Output {
        let mut c = self.cmd();
        c.args(args);
        c.stdin(std::process::Stdio::piped());
        c.stdout(std::process::Stdio::piped());
        c.stderr(std::process::Stdio::piped());
        let mut child = c.spawn().unwrap();
        {
            use std::io::Write;
            let mut si = child.stdin.take().unwrap();
            if let Some(s) = stdin {
                si.write_all(s.as_bytes()).unwrap();
            }
        }
        child.wait_with_output().unwrap()
    }

    fn last(&self) -> Seen {
        self.log
            .lock()
            .unwrap()
            .last()
            .cloned()
            .expect("fake node saw a request")
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
    for s in [API_KEY, ADMIN_TOKEN] {
        assert!(!out.contains(s), "secret leaked to stdout: {out}");
        assert!(!errs.contains(s), "secret leaked to stderr: {errs}");
    }
}

fn mode(p: &Path) -> u32 {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(p).unwrap().permissions().mode() & 0o777
    }
    #[cfg(not(unix))]
    {
        let _ = p;
        0o600
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn health_needs_no_credentials_and_sends_none() {
    let e = Env::new("health").await;
    let o = e.run(&["health", "-o", "json"], None);
    assert!(o.status.success(), "{:?}", text(&o));
    let v: Value = serde_json::from_slice(&o.stdout).unwrap();
    assert_eq!(v["status"], "healthy");
    let seen = e.last();
    assert!(!seen.headers.contains_key("x-api-key"));
    assert!(!seen.headers.contains_key("authorization"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn token_issue_uses_api_key_file_and_matches_schema() {
    let e = Env::new("token").await;
    let key = e.secret_file("api-key", API_KEY);
    let o = e.run(
        &[
            "--api-key-file",
            key.to_str().unwrap(),
            "token",
            "issue",
            "--external-id",
            "p-1",
            "--display-name",
            "Player One",
            "--channel",
            "c-1:jsr",
            "--ad-hoc",
            "raid:positional:8@jr",
            "--region",
            "eu_west",
            "--metadata",
            r#"{"guild":"g1"}"#,
            "-o",
            "json",
        ],
        None,
    );
    assert!(o.status.success(), "{:?}", text(&o));
    assert_no_secrets(&o);
    let v: Value = serde_json::from_slice(&o.stdout).unwrap();
    assert_eq!(
        v["token"], PLAYER_TOKEN,
        "player token is the command's output"
    );
    assert_eq!(v["endpoint"]["ws_url"], "ws://fake/ws");

    let seen = e.last();
    assert_eq!(
        (seen.method.as_str(), seen.path.as_str()),
        ("POST", "/v1/tokens")
    );
    assert_eq!(seen.headers["x-api-key"], API_KEY);
    assert!(!seen.headers.contains_key("authorization"));
    assert_eq!(seen.body["external_id"], "p-1");
    assert_eq!(seen.body["display_name"], "Player One");
    assert_eq!(seen.body["region"], "eu_west");
    assert_eq!(seen.body["metadata"], json!({ "guild": "g1" }));
    let ch = seen.body["channels"].as_array().unwrap();
    assert_eq!(ch.len(), 2);
    assert_eq!(ch[0]["channel_id"], "c-1");
    assert_eq!(
        (&ch[0]["join"], &ch[0]["speak"], &ch[0]["receive"]),
        (&json!(true), &json!(true), &json!(true))
    );
    assert!(ch[1].get("channel_id").is_none());
    assert_eq!(
        ch[1]["ad_hoc"],
        json!({ "name": "raid", "channel_type": "positional", "max_participants": 8 })
    );
    assert_eq!(ch[1]["speak"], json!(false));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn api_key_from_env_and_inline_never_echoed() {
    let e = Env::new("env").await;
    let mut c = e.cmd();
    c.env("AURIX_API_KEY", API_KEY)
        .args(["api", "listChannels", "-q", "per_page=1", "-o", "json"]);
    let o = c.output().unwrap();
    assert!(o.status.success(), "{:?}", text(&o));
    assert_no_secrets(&o);
    assert_eq!(e.last().headers["x-api-key"], API_KEY);
    assert_eq!(e.last().query["per_page"], "1");

    let o = e.run(&["--api-key", API_KEY, "health"], None);
    assert!(o.status.success());
    assert_no_secrets(&o);
    assert!(
        text(&o).1.contains("visible to other processes"),
        "inline key warns"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn generic_api_encodes_path_params_and_maps_errors() {
    let e = Env::new("api").await;
    let key = e.secret_file("api-key", API_KEY);
    let o = e.run(
        &[
            "--api-key-file",
            key.to_str().unwrap(),
            "api",
            "getchannel",
            "-p",
            "channel_id=a b/c?d",
        ],
        None,
    );
    assert_eq!(o.status.code(), Some(4), "{:?}", text(&o));
    assert_eq!(
        e.last().path,
        "/v1/channels/a b/c?d",
        "axum decodes the percent-encoded segment"
    );
    let (_, errs) = text(&o);
    assert!(
        errs.contains("404") && errs.contains("CHANNEL_NOT_FOUND"),
        "{errs}"
    );
    assert!(errs.contains("req-42"), "request id surfaced: {errs}");

    let o = e.run(
        &[
            "--api-key-file",
            key.to_str().unwrap(),
            "-o",
            "json",
            "api",
            "/v1/channels/{channel_id}",
            "-X",
            "GET",
            "-p",
            "channel_id=x",
        ],
        None,
    );
    assert_eq!(o.status.code(), Some(4));
    let v: Value = serde_json::from_slice(&o.stderr).unwrap();
    assert_eq!(v["error"]["status"], 404);
    assert_eq!(v["error"]["code"], "CHANNEL_NOT_FOUND");
    assert_eq!(v["error"]["request_id"], "req-42");

    let o = e.run(
        &["api", "getChannel", "-p", "channel_id=x", "-p", "nope=1"],
        None,
    );
    assert_eq!(o.status.code(), Some(1));
    assert!(
        text(&o).1.contains("nope"),
        "undeclared path param rejected before any request"
    );

    let o = e.run(
        &[
            "api",
            "listChannels",
            "-q",
            "bogus=1",
            "--api-key-file",
            key.to_str().unwrap(),
        ],
        None,
    );
    assert_eq!(o.status.code(), Some(1));
    assert!(text(&o).1.contains("--allow-unknown-query"));

    let o = e.run(&["api", "--list", "webhook", "-o", "json"], None);
    assert!(o.status.success());
    let v: Value = serde_json::from_slice(&o.stdout).unwrap();
    let ops = v["operations"].as_array().unwrap();
    assert!(!ops.is_empty());
    assert!(ops.iter().all(|op| {
        format!("{} {} {}", op["id"], op["path"], op["summary"])
            .to_lowercase()
            .contains("webhook")
    }));
    assert!(ops
        .iter()
        .any(|op| op["id"] == "createWebhook" && op["auth"][0] == "ApiKeyHeader"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn missing_credentials_fail_locally_with_guidance() {
    let e = Env::new("nocreds").await;
    let before = e.log.lock().unwrap().len();
    let o = e.run(
        &[
            "token",
            "issue",
            "--external-id",
            "p",
            "--display-name",
            "P",
        ],
        None,
    );
    assert_eq!(o.status.code(), Some(1));
    assert!(text(&o).1.contains("API key"), "{:?}", text(&o));
    let o = e.run(&["node", "list"], None);
    assert_eq!(o.status.code(), Some(1));
    assert!(text(&o).1.contains("admin login --save"), "{:?}", text(&o));
    assert_eq!(
        e.log.lock().unwrap().len(),
        before,
        "no request left the machine"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn admin_login_save_redacts_and_persists_token_privately() {
    let e = Env::new("admin").await;
    let o = e.run(&["config", "init", "--name", "dev", "--set-default"], None);
    assert!(o.status.success(), "{:?}", text(&o));

    let o = e.run(
        &[
            "admin",
            "login",
            "--email",
            "ops@example.com",
            "--save",
            "-o",
            "json",
        ],
        Some("hunter2\n"),
    );
    assert!(o.status.success(), "{:?}", text(&o));
    assert_no_secrets(&o);
    let (out, _) = text(&o);
    assert!(!out.contains("hunter2"), "password never echoed");
    let v: Value = serde_json::from_slice(&o.stdout).unwrap();
    assert_eq!(v["token"], "<redacted>");
    assert_eq!(e.last().body["password"], "hunter2");
    let token_file = PathBuf::from(v["token_file"].as_str().unwrap());
    assert_eq!(
        std::fs::read_to_string(&token_file).unwrap().trim(),
        ADMIN_TOKEN
    );
    assert_eq!(mode(&token_file), 0o600);
    assert_eq!(mode(&e.dir.join("config.toml")), 0o600);

    let o = e.run(&["admin", "me", "-o", "json"], None);
    assert!(o.status.success(), "{:?}", text(&o));
    assert_eq!(
        e.last().headers["authorization"],
        format!("Bearer {ADMIN_TOKEN}")
    );
    assert!(!e.last().headers.contains_key("x-api-key"));

    let o = e.run(&["node", "list", "-o", "json"], None);
    assert!(o.status.success(), "{:?}", text(&o));
    assert_no_secrets(&o);

    let o = e.run(&["config", "show", "-o", "json"], None);
    assert!(o.status.success());
    assert_no_secrets(&o);
    let v: Value = serde_json::from_slice(&o.stdout).unwrap();
    assert_eq!(v["profiles"]["dev"]["admin_token"]["source"], "login");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn diagnose_reports_credential_sources_not_values() {
    let e = Env::new("diag").await;
    let key = e.secret_file("api-key", API_KEY);
    let o = e.run(
        &[
            "--api-key-file",
            key.to_str().unwrap(),
            "diagnose",
            "-o",
            "json",
        ],
        None,
    );
    assert_no_secrets(&o);
    let v: Value = serde_json::from_slice(&o.stdout).unwrap();
    assert_eq!(v["checks"]["health"]["ok"], true);
    assert_eq!(v["checks"]["auth"]["api_key"]["ok"], true, "{v}");
    assert!(v["checks"]["auth"]["api_key_source"]
        .as_str()
        .unwrap()
        .contains("--api-key-file"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn webhook_sign_and_verify_match_shared_vector() {
    let e = Env::new("webhook").await;
    let vector: Value = serde_json::from_str(include_str!(
        "../../../sdk/server/vectors/webhook_signature.json"
    ))
    .unwrap();
    let secret = e.secret_file("wh.secret", vector["secret"].as_str().unwrap());
    let body = e.dir.join("body.json");
    std::fs::write(&body, vector["body"].as_str().unwrap()).unwrap();
    let ts = vector["timestamp"].as_i64().unwrap().to_string();

    let o = e.run(
        &[
            "webhook",
            "sign",
            "--secret-file",
            secret.to_str().unwrap(),
            "--body-file",
            body.to_str().unwrap(),
            "--timestamp",
            &ts,
            "--field",
            "signature",
        ],
        None,
    );
    assert!(o.status.success(), "{:?}", text(&o));
    let sig = text(&o).0.trim().to_string();
    assert_eq!(sig, vector["header"].as_str().unwrap());

    let now = (vector["timestamp"].as_i64().unwrap() + 5).to_string();
    let o = e.run(
        &[
            "webhook",
            "verify",
            "--secret-file",
            secret.to_str().unwrap(),
            "--body-file",
            body.to_str().unwrap(),
            "--signature",
            &sig,
            "--now",
            &now,
            "-o",
            "json",
        ],
        None,
    );
    assert!(o.status.success(), "{:?}", text(&o));
    let v: Value = serde_json::from_slice(&o.stdout).unwrap();
    assert_eq!(v["valid"], true);

    let late =
        (vector["timestamp"].as_i64().unwrap() + vector["tolerance_sec"].as_i64().unwrap() + 1)
            .to_string();
    let o = e.run(
        &[
            "webhook",
            "verify",
            "--secret-file",
            secret.to_str().unwrap(),
            "--body-file",
            body.to_str().unwrap(),
            "--signature",
            &sig,
            "--now",
            &late,
        ],
        None,
    );
    assert_eq!(o.status.code(), Some(2), "{:?}", text(&o));
}
