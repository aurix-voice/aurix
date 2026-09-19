//! Tenant webhooks: subscriptions on `/v1/webhooks`, an at-least-once delivery queue in
//! Postgres shared by every node, HMAC-SHA256 request signatures, bounded retries with
//! backoff and `webhook.resync` snapshots.
//!
//! Flow: the node that publishes a [`ServerEvent`] (`EventBus::subscribe_outbound`, so remote
//! replicas are not enqueued twice) writes one `webhook_deliveries` row per matching enabled
//! subscription; every node's worker leases due rows (`FOR UPDATE SKIP LOCKED` + lease) and
//! POSTs them. A 2xx marks the row delivered, anything else schedules the next attempt per
//! `webhooks.retry_delays_secs` until the attempts are exhausted.
//!
//! Signature: `X-Aurix-Signature: t=<unix seconds>,v1=<hex(HMAC-SHA256(secret, "<t>.<body>"))>`
//! over the exact request body; receivers should reject stale `t` (5 min is a good tolerance)
//! and compare in constant time — see [`verify_signature`].

use crate::event_bus::{EventBus, ServerEvent};
use aurix_common::config::WebhooksConfig;
use aurix_common::error::AurixError;
use aurix_common::net::{is_private_ip, validate_outbound_url};
use aurix_common::types::*;
use aurix_db::models::{WebhookDeliveryRow, WebhookSubscriptionRow};
use aurix_db::DbPool;
use chrono::{DateTime, Utc};
use dashmap::DashMap;
use futures_util::StreamExt;
use hmac::{Hmac, Mac};
use rand::Rng;
use sha2::Sha256;
use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::{Duration, Instant};
use subtle::ConstantTimeEq;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};
use url::{Host, Url};

type HmacSha256 = Hmac<Sha256>;

pub const SIGNATURE_HEADER: &str = "x-aurix-signature";
pub const EVENT_HEADER: &str = "x-aurix-event";
pub const DELIVERY_HEADER: &str = "x-aurix-delivery-id";
pub const WEBHOOK_HEADER: &str = "x-aurix-webhook-id";
pub const ATTEMPT_HEADER: &str = "x-aurix-attempt";
pub const USER_AGENT: &str = concat!("aurix-webhooks/", env!("CARGO_PKG_VERSION"));

/// Synthetic types produced by the service itself (never by the event bus).
pub const TEST_EVENT: &str = "webhook.test";
pub const RESYNC_EVENT: &str = "webhook.resync";

const MAX_EVENTS_PER_SUBSCRIPTION: usize = 64;
const MAX_DESCRIPTION_LEN: usize = 256;
const SUBSCRIPTION_CACHE_TTL: Duration = Duration::from_secs(10);
const ENQUEUE_BUFFER: usize = 8192;
const MAX_ERROR_LEN: usize = 512;

/// Content-derived id: first 128 bits of `SHA-256("aurix-event\0" || serialized event)`, laid
/// out as a version-8 (custom) UUID.
fn content_event_id(raw: &[u8]) -> uuid::Uuid {
    use sha2::Digest;
    let mut h = Sha256::new();
    h.update(b"aurix-event\0");
    h.update(raw);
    let digest = h.finalize();
    let mut bytes = [0u8; 16];
    bytes.copy_from_slice(&digest[..16]);
    bytes[6] = (bytes[6] & 0x0f) | 0x80;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    uuid::Uuid::from_bytes(bytes)
}

/// Tenant-facing envelope of every webhook body and SSE frame. `id` is derived from the event
/// contents, so the same event carries the same id on every retry, every webhook of the tenant
/// and every SSE stream (idempotency key for receivers).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct PublicEvent {
    pub id: uuid::Uuid,
    #[serde(rename = "type")]
    pub event_type: String,
    pub app_id: AppId,
    pub created_at: DateTime<Utc>,
    pub data: serde_json::Value,
}

impl PublicEvent {
    /// `None` for node-scoped events that are never exported.
    pub fn from_event(event: &ServerEvent) -> Option<Self> {
        let event_type = event.public_type()?;
        let raw = serde_json::to_vec(event).ok()?;
        Some(Self {
            id: content_event_id(&raw),
            event_type: event_type.to_string(),
            app_id: event.app_id()?,
            created_at: event_timestamp(event).unwrap_or_else(Utc::now),
            data: event.public_data(),
        })
    }

    fn synthetic(app_id: AppId, event_type: &str, data: serde_json::Value) -> Self {
        Self {
            id: uuid::Uuid::now_v7(),
            event_type: event_type.to_string(),
            app_id,
            created_at: Utc::now(),
            data,
        }
    }
}

fn event_timestamp(event: &ServerEvent) -> Option<DateTime<Utc>> {
    let v = serde_json::to_value(event).ok()?;
    let ts = v.get("payload")?.get("timestamp")?.as_str()?;
    ts.parse().ok()
}

/// Computes the `X-Aurix-Signature` value for `body` at `timestamp` (unix seconds).
pub fn sign(secret: &str, timestamp: i64, body: &[u8]) -> String {
    let mut mac = HmacSha256::new_from_slice(secret.as_bytes()).expect("HMAC accepts any key");
    mac.update(timestamp.to_string().as_bytes());
    mac.update(b".");
    mac.update(body);
    format!(
        "t={timestamp},v1={}",
        hex::encode(mac.finalize().into_bytes())
    )
}

/// Receiver-side check: parses `t=…,v1=…`, rejects timestamps further than `tolerance` from
/// `now` and compares the digest in constant time.
pub fn verify_signature(
    secret: &str,
    header: &str,
    body: &[u8],
    now: DateTime<Utc>,
    tolerance: Duration,
) -> bool {
    let mut timestamp: Option<i64> = None;
    let mut digest: Option<&str> = None;
    for part in header.split(',') {
        match part.trim().split_once('=') {
            Some(("t", v)) => timestamp = v.parse().ok(),
            Some(("v1", v)) => digest = Some(v),
            _ => {}
        }
    }
    let (Some(t), Some(v1)) = (timestamp, digest) else {
        return false;
    };
    if (now.timestamp() - t).unsigned_abs() > tolerance.as_secs() {
        return false;
    }
    let expected = sign(secret, t, body);
    let expected = expected.rsplit_once("v1=").map(|(_, d)| d).unwrap_or("");
    let (Ok(a), Ok(b)) = (hex::decode(expected), hex::decode(v1)) else {
        return false;
    };
    a.len() == b.len() && a.ct_eq(&b).into()
}

fn generate_secret() -> String {
    let mut rng = rand::thread_rng();
    let bytes: [u8; 32] = rng.gen();
    format!("whsec_{}", hex::encode(bytes))
}

struct CachedSubscriptions {
    fetched: Instant,
    subs: Arc<Vec<WebhookSubscriptionRow>>,
}

pub struct WebhookService {
    cfg: WebhooksConfig,
    production: bool,
    pool: DbPool,
    events: Arc<EventBus>,
    cache: DashMap<uuid::Uuid, CachedSubscriptions>,
    http: reqwest::Client,
}

pub struct CreateWebhook {
    pub url: String,
    pub events: Vec<String>,
    pub description: Option<String>,
}

pub struct UpdateWebhook {
    pub url: Option<String>,
    pub events: Option<Vec<String>>,
    pub description: Option<Option<String>>,
    pub enabled: Option<bool>,
}

impl WebhookService {
    pub fn new(
        cfg: WebhooksConfig,
        production: bool,
        pool: DbPool,
        events: Arc<EventBus>,
    ) -> Result<Self, AurixError> {
        let http = Self::client_builder(&cfg)
            .build()
            .map_err(|e| AurixError::Internal(format!("webhook HTTP client: {e}")))?;
        Ok(Self {
            cfg,
            production,
            pool,
            events,
            cache: DashMap::new(),
            http,
        })
    }

    fn client_builder(cfg: &WebhooksConfig) -> reqwest::ClientBuilder {
        reqwest::Client::builder()
            .user_agent(USER_AGENT)
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_millis(cfg.timeout_ms))
            .connect_timeout(Duration::from_millis(cfg.timeout_ms.min(3000)))
    }

    pub fn enabled(&self) -> bool {
        self.cfg.enabled
    }

    pub fn config(&self) -> &WebhooksConfig {
        &self.cfg
    }

    fn private_urls_allowed(&self) -> bool {
        self.cfg.private_urls_allowed(self.production)
    }

    // ── Subscription management ──

    /// Syntactic checks plus the SSRF guard on literal IP hosts. Host names are resolved at
    /// delivery time (and pinned), so a name that later points at a private range is refused
    /// then.
    pub fn validate_url(&self, raw: &str) -> Result<Url, AurixError> {
        validate_outbound_url(
            raw,
            "https",
            "http",
            self.cfg.https_required(self.production),
            self.private_urls_allowed(),
            "webhooks.require_https",
        )
    }

    fn validate_events(events: &[String]) -> Result<Vec<String>, AurixError> {
        if events.is_empty() {
            return Err(AurixError::Validation(
                "events must list at least one event type (or \"*\")".into(),
            ));
        }
        if events.len() > MAX_EVENTS_PER_SUBSCRIPTION {
            return Err(AurixError::Validation("too many event types".into()));
        }
        let mut out: Vec<String> = Vec::with_capacity(events.len());
        for e in events {
            let e = e.trim();
            if !ServerEvent::webhook_type_allowed(e) {
                return Err(AurixError::Validation(format!(
                    "unknown or unsupported webhook event type '{e}'"
                )));
            }
            if !out.iter().any(|x| x == e) {
                out.push(e.to_string());
            }
        }
        Ok(out)
    }

    fn validate_description(d: Option<String>) -> Result<Option<String>, AurixError> {
        match d {
            Some(d) if d.chars().count() > MAX_DESCRIPTION_LEN => {
                Err(AurixError::Validation("description is too long".into()))
            }
            Some(d) if d.trim().is_empty() => Ok(None),
            other => Ok(other),
        }
    }

    /// Returns the stored row and the plaintext secret (shown once).
    pub async fn create(
        &self,
        app_id: AppId,
        req: CreateWebhook,
    ) -> Result<WebhookSubscriptionRow, AurixError> {
        self.require_enabled()?;
        let url = self.validate_url(&req.url)?;
        let events = Self::validate_events(&req.events)?;
        let description = Self::validate_description(req.description)?;
        let count = aurix_db::queries::count_webhooks(&self.pool, app_id.0)
            .await
            .map_err(db_err)?;
        if count >= self.cfg.max_subscriptions_per_app as i64 {
            return Err(AurixError::Conflict(format!(
                "at most {} webhooks per application",
                self.cfg.max_subscriptions_per_app
            )));
        }
        let now = Utc::now();
        let row = WebhookSubscriptionRow {
            id: uuid::Uuid::now_v7(),
            app_id: app_id.0,
            url: url.to_string(),
            secret: generate_secret(),
            events,
            description,
            enabled: true,
            consecutive_failures: 0,
            last_delivery_at: None,
            last_status: None,
            last_error: None,
            created_at: now,
            updated_at: now,
        };
        let row = aurix_db::queries::insert_webhook(&self.pool, &row)
            .await
            .map_err(db_err)?;
        self.invalidate(app_id);
        Ok(row)
    }

    pub async fn get(
        &self,
        app_id: AppId,
        id: uuid::Uuid,
    ) -> Result<WebhookSubscriptionRow, AurixError> {
        aurix_db::queries::get_webhook(&self.pool, app_id.0, id)
            .await
            .map_err(db_err)?
            .ok_or_else(|| AurixError::NotFound("webhook not found".into()))
    }

    pub async fn list(&self, app_id: AppId) -> Result<Vec<WebhookSubscriptionRow>, AurixError> {
        aurix_db::queries::list_webhooks(&self.pool, app_id.0)
            .await
            .map_err(db_err)
    }

    pub async fn update(
        &self,
        app_id: AppId,
        id: uuid::Uuid,
        req: UpdateWebhook,
    ) -> Result<WebhookSubscriptionRow, AurixError> {
        let url = match &req.url {
            Some(u) => Some(self.validate_url(u)?.to_string()),
            None => None,
        };
        let events = match &req.events {
            Some(e) => Some(Self::validate_events(e)?),
            None => None,
        };
        let description = match req.description {
            Some(d) => Some(Self::validate_description(d)?),
            None => None,
        };
        let row = aurix_db::queries::update_webhook(
            &self.pool,
            app_id.0,
            id,
            url.as_deref(),
            events.as_deref(),
            description.as_ref().map(|d| d.as_deref()),
            req.enabled,
        )
        .await
        .map_err(db_err)?
        .ok_or_else(|| AurixError::NotFound("webhook not found".into()))?;
        self.invalidate(app_id);
        Ok(row)
    }

    /// Replaces the signing secret; the new one is in the returned row (shown once).
    pub async fn rotate_secret(
        &self,
        app_id: AppId,
        id: uuid::Uuid,
    ) -> Result<WebhookSubscriptionRow, AurixError> {
        let row =
            aurix_db::queries::rotate_webhook_secret(&self.pool, app_id.0, id, &generate_secret())
                .await
                .map_err(db_err)?
                .ok_or_else(|| AurixError::NotFound("webhook not found".into()))?;
        self.invalidate(app_id);
        Ok(row)
    }

    pub async fn delete(&self, app_id: AppId, id: uuid::Uuid) -> Result<(), AurixError> {
        let deleted = aurix_db::queries::delete_webhook(&self.pool, app_id.0, id)
            .await
            .map_err(db_err)?;
        self.invalidate(app_id);
        if deleted {
            Ok(())
        } else {
            Err(AurixError::NotFound("webhook not found".into()))
        }
    }

    pub async fn list_deliveries(
        &self,
        app_id: AppId,
        id: uuid::Uuid,
        status: Option<&str>,
        limit: i64,
        offset: i64,
    ) -> Result<Vec<WebhookDeliveryRow>, AurixError> {
        self.get(app_id, id).await?;
        if let Some(s) = status {
            if !matches!(s, "pending" | "delivered" | "failed") {
                return Err(AurixError::Validation(
                    "status must be pending, delivered or failed".into(),
                ));
            }
        }
        aurix_db::queries::list_webhook_deliveries(&self.pool, app_id.0, id, status, limit, offset)
            .await
            .map_err(db_err)
    }

    pub async fn get_delivery(
        &self,
        app_id: AppId,
        id: uuid::Uuid,
        delivery_id: uuid::Uuid,
    ) -> Result<WebhookDeliveryRow, AurixError> {
        aurix_db::queries::get_webhook_delivery(&self.pool, app_id.0, id, delivery_id)
            .await
            .map_err(db_err)?
            .ok_or_else(|| AurixError::NotFound("delivery not found".into()))
    }

    /// Re-sends a finished delivery from scratch (pending rows are left alone).
    pub async fn redeliver(
        &self,
        app_id: AppId,
        id: uuid::Uuid,
        delivery_id: uuid::Uuid,
    ) -> Result<WebhookDeliveryRow, AurixError> {
        let current = self.get_delivery(app_id, id, delivery_id).await?;
        if current.status == "pending" {
            return Err(AurixError::Conflict("delivery is still pending".into()));
        }
        aurix_db::queries::requeue_webhook_delivery(&self.pool, app_id.0, id, delivery_id)
            .await
            .map_err(db_err)?
            .ok_or_else(|| AurixError::NotFound("delivery not found".into()))
    }

    /// Enqueues a `webhook.test` event for one subscription (even a disabled one).
    pub async fn send_test(
        &self,
        app_id: AppId,
        id: uuid::Uuid,
    ) -> Result<WebhookDeliveryRow, AurixError> {
        self.require_enabled()?;
        let sub = self.get(app_id, id).await?;
        let env = PublicEvent::synthetic(
            app_id,
            TEST_EVENT,
            serde_json::json!({
                "webhook_id": sub.id,
                "message": "Aurix webhook test delivery",
            }),
        );
        self.enqueue_for(&sub, &env)
            .await?
            .ok_or_else(|| AurixError::Conflict("delivery queue for this webhook is full".into()))
    }

    /// Snapshot of every live channel and its members, delivered as one `webhook.resync`
    /// event so a consumer that missed events (downtime, dropped queue) can rebuild state.
    pub async fn resync(
        &self,
        app_id: AppId,
        id: uuid::Uuid,
    ) -> Result<WebhookDeliveryRow, AurixError> {
        self.require_enabled()?;
        let sub = self.get(app_id, id).await?;
        let snapshot = self.snapshot(app_id).await?;
        let env = PublicEvent::synthetic(app_id, RESYNC_EVENT, snapshot);
        self.enqueue_for(&sub, &env)
            .await?
            .ok_or_else(|| AurixError::Conflict("delivery queue for this webhook is full".into()))
    }

    /// `{"channels": [{channel_id, channel_type, participants: [...]}]}` from the database
    /// (cluster-wide, not just this node).
    pub async fn snapshot(&self, app_id: AppId) -> Result<serde_json::Value, AurixError> {
        let rows = aurix_db::queries::list_active_channel_members(&self.pool, app_id.0)
            .await
            .map_err(db_err)?;
        let mut channels: Vec<serde_json::Value> = Vec::new();
        let mut current: Option<(uuid::Uuid, String, Vec<serde_json::Value>)> = None;
        for r in rows {
            let same = current
                .as_ref()
                .is_some_and(|(id, _, _)| *id == r.channel_id);
            if !same {
                if let Some((id, ty, participants)) = current.take() {
                    channels.push(serde_json::json!({
                        "channel_id": id, "channel_type": ty, "participants": participants
                    }));
                }
                current = Some((r.channel_id, r.channel_type.clone(), Vec::new()));
            }
            if let Some((_, _, participants)) = current.as_mut() {
                participants.push(serde_json::json!({
                    "user_id": r.user_id,
                    "display_name": r.display_name,
                    "session_id": r.session_id,
                    "ssrc": r.ssrc as u32,
                    "role": r.role,
                    "is_muted": r.is_muted,
                    "is_server_muted": r.is_server_muted,
                    "joined_at": r.joined_at,
                }));
            }
        }
        if let Some((id, ty, participants)) = current.take() {
            channels.push(serde_json::json!({
                "channel_id": id, "channel_type": ty, "participants": participants
            }));
        }
        Ok(serde_json::json!({ "channels": channels, "snapshot_at": Utc::now() }))
    }

    fn require_enabled(&self) -> Result<(), AurixError> {
        if self.cfg.enabled {
            Ok(())
        } else {
            Err(AurixError::NotImplemented(
                "webhooks are disabled on this deployment (webhooks.enabled)".into(),
            ))
        }
    }

    /// Drops the local cache and tells the other nodes to do the same.
    fn invalidate(&self, app_id: AppId) {
        self.cache.remove(&app_id.0);
        self.events.publish(ServerEvent::WebhooksChanged { app_id });
    }

    async fn enabled_subscriptions(
        &self,
        app_id: AppId,
    ) -> Result<Arc<Vec<WebhookSubscriptionRow>>, AurixError> {
        if let Some(c) = self.cache.get(&app_id.0) {
            if c.fetched.elapsed() < SUBSCRIPTION_CACHE_TTL {
                return Ok(c.subs.clone());
            }
        }
        let subs = Arc::new(
            aurix_db::queries::list_enabled_webhooks(&self.pool, app_id.0)
                .await
                .map_err(db_err)?,
        );
        self.cache.insert(
            app_id.0,
            CachedSubscriptions {
                fetched: Instant::now(),
                subs: subs.clone(),
            },
        );
        Ok(subs)
    }

    // ── Enqueue ──

    async fn enqueue_for(
        &self,
        sub: &WebhookSubscriptionRow,
        env: &PublicEvent,
    ) -> Result<Option<WebhookDeliveryRow>, AurixError> {
        let row = WebhookDeliveryRow {
            id: uuid::Uuid::now_v7(),
            subscription_id: sub.id,
            app_id: sub.app_id,
            event_id: env.id,
            event_type: env.event_type.clone(),
            payload: serde_json::to_value(env).map_err(|e| AurixError::Internal(e.to_string()))?,
            status: "pending".into(),
            attempts: 0,
            next_attempt_at: Utc::now(),
            leased_until: None,
            last_status: None,
            last_error: None,
            created_at: Utc::now(),
            delivered_at: None,
        };
        let inserted = aurix_db::queries::enqueue_webhook_delivery(
            &self.pool,
            &row,
            self.cfg.max_pending_per_subscription as i64,
        )
        .await
        .map_err(db_err)?;
        if inserted {
            Ok(Some(row))
        } else {
            aurix_metrics::WEBHOOK_DELIVERIES
                .with_label_values(&["dropped"])
                .inc();
            warn!(
                webhook = %sub.id,
                "webhook queue full ({} pending); dropping {}",
                self.cfg.max_pending_per_subscription,
                env.event_type
            );
            Ok(None)
        }
    }

    /// Fans one bus event out to every enabled subscription of its tenant that wants it.
    pub async fn enqueue_event(&self, event: &ServerEvent) -> Result<usize, AurixError> {
        let Some(env) = PublicEvent::from_event(event) else {
            return Ok(0);
        };
        let subs = self.enabled_subscriptions(env.app_id).await?;
        let mut n = 0;
        for sub in subs.iter().filter(|s| s.wants(&env.event_type)) {
            if self.enqueue_for(sub, &env).await?.is_some() {
                n += 1;
            }
        }
        Ok(n)
    }

    // ── Delivery ──

    /// One delivery attempt. `Ok(status)` on 2xx; `Err((http status if any, message))`.
    pub async fn attempt(
        &self,
        sub: &WebhookSubscriptionRow,
        delivery: &WebhookDeliveryRow,
    ) -> Result<u16, (Option<u16>, String)> {
        let url = Url::parse(&sub.url).map_err(|e| (None, format!("invalid url: {e}")))?;
        let client = self.client_for(&url).await?;
        let body = serde_json::to_vec(&delivery.payload)
            .map_err(|e| (None, format!("payload serialization: {e}")))?;
        let signature = sign(&sub.secret, Utc::now().timestamp(), &body);
        let resp = client
            .post(url)
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .header(SIGNATURE_HEADER, signature)
            .header(EVENT_HEADER, &delivery.event_type)
            .header(DELIVERY_HEADER, delivery.id.to_string())
            .header(WEBHOOK_HEADER, sub.id.to_string())
            .header(ATTEMPT_HEADER, delivery.attempts.to_string())
            .body(body)
            .send()
            .await
            .map_err(|e| (None, truncate(&format!("{e}"), MAX_ERROR_LEN)))?;
        let status = resp.status();
        // Drain (bounded) so the connection can be reused; the body is otherwise ignored.
        let _ = tokio::time::timeout(Duration::from_millis(500), resp.bytes()).await;
        if status.is_success() {
            Ok(status.as_u16())
        } else {
            Err((Some(status.as_u16()), format!("HTTP {status}")))
        }
    }

    /// Shared client, or — when private targets are forbidden — one with the host pinned to the
    /// public addresses it resolved to right now, so DNS rebinding cannot redirect the request
    /// into the deployment's network.
    async fn client_for(&self, url: &Url) -> Result<reqwest::Client, (Option<u16>, String)> {
        if self.private_urls_allowed() {
            return Ok(self.http.clone());
        }
        let host = url
            .host_str()
            .ok_or_else(|| (None, "url has no host".to_string()))?;
        let port = url
            .port_or_known_default()
            .ok_or_else(|| (None, "url has no port".to_string()))?;
        match url.host() {
            Some(Host::Ipv4(ip)) if is_private_ip(IpAddr::V4(ip)) => {
                return Err((None, "target is a private address".into()))
            }
            Some(Host::Ipv6(ip)) if is_private_ip(IpAddr::V6(ip)) => {
                return Err((None, "target is a private address".into()))
            }
            Some(Host::Domain(_)) => {}
            _ => return Ok(self.http.clone()),
        }
        let resolved = tokio::net::lookup_host((host, port))
            .await
            .map_err(|e| (None, format!("dns: {e}")))?;
        let public: Vec<SocketAddr> = resolved.filter(|a| !is_private_ip(a.ip())).collect();
        if public.is_empty() {
            return Err((
                None,
                "host resolves only to private addresses; refusing (webhooks.allow_private_urls)"
                    .into(),
            ));
        }
        Self::client_builder(&self.cfg)
            .resolve_to_addrs(host, &public)
            .build()
            .map_err(|e| (None, format!("http client: {e}")))
    }

    fn next_attempt_at(&self, attempts_done: i32) -> Option<DateTime<Utc>> {
        // attempts_done ≥ 1 here; delay index n-1 gates attempt n+1.
        let idx = usize::try_from(attempts_done.max(1) - 1).ok()?;
        let delay = *self.cfg.retry_delays_secs.get(idx)?;
        Some(Utc::now() + chrono::Duration::seconds(delay as i64))
    }

    /// Leases and delivers one batch of due rows; returns how many were processed.
    pub async fn drain(&self) -> Result<usize, AurixError> {
        let lease_secs = (self.cfg.timeout_ms / 1000) as i64 * 2 + 30;
        let due = aurix_db::queries::lease_due_webhook_deliveries(
            &self.pool,
            self.cfg.batch_size as i64,
            lease_secs,
        )
        .await
        .map_err(db_err)?;
        if due.is_empty() {
            return Ok(0);
        }
        let n = due.len();
        aurix_metrics::WEBHOOK_PENDING.add(n as i64);
        let mut subs: HashMap<uuid::Uuid, Option<WebhookSubscriptionRow>> = HashMap::new();
        for d in &due {
            if let std::collections::hash_map::Entry::Vacant(slot) = subs.entry(d.subscription_id) {
                slot.insert(
                    aurix_db::queries::get_webhook(&self.pool, d.app_id, d.subscription_id)
                        .await
                        .map_err(db_err)?,
                );
            }
        }
        let subs = Arc::new(subs);
        futures_util::stream::iter(due)
            .for_each_concurrent(self.cfg.concurrency, |d| {
                let subs = subs.clone();
                async move {
                    let sub = subs.get(&d.subscription_id).and_then(|s| s.as_ref());
                    self.deliver_leased(sub, d).await;
                }
            })
            .await;
        aurix_metrics::WEBHOOK_PENDING.sub(n as i64);
        Ok(n)
    }

    async fn deliver_leased(&self, sub: Option<&WebhookSubscriptionRow>, d: WebhookDeliveryRow) {
        let Some(sub) = sub else {
            let _ = aurix_db::queries::mark_webhook_attempt_failed(
                &self.pool,
                d.id,
                None,
                "subscription deleted",
                None,
            )
            .await;
            return;
        };
        let is_manual = d.event_type == TEST_EVENT || d.event_type == RESYNC_EVENT;
        if !sub.enabled && !is_manual {
            let _ = aurix_db::queries::mark_webhook_attempt_failed(
                &self.pool,
                d.id,
                None,
                "subscription disabled",
                None,
            )
            .await;
            return;
        }
        match self.attempt(sub, &d).await {
            Ok(status) => {
                aurix_metrics::WEBHOOK_DELIVERIES
                    .with_label_values(&["delivered"])
                    .inc();
                debug!(webhook = %sub.id, delivery = %d.id, attempt = d.attempts, "webhook delivered ({status})");
                if let Err(e) =
                    aurix_db::queries::mark_webhook_delivered(&self.pool, d.id, status as i16).await
                {
                    warn!("webhook bookkeeping failed: {e}");
                }
                let _ = aurix_db::queries::record_webhook_result(
                    &self.pool,
                    sub.id,
                    true,
                    Some(status as i16),
                    None,
                )
                .await;
            }
            Err((status, error)) => {
                let next = self.next_attempt_at(d.attempts);
                let label = if next.is_some() { "retry" } else { "failed" };
                aurix_metrics::WEBHOOK_DELIVERIES
                    .with_label_values(&[label])
                    .inc();
                warn!(
                    webhook = %sub.id, delivery = %d.id, attempt = d.attempts,
                    "webhook delivery failed: {error}{}",
                    match next { Some(t) => format!("; retry at {t}"), None => "; giving up".into() }
                );
                if let Err(e) = aurix_db::queries::mark_webhook_attempt_failed(
                    &self.pool,
                    d.id,
                    status.map(|s| s as i16),
                    &error,
                    next,
                )
                .await
                {
                    warn!("webhook bookkeeping failed: {e}");
                }
                let _ = aurix_db::queries::record_webhook_result(
                    &self.pool,
                    sub.id,
                    false,
                    status.map(|s| s as i16),
                    Some(&error),
                )
                .await;
            }
        }
    }

    // ── Background tasks ──

    /// Spawns the bus listener, the delivery worker and the retention sweep; all stop on
    /// `cancel`.
    pub fn start(self: &Arc<Self>, cancel: CancellationToken) -> Vec<tokio::task::JoinHandle<()>> {
        if !self.cfg.enabled {
            info!("Webhooks disabled (webhooks.enabled = false)");
            return Vec::new();
        }
        let (tx, mut rx) = mpsc::channel::<ServerEvent>(ENQUEUE_BUFFER);
        let mut handles = Vec::new();

        // Listener: only locally published events (remote replicas were enqueued at their origin).
        let mut bus = self.events.subscribe_outbound();
        let listener_cancel = cancel.clone();
        handles.push(tokio::spawn(async move {
            loop {
                let ev = tokio::select! {
                    r = bus.recv() => r,
                    _ = listener_cancel.cancelled() => return,
                };
                match ev {
                    Ok(ev) => {
                        if ev.public_type().is_none() || ev.is_realtime_noise() {
                            continue;
                        }
                        if tx.try_send(ev).is_err() {
                            aurix_metrics::WEBHOOK_DELIVERIES
                                .with_label_values(&["dropped"])
                                .inc();
                            warn!("webhook enqueue buffer full; dropping event");
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                        aurix_metrics::WEBHOOK_DELIVERIES
                            .with_label_values(&["dropped"])
                            .inc_by(n);
                        warn!("webhook listener lagged {n} events");
                    }
                    Err(_) => return,
                }
            }
        }));

        // Cache invalidation from every node (remote replicas arrive through the local bus).
        let svc = self.clone();
        let mut local_bus = self.events.subscribe();
        let inval_cancel = cancel.clone();
        handles.push(tokio::spawn(async move {
            loop {
                let ev = tokio::select! {
                    r = local_bus.recv() => r,
                    _ = inval_cancel.cancelled() => return,
                };
                match ev {
                    Ok(ServerEvent::WebhooksChanged { app_id }) => {
                        svc.cache.remove(&app_id.0);
                    }
                    Ok(_) => {}
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => svc.cache.clear(),
                    Err(_) => return,
                }
            }
        }));

        // Writer: subscription lookup + queue insert, off the bus thread.
        let svc = self.clone();
        let writer_cancel = cancel.clone();
        handles.push(tokio::spawn(async move {
            loop {
                let ev = tokio::select! {
                    r = rx.recv() => match r { Some(ev) => ev, None => return },
                    _ = writer_cancel.cancelled() => return,
                };
                if let Err(e) = svc.enqueue_event(&ev).await {
                    warn!("webhook enqueue failed: {e}");
                }
            }
        }));

        // Worker: drain due deliveries; sweep finished rows hourly.
        let svc = self.clone();
        handles.push(tokio::spawn(async move {
            let mut poll = tokio::time::interval(Duration::from_millis(svc.cfg.poll_interval_ms));
            poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            let mut last_sweep = Instant::now();
            loop {
                tokio::select! {
                    _ = poll.tick() => {}
                    _ = cancel.cancelled() => return,
                }
                // Keep draining while batches come back full so a burst clears quickly.
                loop {
                    match svc.drain().await {
                        Ok(n) if n as u32 >= svc.cfg.batch_size => continue,
                        Ok(_) => break,
                        Err(e) => {
                            warn!("webhook worker: {e}");
                            break;
                        }
                    }
                }
                if svc.cfg.retention_hours > 0 && last_sweep.elapsed() > Duration::from_secs(3600) {
                    last_sweep = Instant::now();
                    let cutoff =
                        Utc::now() - chrono::Duration::hours(svc.cfg.retention_hours as i64);
                    match aurix_db::queries::delete_finished_webhook_deliveries_before(
                        &svc.pool, cutoff,
                    )
                    .await
                    {
                        Ok(n) if n > 0 => info!("Deleted {n} finished webhook deliveries"),
                        Ok(_) => {}
                        Err(e) => warn!("webhook retention sweep failed: {e}"),
                    }
                }
            }
        }));
        handles
    }
}

fn db_err(e: impl std::fmt::Display) -> AurixError {
    AurixError::Database(format!("webhooks: {e}"))
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        let mut end = max;
        while !s.is_char_boundary(end) {
            end -= 1;
        }
        format!("{}…", &s[..end])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signature_round_trips_and_rejects_tampering() {
        let body = br#"{"id":"1","type":"participant.joined"}"#;
        let now = Utc::now();
        let header = sign("whsec_test", now.timestamp(), body);
        assert!(header.starts_with(&format!("t={},v1=", now.timestamp())));
        assert!(verify_signature(
            "whsec_test",
            &header,
            body,
            now,
            Duration::from_secs(300)
        ));
        assert!(!verify_signature(
            "whsec_other",
            &header,
            body,
            now,
            Duration::from_secs(300)
        ));
        assert!(!verify_signature(
            "whsec_test",
            &header,
            b"{}",
            now,
            Duration::from_secs(300)
        ));
        // Stale timestamp.
        assert!(!verify_signature(
            "whsec_test",
            &header,
            body,
            now + chrono::Duration::seconds(301),
            Duration::from_secs(300)
        ));
        // Malformed header.
        assert!(!verify_signature(
            "whsec_test",
            "v1=abc",
            body,
            now,
            Duration::from_secs(300)
        ));
        assert!(!verify_signature(
            "whsec_test",
            "t=1,v1=zz",
            body,
            now,
            Duration::from_secs(300)
        ));
    }

    #[test]
    fn known_vector() {
        // echo -n "1700000000.{}" | openssl dgst -sha256 -hmac secret
        assert_eq!(
            sign("secret", 1_700_000_000, b"{}"),
            "t=1700000000,v1=b8569b78799ff9e3cbff0fc2d63a33a2b57f3282abd07c37ae5e8e7d79a5f163"
        );
    }

    #[test]
    fn private_ranges_are_detected() {
        for ip in [
            "127.0.0.1",
            "10.1.2.3",
            "172.16.5.5",
            "192.168.0.1",
            "169.254.169.254",
            "100.64.0.1",
            "0.0.0.0",
            "::1",
            "fc00::1",
            "fe80::1",
            "::ffff:10.0.0.1",
        ] {
            assert!(is_private_ip(ip.parse().unwrap()), "{ip}");
        }
        for ip in ["8.8.8.8", "203.0.113.1", "2606:4700::1111"] {
            // 203.0.113.0/24 is documentation space and therefore also blocked.
            let private = is_private_ip(ip.parse().unwrap());
            assert_eq!(private, ip == "203.0.113.1", "{ip}");
        }
    }

    #[test]
    fn envelope_strips_tenant_and_names_types() {
        let ev = ServerEvent::ParticipantJoined {
            app_id: AppId::new(),
            channel_id: ChannelId::new(),
            user_id: UserId::new(),
            display_name: "Alice".into(),
            session_id: SessionId::new(),
            ssrc: 7,
            role: ChannelRole::Speaker,
            timestamp: Utc::now(),
        };
        let env = PublicEvent::from_event(&ev).unwrap();
        assert_eq!(env.event_type, "participant.joined");
        assert_eq!(Some(env.app_id), ev.app_id());
        assert!(env.data.get("app_id").is_none());
        assert_eq!(env.data["display_name"], "Alice");
        assert_eq!(env.data["ssrc"], 7);
        assert!(PublicEvent::from_event(&ServerEvent::NodeHealthChanged {
            node_id: MediaNodeId::new(),
            healthy: true,
            timestamp: Utc::now(),
        })
        .is_none());
        assert!(ServerEvent::webhook_type_allowed("*"));
        assert!(ServerEvent::webhook_type_allowed("channel.activated"));
        assert!(!ServerEvent::webhook_type_allowed("channel.energy"));
        assert!(!ServerEvent::webhook_type_allowed("bogus"));
        for t in ServerEvent::PUBLIC_TYPES {
            assert!(!t.is_empty());
        }
    }

    #[test]
    fn subscription_filter() {
        let mut row = WebhookSubscriptionRow {
            id: uuid::Uuid::now_v7(),
            app_id: uuid::Uuid::now_v7(),
            url: "https://x".into(),
            secret: "s".into(),
            events: vec!["participant.joined".into()],
            description: None,
            enabled: true,
            consecutive_failures: 0,
            last_delivery_at: None,
            last_status: None,
            last_error: None,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        };
        assert!(row.wants("participant.joined"));
        assert!(!row.wants("participant.left"));
        row.events = vec!["*".into()];
        assert!(row.wants("anything"));
    }
}
