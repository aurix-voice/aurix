//! Admin single sign-on over OpenID Connect (authorization code + PKCE).
//!
//! The provider is fully stateless on the server side: everything a login attempt needs
//! (PKCE verifier, nonce, expiry, a hash of the browser cookie, where to send the browser
//! afterwards) travels inside an AES-256-GCM sealed `state` parameter keyed from the JWT
//! secret, so a callback may land on any node. Login CSRF is prevented by binding the state
//! to a random `HttpOnly` cookie set on `/admin/oidc/login`; replayed authorization codes are
//! refused by the provider itself (codes are single-use) and by a per-node state ledger.
//!
//! Only asymmetric ID-token signatures are accepted (RS*/PS*/ES*/EdDSA); symmetric keys in the
//! JWKS are ignored so a provider can never be downgraded to `HS*` with a guessable secret.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::time::{Duration, Instant};

use aurix_common::config::OidcConfig;
use aurix_common::crypto::{constant_time_eq, hmac_sha256, CryptoProvider};
use aurix_common::error::{AurixError, Result};
use aurix_common::net::validate_outbound_url;
use aurix_common::types::AdminRole;
use base64::Engine;
use jsonwebtoken::jwk::{AlgorithmParameters, Jwk};
use jsonwebtoken::{decode, decode_header, Algorithm, DecodingKey, Validation};
use parking_lot::{Mutex, RwLock};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use url::Url;

const STATE_AAD: &[u8] = b"aurix-admin-oidc-state-v1";
const STATE_KEY_LABEL: &[u8] = b"aurix-admin-oidc-state";
const DISCOVERY_TTL: Duration = Duration::from_secs(3600);
const JWKS_TTL: Duration = Duration::from_secs(3600);
const JWKS_REFETCH_MIN_INTERVAL: Duration = Duration::from_secs(30);
const MAX_BODY_BYTES: usize = 256 * 1024;
const USER_AGENT: &str = concat!("aurix-admin-oidc/", env!("CARGO_PKG_VERSION"));
const ALLOWED_ALGS: &[Algorithm] = &[
    Algorithm::RS256,
    Algorithm::RS384,
    Algorithm::RS512,
    Algorithm::PS256,
    Algorithm::PS384,
    Algorithm::PS512,
    Algorithm::ES256,
    Algorithm::ES384,
    Algorithm::EdDSA,
];

fn b64url() -> base64::engine::GeneralPurpose {
    base64::engine::general_purpose::URL_SAFE_NO_PAD
}

#[derive(Debug, Clone, Deserialize)]
pub struct Discovery {
    pub issuer: String,
    pub authorization_endpoint: String,
    pub token_endpoint: String,
    pub jwks_uri: String,
    #[serde(default)]
    pub userinfo_endpoint: Option<String>,
    #[serde(default)]
    pub end_session_endpoint: Option<String>,
    #[serde(default)]
    pub token_endpoint_auth_methods_supported: Vec<String>,
    #[serde(default)]
    pub code_challenge_methods_supported: Vec<String>,
    #[serde(default)]
    pub id_token_signing_alg_values_supported: Vec<String>,
}

struct CachedDiscovery {
    doc: Arc<Discovery>,
    fetched_at: Instant,
}

struct CachedJwks {
    keys: HashMap<String, (Algorithm, DecodingKey)>,
    /// Keys without a `kid`, tried in order when the token header has none.
    anonymous: Vec<(Algorithm, DecodingKey)>,
    fetched_at: Instant,
}

/// What `/admin/oidc/login` hands to the browser.
#[derive(Debug, Clone)]
pub struct LoginStart {
    pub authorization_url: String,
    /// Random value for the `aurix_oidc_login` cookie; the sealed state carries its hash.
    pub cookie_value: String,
    pub cookie_max_age_secs: u64,
}

/// Verified identity from the provider, before it is mapped onto a local account.
#[derive(Debug, Clone)]
pub struct VerifiedIdentity {
    pub issuer: String,
    pub subject: String,
    pub email: String,
    pub display_name: String,
    pub groups: Vec<String>,
    pub role: AdminRole,
    pub return_to: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
struct SealedState {
    /// Random id for the per-node replay ledger.
    jti: String,
    nonce: String,
    pkce_verifier: String,
    cookie_hash: String,
    exp: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    return_to: Option<String>,
}

#[derive(Debug, Deserialize)]
struct TokenResponse {
    id_token: Option<String>,
    access_token: Option<String>,
    #[serde(default)]
    error: Option<String>,
    #[serde(default)]
    error_description: Option<String>,
}

#[derive(Debug, Deserialize)]
struct IdTokenClaims {
    iss: String,
    sub: String,
    #[serde(default)]
    nonce: Option<String>,
    #[serde(default)]
    azp: Option<String>,
    #[serde(flatten)]
    rest: BTreeMap<String, serde_json::Value>,
}

pub struct OidcProvider {
    cfg: OidcConfig,
    http: reqwest::Client,
    crypto: CryptoProvider,
    state_key: [u8; 32],
    discovery: RwLock<Option<CachedDiscovery>>,
    jwks: RwLock<Option<CachedJwks>>,
    jwks_last_fetch: Mutex<Option<Instant>>,
    /// jti → expiry, consumed states on this node.
    used_states: Mutex<HashMap<String, i64>>,
}

impl OidcProvider {
    pub fn new(cfg: OidcConfig, jwt_secret: &str) -> Result<Self> {
        let http = reqwest::Client::builder()
            .user_agent(USER_AGENT)
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_millis(cfg.timeout_ms))
            .connect_timeout(Duration::from_millis(cfg.timeout_ms.min(5000)))
            .build()
            .map_err(|e| AurixError::Internal(format!("oidc http client: {e}")))?;
        Ok(Self {
            state_key: hmac_sha256(jwt_secret.as_bytes(), &[STATE_KEY_LABEL]),
            cfg,
            http,
            crypto: CryptoProvider::new(),
            discovery: RwLock::new(None),
            jwks: RwLock::new(None),
            jwks_last_fetch: Mutex::new(None),
            used_states: Mutex::new(HashMap::new()),
        })
    }

    pub fn config(&self) -> &OidcConfig {
        &self.cfg
    }

    pub fn issuer(&self) -> &str {
        self.cfg.issuer_trimmed()
    }

    // ── Discovery and keys ──

    pub async fn discovery(&self) -> Result<Arc<Discovery>> {
        if let Some(cached) = self.discovery.read().as_ref() {
            if cached.fetched_at.elapsed() < DISCOVERY_TTL {
                return Ok(cached.doc.clone());
            }
        }
        let doc = self.fetch_discovery().await?;
        let doc = Arc::new(doc);
        *self.discovery.write() = Some(CachedDiscovery {
            doc: doc.clone(),
            fetched_at: Instant::now(),
        });
        Ok(doc)
    }

    async fn fetch_discovery(&self) -> Result<Discovery> {
        let url = format!("{}/.well-known/openid-configuration", self.issuer());
        let body = self.get_json(&url, None).await?;
        let doc: Discovery = serde_json::from_value(body)
            .map_err(|e| AurixError::Internal(format!("oidc discovery document: {e}")))?;
        if doc.issuer.trim_end_matches('/') != self.issuer() {
            return Err(AurixError::Internal(format!(
                "oidc discovery issuer mismatch: configured {:?}, document says {:?}",
                self.issuer(),
                doc.issuer
            )));
        }
        for (name, value) in [
            ("authorization_endpoint", &doc.authorization_endpoint),
            ("token_endpoint", &doc.token_endpoint),
            ("jwks_uri", &doc.jwks_uri),
        ] {
            self.check_provider_url(value, name)?;
        }
        if let Some(userinfo) = &doc.userinfo_endpoint {
            self.check_provider_url(userinfo, "userinfo_endpoint")?;
        }
        if !doc.code_challenge_methods_supported.is_empty()
            && !doc
                .code_challenge_methods_supported
                .iter()
                .any(|m| m == "S256")
        {
            return Err(AurixError::Internal(
                "oidc provider does not support PKCE S256".into(),
            ));
        }
        Ok(doc)
    }

    fn check_provider_url(&self, raw: &str, name: &str) -> Result<Url> {
        validate_outbound_url(
            raw,
            "https",
            "http",
            !self.cfg.allow_insecure,
            self.cfg.allow_insecure,
            &format!("auth.oidc ({name})"),
        )
        .map_err(|e| AurixError::Internal(format!("oidc discovery {name}: {e}")))
    }

    async fn get_json(&self, url: &str, bearer: Option<&str>) -> Result<serde_json::Value> {
        let mut req = self.http.get(url).header("accept", "application/json");
        if let Some(token) = bearer {
            req = req.bearer_auth(token);
        }
        let resp = req
            .send()
            .await
            .map_err(|e| AurixError::Internal(format!("oidc request to {url} failed: {e}")))?;
        let status = resp.status();
        let bytes = read_capped(resp).await?;
        if !status.is_success() {
            return Err(AurixError::Internal(format!(
                "oidc request to {url} returned {status}"
            )));
        }
        serde_json::from_slice(&bytes)
            .map_err(|e| AurixError::Internal(format!("oidc response from {url}: {e}")))
    }

    async fn refresh_jwks(&self, force: bool) -> Result<()> {
        {
            let mut last = self.jwks_last_fetch.lock();
            if let Some(at) = *last {
                if force && at.elapsed() < JWKS_REFETCH_MIN_INTERVAL {
                    return Ok(());
                }
            }
            *last = Some(Instant::now());
        }
        let disc = self.discovery().await?;
        let body = self.get_json(&disc.jwks_uri, None).await?;
        let raw_keys = body
            .get("keys")
            .and_then(|k| k.as_array())
            .cloned()
            .unwrap_or_default();
        let mut keys = HashMap::new();
        let mut anonymous = Vec::new();
        for raw in raw_keys {
            let Ok(jwk) = serde_json::from_value::<Jwk>(raw) else {
                continue;
            };
            if matches!(jwk.algorithm, AlgorithmParameters::OctetKey(_)) {
                continue;
            }
            let Some(alg) = jwk_algorithm(&jwk) else {
                continue;
            };
            let Ok(key) = DecodingKey::from_jwk(&jwk) else {
                continue;
            };
            match jwk.common.key_id {
                Some(kid) => {
                    keys.insert(kid, (alg, key));
                }
                None => anonymous.push((alg, key)),
            }
        }
        if keys.is_empty() && anonymous.is_empty() {
            return Err(AurixError::Internal(
                "oidc JWKS contains no usable asymmetric keys".into(),
            ));
        }
        *self.jwks.write() = Some(CachedJwks {
            keys,
            anonymous,
            fetched_at: Instant::now(),
        });
        Ok(())
    }

    /// Best-effort warm-up at start-up so misconfiguration shows in the logs early.
    pub async fn warm_up(&self) -> Result<()> {
        self.discovery().await?;
        self.refresh_jwks(false).await
    }

    // ── Login start ──

    pub async fn begin(&self, return_to: Option<&str>) -> Result<LoginStart> {
        let disc = self.discovery().await?;
        self.start_login_with(&disc, return_to)
    }

    fn start_login_with(&self, disc: &Discovery, return_to: Option<&str>) -> Result<LoginStart> {
        let return_to = match return_to.map(str::trim).filter(|s| !s.is_empty()) {
            Some(path) if path.starts_with('/') && !path.starts_with("//") && path.len() <= 512 => {
                Some(path.to_string())
            }
            Some(_) => {
                return Err(AurixError::Validation(
                    "return_to must be a relative path".into(),
                ))
            }
            None => None,
        };
        let verifier = b64url().encode(self.crypto.generate_random_bytes(32)?);
        let challenge = b64url().encode(Sha256::digest(verifier.as_bytes()));
        let nonce = b64url().encode(self.crypto.generate_random_bytes(16)?);
        let cookie_value = b64url().encode(self.crypto.generate_random_bytes(32)?);
        let jti = b64url().encode(self.crypto.generate_random_bytes(12)?);
        let exp = chrono::Utc::now().timestamp() + self.cfg.state_ttl_secs as i64;
        let state = self.seal_state(&SealedState {
            jti,
            nonce: nonce.clone(),
            pkce_verifier: verifier,
            cookie_hash: hash_cookie(&cookie_value),
            exp,
            return_to,
        })?;

        let mut url = Url::parse(&disc.authorization_endpoint)
            .map_err(|e| AurixError::Internal(format!("authorization_endpoint: {e}")))?;
        url.query_pairs_mut()
            .append_pair("response_type", "code")
            .append_pair("client_id", &self.cfg.client_id)
            .append_pair("redirect_uri", &self.cfg.redirect_url)
            .append_pair("scope", &self.cfg.scopes.join(" "))
            .append_pair("state", &state)
            .append_pair("nonce", &nonce)
            .append_pair("code_challenge", &challenge)
            .append_pair("code_challenge_method", "S256");
        Ok(LoginStart {
            authorization_url: url.to_string(),
            cookie_value,
            cookie_max_age_secs: self.cfg.state_ttl_secs,
        })
    }

    fn seal_state(&self, state: &SealedState) -> Result<String> {
        let plain = serde_json::to_vec(state)
            .map_err(|e| AurixError::Internal(format!("state encode: {e}")))?;
        let sealed = self
            .crypto
            .encrypt_aes256gcm(&self.state_key, &plain, STATE_AAD)?;
        Ok(b64url().encode(sealed))
    }

    fn open_state(&self, state: &str) -> Result<SealedState> {
        if state.len() > 4096 {
            return Err(AurixError::AuthenticationFailed(
                "Invalid OIDC state".into(),
            ));
        }
        let sealed = b64url()
            .decode(state)
            .map_err(|_| AurixError::AuthenticationFailed("Invalid OIDC state".into()))?;
        let plain = self
            .crypto
            .decrypt_aes256gcm(&self.state_key, &sealed, STATE_AAD)
            .map_err(|_| AurixError::AuthenticationFailed("Invalid OIDC state".into()))?;
        let parsed: SealedState = serde_json::from_slice(&plain)
            .map_err(|_| AurixError::AuthenticationFailed("Invalid OIDC state".into()))?;
        if parsed.exp < chrono::Utc::now().timestamp() {
            return Err(AurixError::AuthenticationFailed(
                "OIDC login attempt expired; start again".into(),
            ));
        }
        Ok(parsed)
    }

    fn consume_state(&self, state: &SealedState) -> Result<()> {
        let now = chrono::Utc::now().timestamp();
        let mut used = self.used_states.lock();
        used.retain(|_, exp| *exp >= now);
        if used.insert(state.jti.clone(), state.exp).is_some() {
            return Err(AurixError::AuthenticationFailed(
                "OIDC state already used".into(),
            ));
        }
        Ok(())
    }

    // ── Callback ──

    /// Exchanges the authorization code, verifies the ID token against the sealed state and
    /// the login cookie, and maps the claims onto a local role.
    pub async fn complete(
        &self,
        code: &str,
        state: &str,
        cookie_value: Option<&str>,
    ) -> Result<VerifiedIdentity> {
        let state = self.open_state(state)?;
        let cookie_ok = cookie_value.is_some_and(|c| {
            constant_time_eq(hash_cookie(c).as_bytes(), state.cookie_hash.as_bytes())
        });
        if !cookie_ok {
            return Err(AurixError::AuthenticationFailed(
                "OIDC login cookie missing or mismatched; start the login from this browser".into(),
            ));
        }
        self.consume_state(&state)?;
        if code.is_empty() || code.len() > 4096 {
            return Err(AurixError::AuthenticationFailed(
                "Invalid authorization code".into(),
            ));
        }

        let disc = self.discovery().await?;
        let tokens = self
            .exchange_code(&disc, code, &state.pkce_verifier)
            .await?;
        let id_token = tokens.id_token.as_deref().ok_or_else(|| {
            AurixError::AuthenticationFailed("Provider returned no id_token".into())
        })?;
        let mut claims = self.verify_id_token(id_token).await?;
        match &claims.nonce {
            Some(n) if constant_time_eq(n.as_bytes(), state.nonce.as_bytes()) => {}
            _ => {
                return Err(AurixError::AuthenticationFailed(
                    "ID token nonce mismatch".into(),
                ))
            }
        }
        if claims.rest.get("aud").is_some_and(|aud| aud.is_array())
            && claims
                .azp
                .as_deref()
                .is_some_and(|azp| azp != self.cfg.client_id)
        {
            return Err(AurixError::AuthenticationFailed(
                "ID token authorized party mismatch".into(),
            ));
        }

        let needs_userinfo = claim_str(&claims.rest, &self.cfg.email_claim).is_none()
            || claim_strings(&claims.rest, &self.cfg.groups_claim).is_empty();
        if needs_userinfo {
            if let (Some(endpoint), Some(access)) =
                (&disc.userinfo_endpoint, tokens.access_token.as_deref())
            {
                match self.get_json(endpoint, Some(access)).await {
                    Ok(serde_json::Value::Object(info)) => {
                        let sub_ok = info
                            .get("sub")
                            .and_then(|s| s.as_str())
                            .is_some_and(|s| s == claims.sub);
                        if sub_ok {
                            for (k, v) in info {
                                claims.rest.entry(k).or_insert(v);
                            }
                        } else {
                            tracing::warn!("oidc userinfo subject mismatch; ignoring userinfo");
                        }
                    }
                    Ok(_) => tracing::warn!("oidc userinfo returned a non-object"),
                    Err(e) => tracing::warn!("oidc userinfo failed: {e}"),
                }
            }
        }

        let email = claim_str(&claims.rest, &self.cfg.email_claim)
            .map(|e| e.trim().to_ascii_lowercase())
            .filter(|e| !e.is_empty())
            .ok_or_else(|| {
                AurixError::AuthenticationFailed(format!(
                    "ID token has no {:?} claim",
                    self.cfg.email_claim
                ))
            })?;
        if self.cfg.require_email_verified
            && claims.rest.get("email_verified").and_then(|v| match v {
                serde_json::Value::Bool(b) => Some(*b),
                serde_json::Value::String(s) => Some(s == "true"),
                _ => None,
            }) != Some(true)
        {
            return Err(AurixError::AuthenticationFailed(
                "Email is not verified by the identity provider".into(),
            ));
        }
        if !self.cfg.allowed_domains.is_empty() {
            let domain = email.rsplit('@').next().unwrap_or_default();
            if !self
                .cfg
                .allowed_domains
                .iter()
                .any(|d| d.trim_start_matches('@').eq_ignore_ascii_case(domain))
            {
                return Err(AurixError::AuthorizationDenied(format!(
                    "Email domain {domain:?} is not allowed to administer this deployment"
                )));
            }
        }
        let groups = claim_strings(&claims.rest, &self.cfg.groups_claim);
        let role = self.resolve_role(&email, &groups).ok_or_else(|| {
            AurixError::AuthorizationDenied(
                "Your account is not mapped to an administrator role".into(),
            )
        })?;
        let display_name = ["name", "preferred_username", "given_name"]
            .iter()
            .find_map(|k| claim_str(&claims.rest, k))
            .map(str::to_string)
            .unwrap_or_else(|| email.split('@').next().unwrap_or("admin").to_string());
        Ok(VerifiedIdentity {
            issuer: claims.iss,
            subject: claims.sub,
            email,
            display_name,
            groups,
            role,
            return_to: state.return_to,
        })
    }

    /// `superadmin_emails` win, then the highest role among mapped groups, then `default_role`.
    pub fn resolve_role(&self, email: &str, groups: &[String]) -> Option<AdminRole> {
        if self
            .cfg
            .superadmin_emails
            .iter()
            .any(|e| e.trim().eq_ignore_ascii_case(email))
        {
            return Some(AdminRole::Superadmin);
        }
        let mapped = groups
            .iter()
            .filter_map(|g| self.cfg.role_mapping.get(g))
            .filter_map(|r| AdminRole::parse(r))
            .max();
        mapped.or_else(|| AdminRole::parse(&self.cfg.default_role))
    }

    async fn exchange_code(
        &self,
        disc: &Discovery,
        code: &str,
        verifier: &str,
    ) -> Result<TokenResponse> {
        let mut form: Vec<(&str, &str)> = vec![
            ("grant_type", "authorization_code"),
            ("code", code),
            ("redirect_uri", &self.cfg.redirect_url),
            ("client_id", &self.cfg.client_id),
            ("code_verifier", verifier),
        ];
        let mut req = self
            .http
            .post(&disc.token_endpoint)
            .header("accept", "application/json");
        if let Some(secret) = self.cfg.client_secret.as_deref().filter(|s| !s.is_empty()) {
            let methods = &disc.token_endpoint_auth_methods_supported;
            let use_post = !methods.is_empty()
                && !methods.iter().any(|m| m == "client_secret_basic")
                && methods.iter().any(|m| m == "client_secret_post");
            if use_post {
                form.push(("client_secret", secret));
            } else {
                req = req.basic_auth(&self.cfg.client_id, Some(secret));
            }
        }
        let resp = req
            .form(&form)
            .send()
            .await
            .map_err(|e| AurixError::Internal(format!("oidc token endpoint: {e}")))?;
        let status = resp.status();
        let bytes = read_capped(resp).await?;
        let parsed: TokenResponse = serde_json::from_slice(&bytes).map_err(|_| {
            AurixError::Internal(format!(
                "oidc token endpoint returned {status} with an unreadable body"
            ))
        })?;
        if !status.is_success() || parsed.error.is_some() {
            let err = parsed.error.unwrap_or_else(|| status.to_string());
            let desc = parsed.error_description.unwrap_or_default();
            return Err(AurixError::AuthenticationFailed(format!(
                "Provider refused the authorization code: {err} {desc}"
            )));
        }
        Ok(parsed)
    }

    async fn verify_id_token(&self, token: &str) -> Result<IdTokenClaims> {
        let header = decode_header(token)
            .map_err(|_| AurixError::AuthenticationFailed("Malformed ID token".into()))?;
        if !ALLOWED_ALGS.contains(&header.alg) {
            return Err(AurixError::AuthenticationFailed(format!(
                "ID token algorithm {:?} not accepted",
                header.alg
            )));
        }
        let stale = self
            .jwks
            .read()
            .as_ref()
            .is_none_or(|c| c.fetched_at.elapsed() > JWKS_TTL);
        if stale {
            self.refresh_jwks(false).await?;
        }
        if let Some(claims) = self.try_verify(token, &header)? {
            return Ok(claims);
        }
        // Unknown kid: the provider may have rotated keys since the last fetch.
        self.refresh_jwks(true).await?;
        self.try_verify(token, &header)?.ok_or_else(|| {
            AurixError::AuthenticationFailed("ID token signed with an unknown key".into())
        })
    }

    /// `Ok(None)` when no candidate key matches the header (`kid` unknown).
    fn try_verify(
        &self,
        token: &str,
        header: &jsonwebtoken::Header,
    ) -> Result<Option<IdTokenClaims>> {
        let guard = self.jwks.read();
        let Some(cache) = guard.as_ref() else {
            return Ok(None);
        };
        let candidates: Vec<&(Algorithm, DecodingKey)> = match &header.kid {
            Some(kid) => cache.keys.get(kid).into_iter().collect(),
            None => cache.anonymous.iter().collect(),
        };
        if candidates.is_empty() {
            return Ok(None);
        }
        let mut last_err = None;
        for (key_alg, key) in candidates {
            if *key_alg != header.alg && !same_family(*key_alg, header.alg) {
                continue;
            }
            let mut validation = Validation::new(header.alg);
            validation.set_issuer(&[self.issuer(), &format!("{}/", self.issuer())]);
            validation.set_audience(&[&self.cfg.client_id]);
            validation.set_required_spec_claims(&["exp", "iss", "aud", "sub", "iat"]);
            validation.leeway = 60;
            match decode::<IdTokenClaims>(token, key, &validation) {
                Ok(data) => {
                    let iat = data
                        .claims
                        .rest
                        .get("iat")
                        .and_then(|v| v.as_i64())
                        .unwrap_or_default();
                    if iat > chrono::Utc::now().timestamp() + 300 {
                        return Err(AurixError::AuthenticationFailed(
                            "ID token issued in the future".into(),
                        ));
                    }
                    return Ok(Some(data.claims));
                }
                Err(e) => last_err = Some(e),
            }
        }
        match last_err {
            Some(e) => Err(AurixError::AuthenticationFailed(format!(
                "ID token rejected: {e}"
            ))),
            None => Ok(None),
        }
    }
}

fn same_family(a: Algorithm, b: Algorithm) -> bool {
    fn family(alg: Algorithm) -> u8 {
        match alg {
            Algorithm::RS256 | Algorithm::RS384 | Algorithm::RS512 => 1,
            Algorithm::PS256 | Algorithm::PS384 | Algorithm::PS512 => 1,
            Algorithm::ES256 | Algorithm::ES384 => 2,
            Algorithm::EdDSA => 3,
            Algorithm::HS256 | Algorithm::HS384 | Algorithm::HS512 => 4,
        }
    }
    family(a) == family(b)
}

/// Algorithm a JWK is usable for: its `alg` when present, otherwise inferred from the key type
/// (RSA keys default to RS256 and are also accepted for the other RSA algorithms).
fn jwk_algorithm(jwk: &Jwk) -> Option<Algorithm> {
    use jsonwebtoken::jwk::{EllipticCurve, KeyAlgorithm};
    if let Some(alg) = jwk.common.key_algorithm {
        return match alg {
            KeyAlgorithm::RS256 => Some(Algorithm::RS256),
            KeyAlgorithm::RS384 => Some(Algorithm::RS384),
            KeyAlgorithm::RS512 => Some(Algorithm::RS512),
            KeyAlgorithm::PS256 => Some(Algorithm::PS256),
            KeyAlgorithm::PS384 => Some(Algorithm::PS384),
            KeyAlgorithm::PS512 => Some(Algorithm::PS512),
            KeyAlgorithm::ES256 => Some(Algorithm::ES256),
            KeyAlgorithm::ES384 => Some(Algorithm::ES384),
            KeyAlgorithm::EdDSA => Some(Algorithm::EdDSA),
            _ => None,
        };
    }
    match &jwk.algorithm {
        AlgorithmParameters::RSA(_) => Some(Algorithm::RS256),
        AlgorithmParameters::EllipticCurve(p) => match p.curve {
            EllipticCurve::P256 => Some(Algorithm::ES256),
            EllipticCurve::P384 => Some(Algorithm::ES384),
            _ => None,
        },
        AlgorithmParameters::OctetKeyPair(_) => Some(Algorithm::EdDSA),
        AlgorithmParameters::OctetKey(_) => None,
    }
}

fn hash_cookie(value: &str) -> String {
    b64url().encode(Sha256::digest(value.as_bytes()))
}

async fn read_capped(resp: reqwest::Response) -> Result<Vec<u8>> {
    if resp
        .content_length()
        .is_some_and(|len| len > MAX_BODY_BYTES as u64)
    {
        return Err(AurixError::Internal("oidc response too large".into()));
    }
    let bytes = resp
        .bytes()
        .await
        .map_err(|e| AurixError::Internal(format!("oidc response body: {e}")))?;
    if bytes.len() > MAX_BODY_BYTES {
        return Err(AurixError::Internal("oidc response too large".into()));
    }
    Ok(bytes.to_vec())
}

/// Looks a claim up by name, supporting dotted paths into nested objects
/// (`resource_access.aurix.roles`).
fn claim_value<'a>(
    claims: &'a BTreeMap<String, serde_json::Value>,
    path: &str,
) -> Option<&'a serde_json::Value> {
    if let Some(direct) = claims.get(path) {
        return Some(direct);
    }
    let mut parts = path.split('.');
    let mut cur = claims.get(parts.next()?)?;
    for part in parts {
        cur = cur.get(part)?;
    }
    Some(cur)
}

fn claim_str<'a>(claims: &'a BTreeMap<String, serde_json::Value>, path: &str) -> Option<&'a str> {
    claim_value(claims, path).and_then(|v| v.as_str())
}

/// A claim that is either an array of strings or one string with space/comma separators.
fn claim_strings(claims: &BTreeMap<String, serde_json::Value>, path: &str) -> Vec<String> {
    match claim_value(claims, path) {
        Some(serde_json::Value::Array(items)) => items
            .iter()
            .filter_map(|v| v.as_str())
            .map(str::to_string)
            .collect(),
        Some(serde_json::Value::String(s)) => s
            .split([' ', ','])
            .filter(|p| !p.is_empty())
            .map(str::to_string)
            .collect(),
        _ => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> OidcConfig {
        OidcConfig {
            enabled: true,
            issuer: "https://idp.example.com/".into(),
            client_id: "aurix".into(),
            redirect_url: "https://api.example.com/admin/oidc/callback".into(),
            role_mapping: [
                ("ops".to_string(), "superadmin".to_string()),
                ("support".to_string(), "moderator".to_string()),
            ]
            .into_iter()
            .collect(),
            default_role: "viewer".into(),
            superadmin_emails: vec!["Root@Example.com".into()],
            ..OidcConfig::default()
        }
    }

    fn provider() -> OidcProvider {
        OidcProvider::new(cfg(), "unit-test-secret-of-sufficient-length!!").unwrap()
    }

    fn discovery() -> Discovery {
        Discovery {
            issuer: "https://idp.example.com".into(),
            authorization_endpoint: "https://idp.example.com/auth".into(),
            token_endpoint: "https://idp.example.com/token".into(),
            jwks_uri: "https://idp.example.com/jwks".into(),
            userinfo_endpoint: None,
            end_session_endpoint: None,
            token_endpoint_auth_methods_supported: vec![],
            code_challenge_methods_supported: vec!["S256".into()],
            id_token_signing_alg_values_supported: vec![],
        }
    }

    #[test]
    fn role_resolution_prefers_superadmin_emails_then_highest_group() {
        let p = provider();
        assert_eq!(
            p.resolve_role("root@example.com", &[]),
            Some(AdminRole::Superadmin)
        );
        assert_eq!(
            p.resolve_role("a@example.com", &["support".into(), "ops".into()]),
            Some(AdminRole::Superadmin)
        );
        assert_eq!(
            p.resolve_role("a@example.com", &["support".into()]),
            Some(AdminRole::Moderator)
        );
        assert_eq!(
            p.resolve_role("a@example.com", &["unknown".into()]),
            Some(AdminRole::Viewer)
        );
        let mut strict = cfg();
        strict.default_role.clear();
        let p = OidcProvider::new(strict, "s").unwrap();
        assert_eq!(p.resolve_role("a@example.com", &["unknown".into()]), None);
    }

    #[test]
    fn state_round_trip_binds_cookie_and_expires() {
        let p = provider();
        let start = p.start_login_with(&discovery(), Some("/apps")).unwrap();
        let url = Url::parse(&start.authorization_url).unwrap();
        let q: HashMap<_, _> = url.query_pairs().into_owned().collect();
        assert_eq!(q["response_type"], "code");
        assert_eq!(q["code_challenge_method"], "S256");
        assert_eq!(q["client_id"], "aurix");
        let state = p.open_state(&q["state"]).unwrap();
        assert_eq!(state.nonce, q["nonce"]);
        assert_eq!(state.return_to.as_deref(), Some("/apps"));
        assert!(constant_time_eq(
            state.cookie_hash.as_bytes(),
            hash_cookie(&start.cookie_value).as_bytes()
        ));
        let expected = b64url().encode(Sha256::digest(state.pkce_verifier.as_bytes()));
        assert_eq!(q["code_challenge"], expected);

        // Tampering / another node's secret.
        let other = OidcProvider::new(cfg(), "another-secret").unwrap();
        assert!(other.open_state(&q["state"]).is_err());
        let mut broken = q["state"].clone();
        broken.replace_range(10..12, "AA");
        assert!(p.open_state(&broken).is_err());

        // Single use per node.
        p.consume_state(&state).unwrap();
        assert!(p.consume_state(&state).is_err());

        // Expired state.
        let expired = p
            .seal_state(&SealedState {
                jti: "x".into(),
                nonce: "n".into(),
                pkce_verifier: "v".into(),
                cookie_hash: "c".into(),
                exp: chrono::Utc::now().timestamp() - 1,
                return_to: None,
            })
            .unwrap();
        assert!(p.open_state(&expired).is_err());
    }

    #[test]
    fn return_to_must_be_relative() {
        let p = provider();
        assert!(p
            .start_login_with(&discovery(), Some("https://evil.example"))
            .is_err());
        assert!(p.start_login_with(&discovery(), Some("//evil")).is_err());
        assert!(p.start_login_with(&discovery(), Some("/ok")).is_ok());
    }

    #[test]
    fn claims_support_nested_paths_and_string_lists() {
        let claims: BTreeMap<String, serde_json::Value> =
            serde_json::from_value(serde_json::json!({
                "email": "a@b.c",
                "groups": "ops, support",
                "resource_access": { "aurix": { "roles": ["admin", "viewer"] } }
            }))
            .unwrap();
        assert_eq!(claim_str(&claims, "email"), Some("a@b.c"));
        assert_eq!(claim_strings(&claims, "groups"), vec!["ops", "support"]);
        assert_eq!(
            claim_strings(&claims, "resource_access.aurix.roles"),
            vec!["admin", "viewer"]
        );
        assert!(claim_strings(&claims, "missing").is_empty());
    }

    #[test]
    fn jwks_ignores_symmetric_and_unknown_keys() {
        let raw = serde_json::json!({
            "kty": "oct", "k": "c2VjcmV0", "alg": "HS256", "kid": "sym"
        });
        let jwk: Jwk = serde_json::from_value(raw).unwrap();
        assert!(matches!(jwk.algorithm, AlgorithmParameters::OctetKey(_)));
        assert_eq!(jwk_algorithm(&jwk), None);
    }
}
