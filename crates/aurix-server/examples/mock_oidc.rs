//! Stand-in OpenID Connect provider for the admin SSO live E2E and for local development
//! when no Keycloak/Entra/Okta is around:
//!
//! ```text
//! cargo run -p aurix-server --example mock_oidc -- 127.0.0.1:18791 aurix-admin ci-only-oidc-secret
//! AURIX__AUTH__OIDC__ENABLED=true AURIX__AUTH__OIDC__ISSUER=http://127.0.0.1:18791 \
//! AURIX__AUTH__OIDC__CLIENT_ID=aurix-admin AURIX__AUTH__OIDC__CLIENT_SECRET=ci-only-oidc-secret \
//! AURIX__AUTH__OIDC__REDIRECT_URL=http://127.0.0.1:8080/admin/oidc/callback \
//! AURIX__AUTH__OIDC__ALLOW_INSECURE=true cargo run --bin aurix-server
//! ```
//!
//! Standard endpoints: discovery, JWKS, `/authorize` (no login page — the "signed-in user" is
//! whatever `POST /_mock/user` last set, the redirect back carries a one-shot code),
//! `/token` (authorization code + PKCE S256 + client secret via Basic or POST) and
//! `/userinfo` (bearer access token). ID tokens are signed with EdDSA; `POST /_mock/rotate`
//! swaps the signing key for a fresh ES256 key with a new `kid` so clients must refetch the
//! JWKS.
//!
//! Test controls (`/_mock/*`):
//!
//! * `POST /_mock/user` — JSON `{ "sub", "email", "email_verified", "name", "groups": [...],
//!   "email_in_userinfo_only": bool, "wrong_nonce": bool, "wrong_audience": bool,
//!   "deny": bool }`. Everything except `sub` is optional; `deny` makes `/authorize` answer
//!   with `error=access_denied`.
//! * `POST /_mock/rotate` — new signing key (ES256, new kid); old key is dropped.
//! * `GET /_mock/stats` — counters (`authorize`, `token`, `token_rejected`, `userinfo`, `jwks`).

use axum::body::Bytes;
use axum::extract::{Query, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use base64::Engine;
use parking_lot::Mutex;
use ring::rand::SystemRandom;
use ring::signature::{EcdsaKeyPair, Ed25519KeyPair, KeyPair, ECDSA_P256_SHA256_FIXED_SIGNING};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::sync::Arc;

fn b64url() -> base64::engine::GeneralPurpose {
    base64::engine::general_purpose::URL_SAFE_NO_PAD
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
struct MockUser {
    sub: String,
    #[serde(default)]
    email: Option<String>,
    #[serde(default = "default_true")]
    email_verified: bool,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    groups: Vec<String>,
    #[serde(default)]
    email_in_userinfo_only: bool,
    #[serde(default)]
    wrong_nonce: bool,
    #[serde(default)]
    wrong_audience: bool,
    #[serde(default)]
    deny: bool,
}

fn default_true() -> bool {
    true
}

enum SigningKey {
    Ed25519(Ed25519KeyPair),
    Es256(EcdsaKeyPair),
}

struct Signer {
    kid: String,
    key: SigningKey,
}

impl Signer {
    fn ed25519(rng: &SystemRandom) -> Signer {
        let pkcs8 = Ed25519KeyPair::generate_pkcs8(rng).expect("ed25519 keygen");
        let key = Ed25519KeyPair::from_pkcs8(pkcs8.as_ref()).expect("ed25519 key");
        Signer {
            kid: format!("ed-{}", uuid::Uuid::new_v4().simple()),
            key: SigningKey::Ed25519(key),
        }
    }

    fn es256(rng: &SystemRandom) -> Signer {
        let pkcs8 = EcdsaKeyPair::generate_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, rng)
            .expect("p256 keygen");
        let key = EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, pkcs8.as_ref(), rng)
            .expect("p256 key");
        Signer {
            kid: format!("ec-{}", uuid::Uuid::new_v4().simple()),
            key: SigningKey::Es256(key),
        }
    }

    fn alg(&self) -> &'static str {
        match self.key {
            SigningKey::Ed25519(_) => "EdDSA",
            SigningKey::Es256(_) => "ES256",
        }
    }

    fn jwk(&self) -> serde_json::Value {
        match &self.key {
            SigningKey::Ed25519(key) => serde_json::json!({
                "kty": "OKP", "crv": "Ed25519", "use": "sig", "alg": "EdDSA", "kid": self.kid,
                "x": b64url().encode(key.public_key().as_ref()),
            }),
            SigningKey::Es256(key) => {
                // Uncompressed SEC1 point: 0x04 || X (32) || Y (32).
                let point = key.public_key().as_ref();
                serde_json::json!({
                    "kty": "EC", "crv": "P-256", "use": "sig", "alg": "ES256", "kid": self.kid,
                    "x": b64url().encode(&point[1..33]),
                    "y": b64url().encode(&point[33..65]),
                })
            }
        }
    }

    fn sign_jwt(&self, claims: &serde_json::Value, rng: &SystemRandom) -> String {
        let header = serde_json::json!({"alg": self.alg(), "typ": "JWT", "kid": self.kid});
        let signing_input = format!(
            "{}.{}",
            b64url().encode(serde_json::to_vec(&header).unwrap()),
            b64url().encode(serde_json::to_vec(claims).unwrap())
        );
        let signature = match &self.key {
            SigningKey::Ed25519(key) => key.sign(signing_input.as_bytes()).as_ref().to_vec(),
            SigningKey::Es256(key) => key
                .sign(rng, signing_input.as_bytes())
                .expect("p256 sign")
                .as_ref()
                .to_vec(),
        };
        format!("{signing_input}.{}", b64url().encode(signature))
    }
}

struct PendingCode {
    user: MockUser,
    nonce: Option<String>,
    code_challenge: Option<String>,
    redirect_uri: String,
    client_id: String,
}

#[derive(Default, Serialize, Clone)]
struct Stats {
    authorize: u64,
    token: u64,
    token_rejected: u64,
    userinfo: u64,
    jwks: u64,
}

struct Inner {
    signer: Signer,
    user: Option<MockUser>,
    codes: HashMap<String, PendingCode>,
    access_tokens: HashMap<String, MockUser>,
    stats: Stats,
}

#[derive(Clone)]
struct Shared {
    issuer: String,
    client_id: String,
    client_secret: Option<String>,
    rng: SystemRandom,
    inner: Arc<Mutex<Inner>>,
}

#[tokio::main]
async fn main() {
    let bind = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "127.0.0.1:18791".to_string());
    let client_id = std::env::args()
        .nth(2)
        .unwrap_or_else(|| "aurix-admin".to_string());
    let client_secret = std::env::args().nth(3).filter(|s| !s.is_empty());
    let rng = SystemRandom::new();
    let shared = Shared {
        issuer: format!("http://{bind}"),
        client_id,
        client_secret,
        inner: Arc::new(Mutex::new(Inner {
            signer: Signer::ed25519(&rng),
            user: None,
            codes: HashMap::new(),
            access_tokens: HashMap::new(),
            stats: Stats::default(),
        })),
        rng,
    };
    let app = Router::new()
        .route("/.well-known/openid-configuration", get(discovery))
        .route("/jwks", get(jwks))
        .route("/authorize", get(authorize))
        .route("/token", post(token))
        .route("/userinfo", get(userinfo))
        .route("/_mock/user", post(set_user))
        .route("/_mock/rotate", post(rotate))
        .route("/_mock/stats", get(stats))
        .with_state(shared);
    let listener = tokio::net::TcpListener::bind(&bind).await.expect("bind");
    eprintln!("mock OIDC provider on http://{bind}");
    axum::serve(listener, app).await.expect("serve");
}

async fn discovery(State(s): State<Shared>) -> Json<serde_json::Value> {
    let i = &s.issuer;
    Json(serde_json::json!({
        "issuer": i,
        "authorization_endpoint": format!("{i}/authorize"),
        "token_endpoint": format!("{i}/token"),
        "userinfo_endpoint": format!("{i}/userinfo"),
        "jwks_uri": format!("{i}/jwks"),
        "response_types_supported": ["code"],
        "subject_types_supported": ["public"],
        "id_token_signing_alg_values_supported": ["EdDSA", "ES256"],
        "scopes_supported": ["openid", "email", "profile", "groups"],
        "token_endpoint_auth_methods_supported": ["client_secret_basic", "client_secret_post"],
        "code_challenge_methods_supported": ["S256"],
        "claims_supported": ["sub", "email", "email_verified", "name", "groups"],
    }))
}

async fn jwks(State(s): State<Shared>) -> Json<serde_json::Value> {
    let mut inner = s.inner.lock();
    inner.stats.jwks += 1;
    Json(serde_json::json!({"keys": [inner.signer.jwk()]}))
}

fn oauth_error(status: StatusCode, error: &str, description: impl Into<String>) -> Response {
    (
        status,
        Json(serde_json::json!({"error": error, "error_description": description.into()})),
    )
        .into_response()
}

async fn authorize(State(s): State<Shared>, Query(q): Query<HashMap<String, String>>) -> Response {
    let mut inner = s.inner.lock();
    inner.stats.authorize += 1;
    let Some(redirect_uri) = q.get("redirect_uri").cloned() else {
        return oauth_error(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            "redirect_uri missing",
        );
    };
    let state = q.get("state").cloned().unwrap_or_default();
    let mut back = reqwest::Url::parse(&redirect_uri).expect("redirect_uri");
    if q.get("response_type").map(String::as_str) != Some("code") {
        back.query_pairs_mut()
            .append_pair("error", "unsupported_response_type")
            .append_pair("state", &state);
        return Redirect::to(back.as_str()).into_response();
    }
    if q.get("client_id") != Some(&s.client_id) {
        back.query_pairs_mut()
            .append_pair("error", "unauthorized_client")
            .append_pair("state", &state);
        return Redirect::to(back.as_str()).into_response();
    }
    let Some(user) = inner.user.clone() else {
        back.query_pairs_mut()
            .append_pair("error", "login_required")
            .append_pair("error_description", "no mock user configured")
            .append_pair("state", &state);
        return Redirect::to(back.as_str()).into_response();
    };
    if user.deny {
        back.query_pairs_mut()
            .append_pair("error", "access_denied")
            .append_pair("error_description", "the user cancelled")
            .append_pair("state", &state);
        return Redirect::to(back.as_str()).into_response();
    }
    if q.contains_key("code_challenge")
        && q.get("code_challenge_method").map(String::as_str) != Some("S256")
    {
        back.query_pairs_mut()
            .append_pair("error", "invalid_request")
            .append_pair("error_description", "only S256 is supported")
            .append_pair("state", &state);
        return Redirect::to(back.as_str()).into_response();
    }
    let code = b64url().encode(uuid::Uuid::new_v4().as_bytes());
    inner.codes.insert(
        code.clone(),
        PendingCode {
            user,
            nonce: q.get("nonce").cloned(),
            code_challenge: q.get("code_challenge").cloned(),
            redirect_uri: redirect_uri.clone(),
            client_id: s.client_id.clone(),
        },
    );
    back.query_pairs_mut()
        .append_pair("code", &code)
        .append_pair("state", &state);
    Redirect::to(back.as_str()).into_response()
}

fn client_authenticated(s: &Shared, headers: &HeaderMap, form: &HashMap<String, String>) -> bool {
    let Some(expected) = &s.client_secret else {
        return true;
    };
    if let Some(basic) = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Basic "))
    {
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(basic)
            .ok()
            .and_then(|b| String::from_utf8(b).ok());
        return decoded.as_deref() == Some(&format!("{}:{expected}", s.client_id));
    }
    form.get("client_secret") == Some(expected)
}

async fn token(State(s): State<Shared>, headers: HeaderMap, body: Bytes) -> Response {
    let form: HashMap<String, String> = url::form_urlencoded::parse(&body).into_owned().collect();
    let mut inner = s.inner.lock();
    inner.stats.token += 1;
    let reject = |inner: &mut Inner, error: &str, desc: &str| {
        inner.stats.token_rejected += 1;
        oauth_error(StatusCode::BAD_REQUEST, error, desc)
    };
    if !client_authenticated(&s, &headers, &form) {
        return reject(&mut inner, "invalid_client", "client authentication failed");
    }
    if form.get("grant_type").map(String::as_str) != Some("authorization_code") {
        return reject(
            &mut inner,
            "unsupported_grant_type",
            "authorization_code only",
        );
    }
    let Some(pending) = form.get("code").and_then(|c| inner.codes.remove(c)) else {
        return reject(&mut inner, "invalid_grant", "unknown or already used code");
    };
    if form.get("redirect_uri") != Some(&pending.redirect_uri) {
        return reject(&mut inner, "invalid_grant", "redirect_uri mismatch");
    }
    if form
        .get("client_id")
        .is_some_and(|c| *c != pending.client_id)
    {
        return reject(&mut inner, "invalid_grant", "client_id mismatch");
    }
    if let Some(challenge) = &pending.code_challenge {
        let Some(verifier) = form.get("code_verifier") else {
            return reject(&mut inner, "invalid_grant", "code_verifier missing");
        };
        let computed = b64url().encode(Sha256::digest(verifier.as_bytes()));
        if &computed != challenge {
            return reject(&mut inner, "invalid_grant", "PKCE verification failed");
        }
    }

    let now = chrono::Utc::now().timestamp();
    let user = pending.user;
    let mut claims = serde_json::json!({
        "iss": s.issuer,
        "sub": user.sub,
        "aud": if user.wrong_audience { "someone-else".to_string() } else { s.client_id.clone() },
        "iat": now,
        "exp": now + 300,
        "auth_time": now,
    });
    if let Some(nonce) = &pending.nonce {
        claims["nonce"] = serde_json::Value::String(if user.wrong_nonce {
            format!("{nonce}-tampered")
        } else {
            nonce.clone()
        });
    }
    if !user.email_in_userinfo_only {
        if let Some(email) = &user.email {
            claims["email"] = serde_json::Value::String(email.clone());
            claims["email_verified"] = serde_json::Value::Bool(user.email_verified);
        }
        if let Some(name) = &user.name {
            claims["name"] = serde_json::Value::String(name.clone());
        }
        if !user.groups.is_empty() {
            claims["groups"] = serde_json::json!(user.groups);
        }
    }
    let id_token = inner.signer.sign_jwt(&claims, &s.rng);
    let access_token = b64url().encode(uuid::Uuid::new_v4().as_bytes());
    inner.access_tokens.insert(access_token.clone(), user);
    Json(serde_json::json!({
        "access_token": access_token,
        "token_type": "Bearer",
        "expires_in": 300,
        "id_token": id_token,
        "scope": form.get("scope").cloned().unwrap_or_else(|| "openid email profile".into()),
    }))
    .into_response()
}

async fn userinfo(State(s): State<Shared>, headers: HeaderMap) -> Response {
    let mut inner = s.inner.lock();
    inner.stats.userinfo += 1;
    let bearer = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "));
    let Some(user) = bearer.and_then(|t| inner.access_tokens.get(t)).cloned() else {
        return (StatusCode::UNAUTHORIZED, "invalid_token").into_response();
    };
    let mut info = serde_json::json!({"sub": user.sub});
    if let Some(email) = &user.email {
        info["email"] = serde_json::Value::String(email.clone());
        info["email_verified"] = serde_json::Value::Bool(user.email_verified);
    }
    if let Some(name) = &user.name {
        info["name"] = serde_json::Value::String(name.clone());
    }
    if !user.groups.is_empty() {
        info["groups"] = serde_json::json!(user.groups);
    }
    Json(info).into_response()
}

async fn set_user(State(s): State<Shared>, Json(user): Json<MockUser>) -> Json<MockUser> {
    s.inner.lock().user = Some(user.clone());
    Json(user)
}

async fn rotate(State(s): State<Shared>) -> Json<serde_json::Value> {
    let mut inner = s.inner.lock();
    inner.signer = Signer::es256(&s.rng);
    Json(serde_json::json!({"kid": inner.signer.kid, "alg": inner.signer.alg()}))
}

async fn stats(State(s): State<Shared>) -> Json<Stats> {
    Json(s.inner.lock().stats.clone())
}
