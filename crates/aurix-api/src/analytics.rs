//! Usage analytics: application and channel time series, range totals, exports and the
//! per-application quota state (tenant routes under `/v1/analytics*`, fleet routes under
//! `/admin/analytics*`). Data comes from the `usage_*` buckets maintained by
//! [`aurix_control::UsageService`].

use crate::errors::{ApiError, Json, Path, Query};
use crate::middleware::ApiKeyContext;
use crate::state::AppState;
use aurix_common::error::AurixError;
use aurix_common::types::{AdminContext, AdminPermission, AppId};
use aurix_common::usage::{APP_BUCKET_SECS, CHANNEL_BUCKET_SECS};
use aurix_db::usage::{self as db, UsageAppBucketRow, UsageChannelBucketRow, UsageTotalsRow};
use axum::extract::{Extension, State};
use axum::http::{header, HeaderMap, HeaderValue};
use axum::response::{IntoResponse, Response};
use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

type JsonResult = Result<Json<serde_json::Value>, ApiError>;

/// Longest range a single query may cover.
const MAX_RANGE_DAYS: i64 = 400;
/// Points a series response may contain; pick a coarser `step` for longer ranges.
const MAX_POINTS: i64 = 10_000;
/// Points the automatic `step` aims to stay under.
const AUTO_POINTS: i64 = 5_000;
/// Rows an export may contain; narrow the range when the response reports truncation.
const MAX_EXPORT_ROWS: i64 = 200_000;
const DAY_SECS: i64 = 86_400;

#[derive(Deserialize)]
pub struct RangeQuery {
    pub from: Option<String>,
    pub to: Option<String>,
    /// Series resolution in seconds, a multiple of 300 (default: the finest of 300 / 3600 /
    /// 86400 that keeps the series under [`AUTO_POINTS`]).
    pub step: Option<i64>,
    pub limit: Option<i64>,
    /// `json` (default) or `csv`.
    pub format: Option<String>,
    /// Export scope: `app` (5-minute application buckets, default) or `channels` (hourly
    /// per-channel buckets).
    pub scope: Option<String>,
}

struct Range {
    from: DateTime<Utc>,
    to: DateTime<Utc>,
}

impl Range {
    /// Widen `from` down to a `width`-second boundary so the bucket a request starts inside
    /// of is included (buckets are keyed by their start; `to` already admits the bucket it
    /// falls in).
    fn aligned(&self, width: i64) -> Self {
        let secs = self.from.timestamp().div_euclid(width) * width;
        let from = DateTime::from_timestamp(secs, 0).unwrap_or(self.from);
        Self { from, to: self.to }
    }
}

fn parse_range(q: &RangeQuery, default_days: i64) -> Result<Range, ApiError> {
    let parse = |s: &Option<String>| -> Result<Option<DateTime<Utc>>, ApiError> {
        match s {
            None => Ok(None),
            Some(s) => DateTime::parse_from_rfc3339(s)
                .map(|d| Some(d.with_timezone(&Utc)))
                .map_err(|_| {
                    AurixError::Validation("from/to must be RFC 3339 timestamps".into()).into()
                }),
        }
    };
    let to = parse(&q.to)?.unwrap_or_else(Utc::now);
    let from = parse(&q.from)?.unwrap_or(to - Duration::days(default_days));
    if to <= from || to - from > Duration::days(MAX_RANGE_DAYS) {
        return Err(AurixError::Validation(format!(
            "range must be positive and at most {MAX_RANGE_DAYS} days"
        ))
        .into());
    }
    Ok(Range { from, to })
}

/// Chosen or validated series step: a multiple of the 5-minute bucket that keeps the
/// response under [`MAX_POINTS`]; automatic selection prefers the finest such resolution.
fn resolve_step(q: &RangeQuery, range: &Range) -> Result<i64, ApiError> {
    let span = (range.to - range.from).num_seconds();
    match q.step {
        Some(step) => {
            if step < APP_BUCKET_SECS || step % APP_BUCKET_SECS != 0 {
                return Err(AurixError::Validation(format!(
                    "step must be a multiple of {APP_BUCKET_SECS} seconds"
                ))
                .into());
            }
            if span / step > MAX_POINTS {
                return Err(AurixError::Validation(format!(
                    "range/step yields more than {MAX_POINTS} points; use a coarser step"
                ))
                .into());
            }
            Ok(step)
        }
        None => Ok([APP_BUCKET_SECS, CHANNEL_BUCKET_SECS, DAY_SECS]
            .into_iter()
            .find(|s| span / s <= AUTO_POINTS)
            .unwrap_or(DAY_SECS)),
    }
}

fn limit(q: &RangeQuery, default: i64, max: i64) -> i64 {
    q.limit.unwrap_or(default).clamp(1, max)
}

fn wants_csv(q: &RangeQuery) -> Result<bool, ApiError> {
    match q.format.as_deref() {
        None | Some("json") => Ok(false),
        Some("csv") => Ok(true),
        Some(_) => Err(AurixError::Validation("format must be json or csv".into()).into()),
    }
}

#[derive(Serialize)]
struct RangeView {
    from: DateTime<Utc>,
    to: DateTime<Utc>,
    step_secs: i64,
    /// Buckets before this instant are final; the current bucket is still accruing.
    finalized_through: Option<DateTime<Utc>>,
}

fn totals_json(t: Option<UsageTotalsRow>) -> serde_json::Value {
    match t {
        Some(t) => serde_json::json!({
            "peak_sessions": t.peak_sessions,
            "session_minutes": t.session_minutes,
            "sessions_started": t.sessions_started,
            "peak_participants": t.peak_participants,
            "participant_minutes": t.participant_minutes,
            "recording_seconds": t.recording_seconds,
            "media_bytes_in": t.media_bytes_in,
            "media_bytes_out": t.media_bytes_out,
            "chat_messages": t.chat_messages,
            "tts_requests": t.tts_requests,
            "tts_characters": t.tts_characters,
            "stt_audio_ms": t.stt_audio_ms,
        }),
        None => serde_json::json!({
            "peak_sessions": 0, "session_minutes": 0.0, "sessions_started": 0,
            "peak_participants": 0, "participant_minutes": 0.0, "recording_seconds": 0.0,
            "media_bytes_in": 0, "media_bytes_out": 0, "chat_messages": 0,
            "tts_requests": 0, "tts_characters": 0, "stt_audio_ms": 0,
        }),
    }
}

async fn app_usage_body(
    state: &AppState,
    app_id: AppId,
    q: &RangeQuery,
) -> Result<serde_json::Value, ApiError> {
    let range = parse_range(q, 7)?;
    let step = resolve_step(q, &range)?;
    let range = range.aligned(step);
    let pool = &state.control.pool;
    let series = if step == APP_BUCKET_SECS {
        db::app_series(pool, app_id.0, range.from, range.to, MAX_POINTS).await?
    } else {
        db::app_series_rollup(pool, app_id.0, range.from, range.to, step, MAX_POINTS).await?
    };
    let totals = db::app_totals(pool, app_id.0, range.from, range.to).await?;
    let finalized_through = state.control.usage.app_watermark().await?;
    let active_sessions = state.control.sessions.count_active_sessions(app_id).await?;
    let active_channels = state.control.channels.count_active_channels(app_id).await?;
    let users = aurix_db::queries::count_users(pool, app_id.0).await?;
    Ok(serde_json::json!({
        "current": { "active_sessions": active_sessions, "active_channels": active_channels, "users": users },
        "range": RangeView { from: range.from, to: range.to, step_secs: step, finalized_through },
        "totals": totals_json(totals),
        "series": series,
    }))
}

/// `GET /v1/analytics` — current counters, range totals and the application time series.
pub async fn get_analytics(
    State(state): State<AppState>,
    Extension(ctx): Extension<ApiKeyContext>,
    Query(q): Query<RangeQuery>,
) -> JsonResult {
    ctx.require("analytics:read")?;
    Ok(Json(app_usage_body(&state, ctx.app_id, &q).await?))
}

/// `GET /v1/analytics/channels` — per-channel totals over the range, busiest first.
pub async fn list_channel_usage(
    State(state): State<AppState>,
    Extension(ctx): Extension<ApiKeyContext>,
    Query(q): Query<RangeQuery>,
) -> JsonResult {
    ctx.require("analytics:read")?;
    let range = parse_range(&q, 7)?.aligned(CHANNEL_BUCKET_SECS);
    let limit = limit(&q, 100, 1000);
    let rows = db::channel_totals(
        &state.control.pool,
        ctx.app_id.0,
        range.from,
        range.to,
        limit,
    )
    .await?;
    let channels: Vec<serde_json::Value> = rows.iter().map(channel_totals_json).collect();
    Ok(Json(serde_json::json!({
        "range": { "from": range.from, "to": range.to },
        "channels": channels,
    })))
}

fn channel_totals_json(r: &UsageChannelBucketRow) -> serde_json::Value {
    serde_json::json!({
        "channel_id": r.channel_id,
        "peak_participants": r.peak_participants,
        "participant_minutes": r.participant_minutes,
        "joins": r.joins,
        "unique_users": r.unique_users,
        "chat_messages": r.chat_messages,
        "tts_requests": r.tts_requests,
        "tts_characters": r.tts_characters,
        "stt_audio_ms": r.stt_audio_ms,
    })
}

/// `GET /v1/analytics/channels/{channel_id}` — hourly series of one channel. A channel that
/// neither exists in the tenant nor has usage there is `404`.
pub async fn get_channel_usage(
    State(state): State<AppState>,
    Extension(ctx): Extension<ApiKeyContext>,
    Path(channel_id): Path<Uuid>,
    Query(q): Query<RangeQuery>,
) -> JsonResult {
    ctx.require("analytics:read")?;
    let range = parse_range(&q, 7)?.aligned(CHANNEL_BUCKET_SECS);
    let pool = &state.control.pool;
    let series = db::channel_series(
        pool,
        ctx.app_id.0,
        Some(channel_id),
        range.from,
        range.to,
        MAX_POINTS,
    )
    .await?;
    if series.is_empty()
        && aurix_db::queries::get_channel(pool, ctx.app_id.0, channel_id)
            .await?
            .is_none()
    {
        return Err(AurixError::ChannelNotFound(channel_id.to_string()).into());
    }
    let finalized_through = state.control.usage.channel_watermark().await?;
    let totals = serde_json::json!({
        "peak_participants": series.iter().map(|r| r.peak_participants).max().unwrap_or(0),
        "participant_minutes": series.iter().map(|r| r.participant_minutes).sum::<f64>(),
        "joins": series.iter().map(|r| r.joins).sum::<i64>(),
        "chat_messages": series.iter().map(|r| r.chat_messages).sum::<i64>(),
        "tts_requests": series.iter().map(|r| r.tts_requests).sum::<i64>(),
        "tts_characters": series.iter().map(|r| r.tts_characters).sum::<i64>(),
        "stt_audio_ms": series.iter().map(|r| r.stt_audio_ms).sum::<i64>(),
    });
    Ok(Json(serde_json::json!({
        "channel_id": channel_id,
        "range": RangeView { from: range.from, to: range.to, step_secs: CHANNEL_BUCKET_SECS, finalized_through },
        "totals": totals,
        "series": series,
    })))
}

/// `GET /v1/analytics/quota` — limits and how much of them the application has used.
pub async fn get_quota(
    State(state): State<AppState>,
    Extension(ctx): Extension<ApiKeyContext>,
) -> JsonResult {
    ctx.require("analytics:read")?;
    let app = aurix_db::queries::get_app(&state.control.pool, ctx.app_id.0)
        .await?
        .ok_or_else(|| AurixError::NotFound("Application not found".into()))?;
    Ok(Json(quota_body(&state, &app).await?))
}

async fn quota_body(
    state: &AppState,
    app: &aurix_db::models::AppRow,
) -> Result<serde_json::Value, ApiError> {
    let q = state
        .control
        .usage
        .quota_state(
            AppId(app.id),
            app.max_concurrent_sessions,
            app.monthly_participant_minutes,
        )
        .await?;
    Ok(serde_json::json!({
        "max_concurrent_sessions": q.max_concurrent_sessions,
        "active_sessions": q.active_sessions,
        "monthly_participant_minutes": q.monthly_participant_minutes,
        "participant_minutes_this_month": q.participant_minutes_this_month,
        "month_start": q.month_start,
    }))
}

const APP_CSV_HEADER: &str = "app_id,bucket,peak_sessions,session_minutes,sessions_started,unique_users,peak_participants,participant_minutes,active_channels,recording_seconds,media_bytes_in,media_bytes_out,chat_messages,tts_requests,tts_characters,stt_audio_ms";
const CHANNEL_CSV_HEADER: &str = "app_id,channel_id,bucket,peak_participants,participant_minutes,joins,unique_users,chat_messages,tts_requests,tts_characters,stt_audio_ms";

fn app_csv(rows: &[UsageAppBucketRow]) -> String {
    let mut out = String::with_capacity(rows.len() * 160 + APP_CSV_HEADER.len() + 1);
    out.push_str(APP_CSV_HEADER);
    out.push('\n');
    for r in rows {
        out.push_str(&format!(
            "{},{},{},{:.4},{},{},{},{:.4},{},{:.3},{},{},{},{},{},{}\n",
            r.app_id,
            r.bucket.to_rfc3339(),
            r.peak_sessions,
            r.session_minutes,
            r.sessions_started,
            r.unique_users,
            r.peak_participants,
            r.participant_minutes,
            r.active_channels,
            r.recording_seconds,
            r.media_bytes_in,
            r.media_bytes_out,
            r.chat_messages,
            r.tts_requests,
            r.tts_characters,
            r.stt_audio_ms
        ));
    }
    out
}

fn channel_csv(rows: &[UsageChannelBucketRow]) -> String {
    let mut out = String::with_capacity(rows.len() * 140 + CHANNEL_CSV_HEADER.len() + 1);
    out.push_str(CHANNEL_CSV_HEADER);
    out.push('\n');
    for r in rows {
        out.push_str(&format!(
            "{},{},{},{},{:.4},{},{},{},{},{},{}\n",
            r.app_id,
            r.channel_id,
            r.bucket.to_rfc3339(),
            r.peak_participants,
            r.participant_minutes,
            r.joins,
            r.unique_users,
            r.chat_messages,
            r.tts_requests,
            r.tts_characters,
            r.stt_audio_ms
        ));
    }
    out
}

enum ExportRows {
    App(Vec<UsageAppBucketRow>),
    Channels(Vec<UsageChannelBucketRow>),
}

/// Renders an export as JSON (`{range, scope, truncated, rows}`) or CSV (`text/csv`, with
/// `X-Aurix-Truncated: true` when the row cap cut the range short).
fn export_response(
    range: &Range,
    scope: &str,
    mut rows: ExportRows,
    csv: bool,
    filename: &str,
) -> Result<Response, ApiError> {
    // Queries fetch one row past the cap so a range that fits exactly is not flagged.
    let cap = MAX_EXPORT_ROWS as usize;
    let (len, truncated) = match &mut rows {
        ExportRows::App(r) => {
            let t = r.len() > cap;
            r.truncate(cap);
            (r.len(), t)
        }
        ExportRows::Channels(r) => {
            let t = r.len() > cap;
            r.truncate(cap);
            (r.len(), t)
        }
    };
    if !csv {
        let rows = match rows {
            ExportRows::App(r) => serde_json::to_value(r),
            ExportRows::Channels(r) => serde_json::to_value(r),
        }
        .map_err(|e| AurixError::Internal(format!("export encode: {e}")))?;
        return Ok(Json(serde_json::json!({
            "range": { "from": range.from, "to": range.to },
            "scope": scope,
            "count": len,
            "truncated": truncated,
            "rows": rows,
        }))
        .into_response());
    }
    let body = match rows {
        ExportRows::App(r) => app_csv(&r),
        ExportRows::Channels(r) => channel_csv(&r),
    };
    let mut headers = HeaderMap::new();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/csv; charset=utf-8"),
    );
    headers.insert(
        header::CONTENT_DISPOSITION,
        format!("attachment; filename=\"{filename}\"")
            .parse()
            .map_err(|_| AurixError::Internal("bad disposition".into()))?,
    );
    if truncated {
        headers.insert("x-aurix-truncated", HeaderValue::from_static("true"));
    }
    Ok((headers, body).into_response())
}

/// `GET /v1/analytics/export` — raw buckets of the tenant for billing/BI, JSON or CSV.
pub async fn export_usage(
    State(state): State<AppState>,
    Extension(ctx): Extension<ApiKeyContext>,
    Query(q): Query<RangeQuery>,
) -> Result<Response, ApiError> {
    ctx.require("analytics:read")?;
    let range = parse_range(&q, 30)?;
    let csv = wants_csv(&q)?;
    let pool = &state.control.pool;
    let app = ctx.app_id.0;
    let stamp = range.from.format("%Y%m%d");
    match q.scope.as_deref().unwrap_or("app") {
        "app" => {
            let range = range.aligned(APP_BUCKET_SECS);
            let rows = db::app_series(pool, app, range.from, range.to, MAX_EXPORT_ROWS + 1).await?;
            export_response(
                &range,
                "app",
                ExportRows::App(rows),
                csv,
                &format!("usage-{app}-{stamp}.csv"),
            )
        }
        "channels" => {
            let range = range.aligned(CHANNEL_BUCKET_SECS);
            let rows =
                db::channel_series(pool, app, None, range.from, range.to, MAX_EXPORT_ROWS + 1)
                    .await?;
            export_response(
                &range,
                "channels",
                ExportRows::Channels(rows),
                csv,
                &format!("usage-channels-{app}-{stamp}.csv"),
            )
        }
        _ => Err(AurixError::Validation("scope must be app or channels".into()).into()),
    }
}

// ── Administrator (fleet) views ──

/// `GET /admin/analytics/usage` — range totals per application across the fleet.
pub async fn admin_usage(
    State(state): State<AppState>,
    Extension(admin): Extension<AdminContext>,
    Query(q): Query<RangeQuery>,
) -> JsonResult {
    admin.require(AdminPermission::AnalyticsRead)?;
    let range = parse_range(&q, 30)?.aligned(APP_BUCKET_SECS);
    let rows = db::all_app_totals(&state.control.pool, range.from, range.to).await?;
    let finalized_through = state.control.usage.app_watermark().await?;
    let apps: Vec<serde_json::Value> = rows
        .into_iter()
        .map(|t| {
            let app_id = t.app_id;
            let mut v = totals_json(Some(t));
            v["app_id"] = serde_json::json!(app_id);
            v
        })
        .collect();
    Ok(Json(serde_json::json!({
        "range": { "from": range.from, "to": range.to, "finalized_through": finalized_through },
        "apps": apps,
    })))
}

/// `GET /admin/analytics/apps/{app_id}` — one application's series, totals and quota state.
/// Deactivated applications stay readable so their last invoice can still be produced.
pub async fn admin_app_usage(
    State(state): State<AppState>,
    Extension(admin): Extension<AdminContext>,
    Path(app_id): Path<Uuid>,
    Query(q): Query<RangeQuery>,
) -> JsonResult {
    admin.require(AdminPermission::AnalyticsRead)?;
    let app = aurix_db::queries::get_app_any(&state.control.pool, app_id)
        .await?
        .ok_or_else(|| AurixError::NotFound("Application not found".into()))?;
    let app_id = AppId(app_id);
    let mut body = app_usage_body(&state, app_id, &q).await?;
    body["app_id"] = serde_json::json!(app_id);
    body["active"] = serde_json::json!(app.active);
    body["quota"] = quota_body(&state, &app).await?;
    Ok(Json(body))
}

/// `GET /admin/analytics/export` — 5-minute buckets of every application, JSON or CSV.
pub async fn admin_export_usage(
    State(state): State<AppState>,
    Extension(admin): Extension<AdminContext>,
    Query(q): Query<RangeQuery>,
) -> Result<Response, ApiError> {
    admin.require(AdminPermission::AnalyticsRead)?;
    let range = parse_range(&q, 30)?.aligned(APP_BUCKET_SECS);
    let csv = wants_csv(&q)?;
    let rows = db::all_app_series(
        &state.control.pool,
        range.from,
        range.to,
        MAX_EXPORT_ROWS + 1,
    )
    .await?;
    export_response(
        &range,
        "fleet",
        ExportRows::App(rows),
        csv,
        &format!("usage-fleet-{}.csv", range.from.format("%Y%m%d")),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn q(from: &str, to: &str, step: Option<i64>) -> RangeQuery {
        RangeQuery {
            from: Some(from.into()),
            to: Some(to.into()),
            step,
            limit: None,
            format: None,
            scope: None,
        }
    }

    #[test]
    fn auto_step_coarsens_with_the_range() {
        let day = q("2024-01-01T00:00:00Z", "2024-01-02T00:00:00Z", None);
        let r = parse_range(&day, 7).unwrap();
        assert_eq!(resolve_step(&day, &r).unwrap(), APP_BUCKET_SECS);
        let month = q("2024-01-01T00:00:00Z", "2024-03-01T00:00:00Z", None);
        let r = parse_range(&month, 7).unwrap();
        assert_eq!(resolve_step(&month, &r).unwrap(), CHANNEL_BUCKET_SECS);
        let year = q("2023-01-01T00:00:00Z", "2024-01-01T00:00:00Z", None);
        let r = parse_range(&year, 7).unwrap();
        assert_eq!(resolve_step(&year, &r).unwrap(), DAY_SECS);
    }

    #[test]
    fn explicit_step_is_validated() {
        let day = q("2024-01-01T00:00:00Z", "2024-01-02T00:00:00Z", Some(299));
        let r = parse_range(&day, 7).unwrap();
        assert!(resolve_step(&day, &r).is_err());
        let year = q("2023-01-01T00:00:00Z", "2024-01-01T00:00:00Z", Some(300));
        let r = parse_range(&year, 7).unwrap();
        assert!(resolve_step(&year, &r).is_err(), "too many points");
        let fine = q("2023-01-01T00:00:00Z", "2024-01-01T00:00:00Z", Some(3600));
        assert_eq!(resolve_step(&fine, &r).unwrap(), 3600);
    }

    #[test]
    fn range_rejects_inverted_and_oversized() {
        assert!(parse_range(&q("2024-01-02T00:00:00Z", "2024-01-01T00:00:00Z", None), 7).is_err());
        assert!(parse_range(&q("2022-01-01T00:00:00Z", "2024-01-01T00:00:00Z", None), 7).is_err());
        assert!(parse_range(&q("nope", "2024-01-01T00:00:00Z", None), 7).is_err());
        let default = RangeQuery {
            from: None,
            to: None,
            step: None,
            limit: None,
            format: None,
            scope: None,
        };
        let r = parse_range(&default, 7).unwrap();
        assert_eq!((r.to - r.from).num_days(), 7);
    }

    #[test]
    fn range_start_is_widened_to_the_bucket_boundary() {
        let r = parse_range(&q("2024-01-01T08:03:47Z", "2024-01-01T09:00:00Z", None), 7).unwrap();
        let app = r.aligned(APP_BUCKET_SECS);
        assert_eq!(app.from.to_rfc3339(), "2024-01-01T08:00:00+00:00");
        let r = parse_range(&q("2024-01-01T08:33:47Z", "2024-01-01T09:00:00Z", None), 7).unwrap();
        assert_eq!(
            r.aligned(APP_BUCKET_SECS).from.to_rfc3339(),
            "2024-01-01T08:30:00+00:00"
        );
        assert_eq!(
            r.aligned(CHANNEL_BUCKET_SECS).from.to_rfc3339(),
            "2024-01-01T08:00:00+00:00"
        );
        assert_eq!(
            r.aligned(DAY_SECS).from.to_rfc3339(),
            "2024-01-01T00:00:00+00:00"
        );
        assert_eq!(r.aligned(DAY_SECS).to, r.to);
    }

    #[test]
    fn csv_rows_follow_the_header() {
        let now = Utc::now();
        let row = UsageAppBucketRow {
            app_id: Uuid::nil(),
            bucket: now,
            peak_sessions: 3,
            session_minutes: 12.5,
            sessions_started: 4,
            unique_users: 2,
            peak_participants: 3,
            participant_minutes: 10.0,
            active_channels: 1,
            recording_seconds: 0.0,
            media_bytes_in: 100,
            media_bytes_out: 200,
            chat_messages: 5,
            tts_requests: 0,
            tts_characters: 0,
            stt_audio_ms: 0,
            updated_at: now,
        };
        let csv = app_csv(&[row]);
        let mut lines = csv.lines();
        let header = lines.next().unwrap();
        let data = lines.next().unwrap();
        assert_eq!(header.split(',').count(), data.split(',').count());
        assert!(data.starts_with(&format!("{},{}", Uuid::nil(), now.to_rfc3339())));
        assert!(data.ends_with(",100,200,5,0,0,0"));
        let ch = UsageChannelBucketRow {
            app_id: Uuid::nil(),
            channel_id: Uuid::nil(),
            bucket: now,
            peak_participants: 2,
            participant_minutes: 1.0,
            joins: 2,
            unique_users: 2,
            chat_messages: 0,
            tts_requests: 0,
            tts_characters: 0,
            stt_audio_ms: 0,
            updated_at: now,
        };
        let csv = channel_csv(&[ch]);
        let mut lines = csv.lines();
        assert_eq!(
            lines.next().unwrap().split(',').count(),
            lines.next().unwrap().split(',').count()
        );
    }
}
