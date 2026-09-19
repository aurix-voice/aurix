//! `/v1/webhooks` subscription management and the `GET /v1/events` server-sent-events stream
//! for game servers (API key auth, tenant-scoped).

use crate::errors::{ApiError, Json, Path, Query};
use crate::middleware::{ApiKeyContext, ClientIp};
use crate::state::AppState;
use aurix_common::error::AurixError;
use aurix_common::types::AuditAction;
use aurix_control::webhooks::{CreateWebhook, UpdateWebhook};
use aurix_control::{PublicEvent, ServerEvent};
use aurix_db::models::{WebhookDeliveryRow, WebhookSubscriptionRow};
use axum::{
    extract::{Extension, State},
    response::sse::{Event, KeepAlive, Sse},
};
use futures_util::stream::Stream;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::convert::Infallible;
use std::time::Duration;
use tokio::sync::broadcast;
use uuid::Uuid;

type JsonResult = Result<Json<serde_json::Value>, ApiError>;

/// Public view of a subscription. `secret` is present only in the create/rotate response.
#[derive(Serialize)]
pub struct WebhookView {
    pub id: Uuid,
    pub url: String,
    pub events: Vec<String>,
    pub description: Option<String>,
    pub enabled: bool,
    pub consecutive_failures: i32,
    pub last_delivery_at: Option<chrono::DateTime<chrono::Utc>>,
    pub last_status: Option<i16>,
    pub last_error: Option<String>,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub updated_at: chrono::DateTime<chrono::Utc>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub secret: Option<String>,
}

impl WebhookView {
    fn from_row(row: WebhookSubscriptionRow, reveal_secret: bool) -> Self {
        Self {
            id: row.id,
            url: row.url,
            events: row.events,
            description: row.description,
            enabled: row.enabled,
            consecutive_failures: row.consecutive_failures,
            last_delivery_at: row.last_delivery_at,
            last_status: row.last_status,
            last_error: row.last_error,
            created_at: row.created_at,
            updated_at: row.updated_at,
            secret: reveal_secret.then_some(row.secret),
        }
    }
}

#[derive(Serialize)]
pub struct DeliveryView {
    pub id: Uuid,
    pub webhook_id: Uuid,
    pub event_id: Uuid,
    #[serde(rename = "type")]
    pub event_type: String,
    pub status: String,
    pub attempts: i32,
    pub next_attempt_at: Option<chrono::DateTime<chrono::Utc>>,
    pub last_status: Option<i16>,
    pub last_error: Option<String>,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub delivered_at: Option<chrono::DateTime<chrono::Utc>>,
    pub payload: serde_json::Value,
}

impl From<WebhookDeliveryRow> for DeliveryView {
    fn from(r: WebhookDeliveryRow) -> Self {
        Self {
            id: r.id,
            webhook_id: r.subscription_id,
            event_id: r.event_id,
            event_type: r.event_type,
            next_attempt_at: (r.status == "pending").then_some(r.next_attempt_at),
            status: r.status,
            attempts: r.attempts,
            last_status: r.last_status,
            last_error: r.last_error,
            created_at: r.created_at,
            delivered_at: r.delivered_at,
            payload: r.payload,
        }
    }
}

fn to_json<T: Serialize>(v: T) -> JsonResult {
    serde_json::to_value(v)
        .map(Json)
        .map_err(|e| AurixError::Internal(format!("serialization failed: {e}")).into())
}

/// Event types a subscription or stream may ask for, with `webhook.test` / `webhook.resync`
/// listed as server-initiated.
pub async fn list_event_types(Extension(ctx): Extension<ApiKeyContext>) -> JsonResult {
    if !(ctx.has("webhooks:read") || ctx.has("events:read")) {
        ctx.require("webhooks:read")?;
    }
    let webhook: Vec<&str> = ServerEvent::PUBLIC_TYPES
        .iter()
        .copied()
        .filter(|t| ServerEvent::webhook_type_allowed(t))
        .collect();
    Ok(Json(serde_json::json!({
        "webhook": webhook,
        "stream": ServerEvent::PUBLIC_TYPES,
        "server_initiated": [aurix_control::webhooks::TEST_EVENT, aurix_control::webhooks::RESYNC_EVENT],
        "signature_header": "X-Aurix-Signature",
        "signature_scheme": "t=<unix>,v1=hex(HMAC-SHA256(secret, \"<t>.<body>\"))",
    })))
}

#[derive(Deserialize)]
pub struct CreateWebhookRequest {
    pub url: String,
    pub events: Vec<String>,
    pub description: Option<String>,
}

pub async fn create_webhook(
    State(state): State<AppState>,
    Extension(ctx): Extension<ApiKeyContext>,
    ip: Option<Extension<ClientIp>>,
    Json(req): Json<CreateWebhookRequest>,
) -> Result<(axum::http::StatusCode, Json<serde_json::Value>), ApiError> {
    ctx.require("webhooks:write")?;
    let row = state
        .control
        .webhooks
        .create(
            ctx.app_id,
            CreateWebhook {
                url: req.url,
                events: req.events,
                description: req.description,
            },
        )
        .await?;
    state.control.audit.log(
        Some(ctx.app_id),
        ctx.actor(),
        AuditAction::WebhookCreated,
        "webhook",
        &row.id.to_string(),
        serde_json::json!({ "url": row.url, "events": row.events }),
        ip.map(|Extension(c)| c.0.to_string()),
    );
    Ok((
        axum::http::StatusCode::CREATED,
        to_json(WebhookView::from_row(row, true))?,
    ))
}

pub async fn list_webhooks(
    State(state): State<AppState>,
    Extension(ctx): Extension<ApiKeyContext>,
) -> JsonResult {
    ctx.require("webhooks:read")?;
    let rows = state.control.webhooks.list(ctx.app_id).await?;
    let views: Vec<WebhookView> = rows
        .into_iter()
        .map(|r| WebhookView::from_row(r, false))
        .collect();
    Ok(Json(serde_json::json!({ "webhooks": views })))
}

pub async fn get_webhook(
    State(state): State<AppState>,
    Extension(ctx): Extension<ApiKeyContext>,
    Path(id): Path<Uuid>,
) -> JsonResult {
    ctx.require("webhooks:read")?;
    let row = state.control.webhooks.get(ctx.app_id, id).await?;
    to_json(WebhookView::from_row(row, false))
}

/// PATCH semantics: absent fields are left alone; `"description": null` clears it.
#[derive(Deserialize)]
pub struct UpdateWebhookRequest {
    pub url: Option<String>,
    pub events: Option<Vec<String>>,
    #[serde(default, deserialize_with = "deserialize_double_option")]
    pub description: Option<Option<String>>,
    pub enabled: Option<bool>,
}

fn deserialize_double_option<'de, D>(d: D) -> Result<Option<Option<String>>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Option::<String>::deserialize(d).map(Some)
}

pub async fn update_webhook(
    State(state): State<AppState>,
    Extension(ctx): Extension<ApiKeyContext>,
    ip: Option<Extension<ClientIp>>,
    Path(id): Path<Uuid>,
    Json(req): Json<UpdateWebhookRequest>,
) -> JsonResult {
    ctx.require("webhooks:write")?;
    if req.url.is_none()
        && req.events.is_none()
        && req.description.is_none()
        && req.enabled.is_none()
    {
        return Err(AurixError::Validation("nothing to update".into()).into());
    }
    let row = state
        .control
        .webhooks
        .update(
            ctx.app_id,
            id,
            UpdateWebhook {
                url: req.url,
                events: req.events,
                description: req.description,
                enabled: req.enabled,
            },
        )
        .await?;
    state.control.audit.log(
        Some(ctx.app_id),
        ctx.actor(),
        AuditAction::WebhookUpdated,
        "webhook",
        &row.id.to_string(),
        serde_json::json!({ "url": row.url, "events": row.events, "enabled": row.enabled }),
        ip.map(|Extension(c)| c.0.to_string()),
    );
    to_json(WebhookView::from_row(row, false))
}

pub async fn delete_webhook(
    State(state): State<AppState>,
    Extension(ctx): Extension<ApiKeyContext>,
    ip: Option<Extension<ClientIp>>,
    Path(id): Path<Uuid>,
) -> Result<axum::http::StatusCode, ApiError> {
    ctx.require("webhooks:write")?;
    state.control.webhooks.delete(ctx.app_id, id).await?;
    state.control.audit.log(
        Some(ctx.app_id),
        ctx.actor(),
        AuditAction::WebhookDeleted,
        "webhook",
        &id.to_string(),
        serde_json::Value::Null,
        ip.map(|Extension(c)| c.0.to_string()),
    );
    Ok(axum::http::StatusCode::NO_CONTENT)
}

pub async fn rotate_webhook_secret(
    State(state): State<AppState>,
    Extension(ctx): Extension<ApiKeyContext>,
    ip: Option<Extension<ClientIp>>,
    Path(id): Path<Uuid>,
) -> JsonResult {
    ctx.require("webhooks:write")?;
    let row = state.control.webhooks.rotate_secret(ctx.app_id, id).await?;
    state.control.audit.log(
        Some(ctx.app_id),
        ctx.actor(),
        AuditAction::WebhookSecretRotated,
        "webhook",
        &id.to_string(),
        serde_json::Value::Null,
        ip.map(|Extension(c)| c.0.to_string()),
    );
    to_json(WebhookView::from_row(row, true))
}

pub async fn test_webhook(
    State(state): State<AppState>,
    Extension(ctx): Extension<ApiKeyContext>,
    Path(id): Path<Uuid>,
) -> Result<(axum::http::StatusCode, Json<serde_json::Value>), ApiError> {
    ctx.require("webhooks:write")?;
    let d = state.control.webhooks.send_test(ctx.app_id, id).await?;
    Ok((
        axum::http::StatusCode::ACCEPTED,
        Json(serde_json::json!({ "delivery": DeliveryView::from(d) })),
    ))
}

/// Queues a `webhook.resync` delivery carrying every live channel + participant of the app.
pub async fn resync_webhook(
    State(state): State<AppState>,
    Extension(ctx): Extension<ApiKeyContext>,
    Path(id): Path<Uuid>,
) -> Result<(axum::http::StatusCode, Json<serde_json::Value>), ApiError> {
    ctx.require("webhooks:write")?;
    let d = state.control.webhooks.resync(ctx.app_id, id).await?;
    Ok((
        axum::http::StatusCode::ACCEPTED,
        Json(serde_json::json!({ "delivery": DeliveryView::from(d) })),
    ))
}

#[derive(Deserialize)]
pub struct DeliveriesQuery {
    pub status: Option<String>,
    pub limit: Option<i64>,
    pub offset: Option<i64>,
}

pub async fn list_deliveries(
    State(state): State<AppState>,
    Extension(ctx): Extension<ApiKeyContext>,
    Path(id): Path<Uuid>,
    Query(q): Query<DeliveriesQuery>,
) -> JsonResult {
    ctx.require("webhooks:read")?;
    let rows = state
        .control
        .webhooks
        .list_deliveries(
            ctx.app_id,
            id,
            q.status.as_deref(),
            q.limit.unwrap_or(50).clamp(1, 200),
            q.offset.unwrap_or(0).max(0),
        )
        .await?;
    let views: Vec<DeliveryView> = rows.into_iter().map(DeliveryView::from).collect();
    Ok(Json(serde_json::json!({ "deliveries": views })))
}

pub async fn get_delivery(
    State(state): State<AppState>,
    Extension(ctx): Extension<ApiKeyContext>,
    Path((id, delivery_id)): Path<(Uuid, Uuid)>,
) -> JsonResult {
    ctx.require("webhooks:read")?;
    let d = state
        .control
        .webhooks
        .get_delivery(ctx.app_id, id, delivery_id)
        .await?;
    to_json(DeliveryView::from(d))
}

/// Re-sends a delivered or failed delivery (same event id, attempts reset).
pub async fn retry_delivery(
    State(state): State<AppState>,
    Extension(ctx): Extension<ApiKeyContext>,
    Path((id, delivery_id)): Path<(Uuid, Uuid)>,
) -> Result<(axum::http::StatusCode, Json<serde_json::Value>), ApiError> {
    ctx.require("webhooks:write")?;
    let d = state
        .control
        .webhooks
        .redeliver(ctx.app_id, id, delivery_id)
        .await?;
    Ok((
        axum::http::StatusCode::ACCEPTED,
        Json(serde_json::json!({ "delivery": DeliveryView::from(d) })),
    ))
}

// ── Server-sent events ──

#[derive(Deserialize)]
pub struct EventStreamQuery {
    /// Comma-separated public event types. Empty = everything except the high-frequency
    /// `participant.typing` / `participant.speaking` / `channel.energy` (ask for them explicitly).
    pub types: Option<String>,
}

fn parse_type_filter(raw: Option<&str>) -> Result<Option<HashSet<&str>>, ApiError> {
    let Some(raw) = raw.map(str::trim).filter(|s| !s.is_empty()) else {
        return Ok(None);
    };
    let mut set = HashSet::new();
    for t in raw.split(',').map(str::trim).filter(|t| !t.is_empty()) {
        if t == "*" {
            return Ok(None);
        }
        if !ServerEvent::PUBLIC_TYPES.contains(&t) {
            return Err(AurixError::Validation(format!("unknown event type '{t}'")).into());
        }
        set.insert(t);
    }
    if set.is_empty() {
        return Ok(None);
    }
    Ok(Some(set))
}

struct StreamGauge;

impl StreamGauge {
    fn open() -> Self {
        aurix_metrics::SSE_CLIENTS.inc();
        Self
    }
}

impl Drop for StreamGauge {
    fn drop(&mut self) {
        aurix_metrics::SSE_CLIENTS.dec();
    }
}

/// `GET /v1/events` — long-lived `text/event-stream` of the caller's tenant events. Frames:
/// `event: <type>`, `id: <event id>`, `data: <PublicEvent JSON>`; a `lagged` frame with
/// `{"dropped": n}` signals that the consumer fell behind (fetch `/v1/events/snapshot`).
pub async fn event_stream(
    State(state): State<AppState>,
    Extension(ctx): Extension<ApiKeyContext>,
    Query(q): Query<EventStreamQuery>,
) -> Result<Sse<impl Stream<Item = Result<Event, Infallible>>>, ApiError> {
    ctx.require("events:read")?;
    let filter: Option<HashSet<String>> =
        parse_type_filter(q.types.as_deref())?.map(|s| s.into_iter().map(str::to_string).collect());
    let app_id = ctx.app_id;
    let rx = state.control.events.subscribe();
    let keepalive = state.control.config.webhooks.sse_keepalive_secs.max(1);
    let gauge = StreamGauge::open();

    let hello = Event::default()
        .event("stream.open")
        .json_data(serde_json::json!({ "app_id": app_id, "filter": filter }))
        .map_err(|e| AurixError::Internal(e.to_string()))?;

    let stream = futures_util::stream::unfold(
        (rx, filter, Some(hello), gauge),
        move |(mut rx, filter, hello, gauge)| async move {
            if let Some(h) = hello {
                return Some((Ok(h), (rx, filter, None, gauge)));
            }
            loop {
                match rx.recv().await {
                    Ok(ev) => {
                        if ev.app_id() != Some(app_id) {
                            continue;
                        }
                        let Some(ty) = ev.public_type() else { continue };
                        match &filter {
                            Some(set) if !set.contains(ty) => continue,
                            None if ev.is_realtime_noise() => continue,
                            _ => {}
                        }
                        let Some(env) = PublicEvent::from_event(&ev) else {
                            continue;
                        };
                        let frame = match Event::default()
                            .event(ty)
                            .id(env.id.to_string())
                            .json_data(&env)
                        {
                            Ok(f) => f,
                            Err(_) => continue,
                        };
                        return Some((Ok(frame), (rx, filter, None, gauge)));
                    }
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        let frame = Event::default()
                            .event("lagged")
                            .data(format!("{{\"dropped\":{n}}}"));
                        return Some((Ok(frame), (rx, filter, None, gauge)));
                    }
                    Err(broadcast::error::RecvError::Closed) => return None,
                }
            }
        },
    );

    Ok(Sse::new(stream).keep_alive(
        KeepAlive::new()
            .interval(Duration::from_secs(keepalive))
            .text("keepalive"),
    ))
}

/// Current live channels + participants of the tenant; the bootstrap/resync companion of the
/// event stream.
pub async fn event_snapshot(
    State(state): State<AppState>,
    Extension(ctx): Extension<ApiKeyContext>,
) -> JsonResult {
    ctx.require("events:read")?;
    Ok(Json(state.control.webhooks.snapshot(ctx.app_id).await?))
}
