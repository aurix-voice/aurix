//! Usage accounting queries: bucket aggregation from the session / membership ledgers,
//! metered-counter upserts, time-series reads and quota totals.

use crate::DbPool;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::FromRow;
use uuid::Uuid;

pub const SCOPE_APP: &str = "app";
pub const SCOPE_CHANNEL: &str = "channel";

#[derive(Debug, Clone, PartialEq, FromRow, Serialize, Deserialize)]
pub struct UsageAppBucketRow {
    pub app_id: Uuid,
    pub bucket: DateTime<Utc>,
    pub peak_sessions: i64,
    pub session_minutes: f64,
    pub sessions_started: i64,
    pub unique_users: i64,
    pub peak_participants: i64,
    pub participant_minutes: f64,
    pub active_channels: i64,
    pub recording_seconds: f64,
    pub media_bytes_in: i64,
    pub media_bytes_out: i64,
    pub chat_messages: i64,
    pub tts_requests: i64,
    pub tts_characters: i64,
    pub stt_audio_ms: i64,
    pub quality_samples: i64,
    pub mos_sum_milli: i64,
    pub rtt_sum_ms: i64,
    pub jitter_sum_ms: i64,
    pub loss_sum_permille: i64,
    pub poor_quality_samples: i64,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, FromRow, Serialize, Deserialize)]
pub struct UsageChannelBucketRow {
    pub app_id: Uuid,
    pub channel_id: Uuid,
    pub bucket: DateTime<Utc>,
    pub peak_participants: i64,
    pub participant_minutes: f64,
    pub joins: i64,
    pub unique_users: i64,
    pub chat_messages: i64,
    pub tts_requests: i64,
    pub tts_characters: i64,
    pub stt_audio_ms: i64,
    pub updated_at: DateTime<Utc>,
}

/// Totals over a range (sums of the bucket columns, maxima of the peaks).
#[derive(Debug, Clone, PartialEq, FromRow, Serialize, Deserialize)]
pub struct UsageTotalsRow {
    pub app_id: Uuid,
    pub peak_sessions: i64,
    pub session_minutes: f64,
    pub sessions_started: i64,
    pub peak_participants: i64,
    pub participant_minutes: f64,
    pub recording_seconds: f64,
    pub media_bytes_in: i64,
    pub media_bytes_out: i64,
    pub chat_messages: i64,
    pub tts_requests: i64,
    pub tts_characters: i64,
    pub stt_audio_ms: i64,
    pub quality_samples: i64,
    pub mos_sum_milli: i64,
    pub rtt_sum_ms: i64,
    pub jitter_sum_ms: i64,
    pub loss_sum_permille: i64,
    pub poor_quality_samples: i64,
}

/// One metered increment for [`add_counters`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CounterDelta {
    pub app_id: Uuid,
    pub channel_id: Option<Uuid>,
    /// Application bucket start (5 minutes). Channel rows use the containing hour.
    pub bucket: DateTime<Utc>,
    pub metric: &'static str,
    pub value: i64,
}

pub async fn get_watermark(
    pool: &DbPool,
    scope: &str,
) -> Result<Option<DateTime<Utc>>, sqlx::Error> {
    sqlx::query_scalar::<_, DateTime<Utc>>("SELECT bucket FROM usage_watermarks WHERE scope = $1")
        .bind(scope)
        .fetch_optional(pool)
        .await
}

pub async fn set_watermark(
    pool: &DbPool,
    scope: &str,
    bucket: DateTime<Utc>,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        r#"INSERT INTO usage_watermarks (scope, bucket, updated_at) VALUES ($1, $2, NOW())
           ON CONFLICT (scope) DO UPDATE SET bucket = EXCLUDED.bucket, updated_at = NOW()"#,
    )
    .bind(scope)
    .bind(bucket)
    .execute(pool)
    .await?;
    Ok(())
}

/// Earliest session start, i.e. where a fresh deployment's backfill begins.
pub async fn earliest_session(pool: &DbPool) -> Result<Option<DateTime<Utc>>, sqlx::Error> {
    sqlx::query_scalar::<_, Option<DateTime<Utc>>>("SELECT MIN(connected_at) FROM sessions")
        .fetch_one(pool)
        .await
}

/// Finalizes application buckets of `width_secs` in `[from, to)` from the session and
/// membership ledgers. Peak concurrency is exact: sessions live at the bucket start plus the
/// running maximum of connect(+1)/disconnect(-1) events inside it (a disconnect and a connect
/// at the same instant count the disconnect first). Minutes are the overlap of every session
/// with the bucket, so sessions still open contribute their full overlap and one session
/// spanning several buckets is split between them. Overlap is capped at the transaction's
/// `NOW()`, so the bucket containing the present holds a partial, monotonically growing value.
pub async fn aggregate_app_buckets(
    pool: &DbPool,
    from: DateTime<Utc>,
    to: DateTime<Utc>,
    width_secs: i64,
) -> Result<u64, sqlx::Error> {
    let mut tx = pool.begin().await?;
    // A re-derivation must also clear buckets whose intervals vanished (a session closed late
    // with an earlier end than the open row implied); the upserts below only touch buckets
    // that still overlap something.
    sqlx::query(
        r#"UPDATE usage_app_buckets SET
               peak_sessions = 0, session_minutes = 0, sessions_started = 0, unique_users = 0,
               peak_participants = 0, participant_minutes = 0, active_channels = 0,
               recording_seconds = 0, updated_at = NOW()
           WHERE bucket >= $1 AND bucket < $2
             AND (peak_sessions <> 0 OR session_minutes <> 0 OR sessions_started <> 0 OR unique_users <> 0
                  OR peak_participants <> 0 OR participant_minutes <> 0 OR active_channels <> 0
                  OR recording_seconds <> 0)"#,
    )
    .bind(from)
    .bind(to)
    .execute(&mut *tx)
    .await?;
    let sessions = sqlx::query(
        r#"WITH b AS (
               SELECT gs AS bucket, gs + make_interval(secs => $3) AS bucket_end
               FROM generate_series($1::timestamptz, $2::timestamptz - interval '1 second', make_interval(secs => $3)) gs
           ), s AS (
               SELECT app_id, user_id, connected_at, COALESCE(disconnected_at, 'infinity'::timestamptz) AS ended
               FROM sessions
               WHERE connected_at < $2 AND (disconnected_at IS NULL OR disconnected_at > $1)
           ), ov AS (
               SELECT b.bucket, b.bucket_end, s.app_id, s.user_id, s.connected_at, s.ended
               FROM b JOIN s ON s.connected_at < b.bucket_end AND s.ended > b.bucket
           ), base AS (
               SELECT bucket, app_id,
                      COUNT(*) FILTER (WHERE connected_at < bucket) AS at_start,
                      SUM(GREATEST(EXTRACT(EPOCH FROM (LEAST(ended, bucket_end, NOW()) - GREATEST(connected_at, bucket))), 0)) / 60.0 AS minutes,
                      COUNT(*) FILTER (WHERE connected_at >= bucket) AS started,
                      COUNT(DISTINCT user_id) AS users
               FROM ov GROUP BY bucket, app_id
           ), ev AS (
               SELECT bucket, app_id, connected_at AS t, 1 AS d FROM ov WHERE connected_at >= bucket
               UNION ALL
               SELECT bucket, app_id, ended AS t, -1 AS d FROM ov WHERE ended < bucket_end
           ), run AS (
               SELECT bucket, app_id,
                      SUM(d) OVER (PARTITION BY bucket, app_id ORDER BY t, d ROWS UNBOUNDED PRECEDING) AS running
               FROM ev
           ), pk AS (
               SELECT bucket, app_id, GREATEST(MAX(running), 0) AS peak_delta FROM run GROUP BY bucket, app_id
           )
           INSERT INTO usage_app_buckets (app_id, bucket, peak_sessions, session_minutes, sessions_started, unique_users, updated_at)
           SELECT base.app_id, base.bucket, base.at_start + COALESCE(pk.peak_delta, 0), base.minutes, base.started, base.users, NOW()
           FROM base LEFT JOIN pk ON pk.bucket = base.bucket AND pk.app_id = base.app_id
           ON CONFLICT (app_id, bucket) DO UPDATE SET
               peak_sessions = EXCLUDED.peak_sessions,
               session_minutes = EXCLUDED.session_minutes,
               sessions_started = EXCLUDED.sessions_started,
               unique_users = EXCLUDED.unique_users,
               updated_at = NOW()"#,
    )
    .bind(from)
    .bind(to)
    .bind(width_secs as f64)
    .execute(&mut *tx)
    .await?
    .rows_affected();

    sqlx::query(
        r#"WITH b AS (
               SELECT gs AS bucket, gs + make_interval(secs => $3) AS bucket_end
               FROM generate_series($1::timestamptz, $2::timestamptz - interval '1 second', make_interval(secs => $3)) gs
           ), m AS (
               SELECT c.app_id, m.channel_id, m.joined_at, COALESCE(m.left_at, 'infinity'::timestamptz) AS ended
               FROM channel_memberships m JOIN channels c ON c.id = m.channel_id
               WHERE m.joined_at < $2 AND (m.left_at IS NULL OR m.left_at > $1)
           ), ov AS (
               SELECT b.bucket, b.bucket_end, m.app_id, m.channel_id, m.joined_at, m.ended
               FROM b JOIN m ON m.joined_at < b.bucket_end AND m.ended > b.bucket
           ), base AS (
               SELECT bucket, app_id,
                      COUNT(*) FILTER (WHERE joined_at < bucket) AS at_start,
                      SUM(GREATEST(EXTRACT(EPOCH FROM (LEAST(ended, bucket_end, NOW()) - GREATEST(joined_at, bucket))), 0)) / 60.0 AS minutes,
                      COUNT(DISTINCT channel_id) AS channels
               FROM ov GROUP BY bucket, app_id
           ), ev AS (
               SELECT bucket, app_id, joined_at AS t, 1 AS d FROM ov WHERE joined_at >= bucket
               UNION ALL
               SELECT bucket, app_id, ended AS t, -1 AS d FROM ov WHERE ended < bucket_end
           ), run AS (
               SELECT bucket, app_id,
                      SUM(d) OVER (PARTITION BY bucket, app_id ORDER BY t, d ROWS UNBOUNDED PRECEDING) AS running
               FROM ev
           ), pk AS (
               SELECT bucket, app_id, GREATEST(MAX(running), 0) AS peak_delta FROM run GROUP BY bucket, app_id
           )
           INSERT INTO usage_app_buckets (app_id, bucket, peak_participants, participant_minutes, active_channels, updated_at)
           SELECT base.app_id, base.bucket, base.at_start + COALESCE(pk.peak_delta, 0), base.minutes, base.channels, NOW()
           FROM base LEFT JOIN pk ON pk.bucket = base.bucket AND pk.app_id = base.app_id
           ON CONFLICT (app_id, bucket) DO UPDATE SET
               peak_participants = EXCLUDED.peak_participants,
               participant_minutes = EXCLUDED.participant_minutes,
               active_channels = EXCLUDED.active_channels,
               updated_at = NOW()"#,
    )
    .bind(from)
    .bind(to)
    .bind(width_secs as f64)
    .execute(&mut *tx)
    .await?;

    // Recordings are attributed to the bucket in which they finished (their length is only
    // known then).
    sqlx::query(
        r#"INSERT INTO usage_app_buckets (app_id, bucket, recording_seconds, updated_at)
           SELECT app_id, to_timestamp(floor(EXTRACT(EPOCH FROM ended_at) / $3) * $3), SUM(duration_secs), NOW()
           FROM recordings
           WHERE kind = 'recording' AND ended_at >= $1 AND ended_at < $2
           GROUP BY app_id, 2
           ON CONFLICT (app_id, bucket) DO UPDATE SET
               recording_seconds = EXCLUDED.recording_seconds,
               updated_at = NOW()"#,
    )
    .bind(from)
    .bind(to)
    .bind(width_secs as f64)
    .execute(&mut *tx)
    .await?;

    tx.commit().await?;
    Ok(sessions)
}

/// Finalizes per-channel buckets of `width_secs` in `[from, to)` (see
/// [`aggregate_app_buckets`] for the semantics).
pub async fn aggregate_channel_buckets(
    pool: &DbPool,
    from: DateTime<Utc>,
    to: DateTime<Utc>,
    width_secs: i64,
) -> Result<u64, sqlx::Error> {
    let mut tx = pool.begin().await?;
    sqlx::query(
        r#"UPDATE usage_channel_buckets SET
               peak_participants = 0, participant_minutes = 0, joins = 0, unique_users = 0, updated_at = NOW()
           WHERE bucket >= $1 AND bucket < $2
             AND (peak_participants <> 0 OR participant_minutes <> 0 OR joins <> 0 OR unique_users <> 0)"#,
    )
    .bind(from)
    .bind(to)
    .execute(&mut *tx)
    .await?;
    let rows = sqlx::query(
        r#"WITH b AS (
               SELECT gs AS bucket, gs + make_interval(secs => $3) AS bucket_end
               FROM generate_series($1::timestamptz, $2::timestamptz - interval '1 second', make_interval(secs => $3)) gs
           ), m AS (
               SELECT c.app_id, m.channel_id, m.user_id, m.joined_at, COALESCE(m.left_at, 'infinity'::timestamptz) AS ended
               FROM channel_memberships m JOIN channels c ON c.id = m.channel_id
               WHERE m.joined_at < $2 AND (m.left_at IS NULL OR m.left_at > $1)
           ), ov AS (
               SELECT b.bucket, b.bucket_end, m.app_id, m.channel_id, m.user_id, m.joined_at, m.ended
               FROM b JOIN m ON m.joined_at < b.bucket_end AND m.ended > b.bucket
           ), base AS (
               SELECT bucket, app_id, channel_id,
                      COUNT(*) FILTER (WHERE joined_at < bucket) AS at_start,
                      SUM(GREATEST(EXTRACT(EPOCH FROM (LEAST(ended, bucket_end, NOW()) - GREATEST(joined_at, bucket))), 0)) / 60.0 AS minutes,
                      COUNT(*) FILTER (WHERE joined_at >= bucket) AS joins,
                      COUNT(DISTINCT user_id) AS users
               FROM ov GROUP BY bucket, app_id, channel_id
           ), ev AS (
               SELECT bucket, channel_id, joined_at AS t, 1 AS d FROM ov WHERE joined_at >= bucket
               UNION ALL
               SELECT bucket, channel_id, ended AS t, -1 AS d FROM ov WHERE ended < bucket_end
           ), run AS (
               SELECT bucket, channel_id,
                      SUM(d) OVER (PARTITION BY bucket, channel_id ORDER BY t, d ROWS UNBOUNDED PRECEDING) AS running
               FROM ev
           ), pk AS (
               SELECT bucket, channel_id, GREATEST(MAX(running), 0) AS peak_delta FROM run GROUP BY bucket, channel_id
           )
           INSERT INTO usage_channel_buckets (app_id, channel_id, bucket, peak_participants, participant_minutes, joins, unique_users, updated_at)
           SELECT base.app_id, base.channel_id, base.bucket, base.at_start + COALESCE(pk.peak_delta, 0), base.minutes, base.joins, base.users, NOW()
           FROM base LEFT JOIN pk ON pk.bucket = base.bucket AND pk.channel_id = base.channel_id
           ON CONFLICT (app_id, channel_id, bucket) DO UPDATE SET
               peak_participants = EXCLUDED.peak_participants,
               participant_minutes = EXCLUDED.participant_minutes,
               joins = EXCLUDED.joins,
               unique_users = EXCLUDED.unique_users,
               updated_at = NOW()"#,
    )
    .bind(from)
    .bind(to)
    .bind(width_secs as f64)
    .execute(&mut *tx)
    .await?
    .rows_affected();
    tx.commit().await?;
    Ok(rows)
}

const METERED_COLUMNS: [&str; 12] = [
    "media_bytes_in",
    "media_bytes_out",
    "chat_messages",
    "tts_requests",
    "tts_characters",
    "stt_audio_ms",
    "quality_samples",
    "mos_sum_milli",
    "rtt_sum_ms",
    "jitter_sum_ms",
    "loss_sum_permille",
    "poor_quality_samples",
];

/// Counters that channel rows carry (media bytes are only known per session, hence per app).
const CHANNEL_METERED_COLUMNS: [&str; 4] = [
    "chat_messages",
    "tts_requests",
    "tts_characters",
    "stt_audio_ms",
];

/// Adds metered counters to their buckets. A delta with a channel is added to that channel's
/// hourly row (when the channel table carries that counter) and to the application's 5-minute
/// row. Deltas with a metric that is not a counter column are ignored.
pub async fn add_counters(
    pool: &DbPool,
    deltas: &[CounterDelta],
    channel_width_secs: i64,
) -> Result<(), sqlx::Error> {
    if deltas.is_empty() {
        return Ok(());
    }
    let mut tx = pool.begin().await?;
    for metric in METERED_COLUMNS {
        let app_rows: Vec<&CounterDelta> = deltas
            .iter()
            .filter(|d| d.metric == metric && d.value > 0)
            .collect();
        if app_rows.is_empty() {
            continue;
        }
        let app_ids: Vec<Uuid> = app_rows.iter().map(|d| d.app_id).collect();
        let buckets: Vec<DateTime<Utc>> = app_rows.iter().map(|d| d.bucket).collect();
        let values: Vec<i64> = app_rows.iter().map(|d| d.value).collect();
        sqlx::query(&format!(
            r#"INSERT INTO usage_app_buckets (app_id, bucket, {metric}, updated_at)
               SELECT app_id, bucket, SUM(value), NOW()
               FROM UNNEST($1::uuid[], $2::timestamptz[], $3::bigint[]) AS t(app_id, bucket, value)
               WHERE EXISTS (SELECT 1 FROM apps a WHERE a.id = t.app_id)
               GROUP BY app_id, bucket
               ON CONFLICT (app_id, bucket) DO UPDATE SET
                   {metric} = usage_app_buckets.{metric} + EXCLUDED.{metric},
                   updated_at = NOW()"#
        ))
        .bind(&app_ids)
        .bind(&buckets)
        .bind(&values)
        .execute(&mut *tx)
        .await?;

        let channel_rows: Vec<&&CounterDelta> =
            app_rows.iter().filter(|d| d.channel_id.is_some()).collect();
        if channel_rows.is_empty() || !CHANNEL_METERED_COLUMNS.contains(&metric) {
            continue;
        }
        let app_ids: Vec<Uuid> = channel_rows.iter().map(|d| d.app_id).collect();
        let channel_ids: Vec<Uuid> = channel_rows.iter().filter_map(|d| d.channel_id).collect();
        let buckets: Vec<DateTime<Utc>> = channel_rows.iter().map(|d| d.bucket).collect();
        let values: Vec<i64> = channel_rows.iter().map(|d| d.value).collect();
        sqlx::query(&format!(
            r#"INSERT INTO usage_channel_buckets (app_id, channel_id, bucket, {metric}, updated_at)
               SELECT app_id, channel_id, to_timestamp(floor(EXTRACT(EPOCH FROM bucket) / $5) * $5), SUM(value), NOW()
               FROM UNNEST($1::uuid[], $2::uuid[], $3::timestamptz[], $4::bigint[]) AS t(app_id, channel_id, bucket, value)
               WHERE EXISTS (SELECT 1 FROM apps a WHERE a.id = t.app_id)
               GROUP BY app_id, channel_id, 3
               ON CONFLICT (app_id, channel_id, bucket) DO UPDATE SET
                   {metric} = usage_channel_buckets.{metric} + EXCLUDED.{metric},
                   updated_at = NOW()"#
        ))
        .bind(&app_ids)
        .bind(&channel_ids)
        .bind(&buckets)
        .bind(&values)
        .bind(channel_width_secs as f64)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    Ok(())
}

/// Application buckets in `[from, to)`, oldest first.
pub async fn app_series(
    pool: &DbPool,
    app_id: Uuid,
    from: DateTime<Utc>,
    to: DateTime<Utc>,
    limit: i64,
) -> Result<Vec<UsageAppBucketRow>, sqlx::Error> {
    sqlx::query_as::<_, UsageAppBucketRow>(
        r#"SELECT * FROM usage_app_buckets
           WHERE app_id = $1 AND bucket >= $2 AND bucket < $3
           ORDER BY bucket ASC LIMIT $4"#,
    )
    .bind(app_id)
    .bind(from)
    .bind(to)
    .bind(limit)
    .fetch_all(pool)
    .await
}

/// Application buckets rolled up to `width_secs` (a multiple of the stored bucket width):
/// sums of the counters, maxima of the peaks.
pub async fn app_series_rollup(
    pool: &DbPool,
    app_id: Uuid,
    from: DateTime<Utc>,
    to: DateTime<Utc>,
    width_secs: i64,
    limit: i64,
) -> Result<Vec<UsageAppBucketRow>, sqlx::Error> {
    sqlx::query_as::<_, UsageAppBucketRow>(
        r#"SELECT app_id,
                  to_timestamp(floor(EXTRACT(EPOCH FROM bucket) / $4) * $4) AS bucket,
                  MAX(peak_sessions) AS peak_sessions,
                  SUM(session_minutes) AS session_minutes,
                  SUM(sessions_started)::bigint AS sessions_started,
                  MAX(unique_users) AS unique_users,
                  MAX(peak_participants) AS peak_participants,
                  SUM(participant_minutes) AS participant_minutes,
                  MAX(active_channels) AS active_channels,
                  SUM(recording_seconds) AS recording_seconds,
                  SUM(media_bytes_in)::bigint AS media_bytes_in,
                  SUM(media_bytes_out)::bigint AS media_bytes_out,
                  SUM(chat_messages)::bigint AS chat_messages,
                  SUM(tts_requests)::bigint AS tts_requests,
                  SUM(tts_characters)::bigint AS tts_characters,
                  SUM(stt_audio_ms)::bigint AS stt_audio_ms,
                  SUM(quality_samples)::bigint AS quality_samples,
                  SUM(mos_sum_milli)::bigint AS mos_sum_milli,
                  SUM(rtt_sum_ms)::bigint AS rtt_sum_ms,
                  SUM(jitter_sum_ms)::bigint AS jitter_sum_ms,
                  SUM(loss_sum_permille)::bigint AS loss_sum_permille,
                  SUM(poor_quality_samples)::bigint AS poor_quality_samples,
                  MAX(updated_at) AS updated_at
           FROM usage_app_buckets
           WHERE app_id = $1 AND bucket >= $2 AND bucket < $3
           GROUP BY app_id, 2
           ORDER BY 2 ASC LIMIT $5"#,
    )
    .bind(app_id)
    .bind(from)
    .bind(to)
    .bind(width_secs as f64)
    .bind(limit)
    .fetch_all(pool)
    .await
}

/// Channel buckets in `[from, to)` for one channel, or for every channel of the application
/// when `channel_id` is `None` (ordered by bucket, then channel).
pub async fn channel_series(
    pool: &DbPool,
    app_id: Uuid,
    channel_id: Option<Uuid>,
    from: DateTime<Utc>,
    to: DateTime<Utc>,
    limit: i64,
) -> Result<Vec<UsageChannelBucketRow>, sqlx::Error> {
    sqlx::query_as::<_, UsageChannelBucketRow>(
        r#"SELECT * FROM usage_channel_buckets
           WHERE app_id = $1 AND ($2::uuid IS NULL OR channel_id = $2) AND bucket >= $3 AND bucket < $4
           ORDER BY bucket ASC, channel_id ASC LIMIT $5"#,
    )
    .bind(app_id)
    .bind(channel_id)
    .bind(from)
    .bind(to)
    .bind(limit)
    .fetch_all(pool)
    .await
}

/// Per-channel totals over `[from, to)`, busiest (most participant-minutes) first.
pub async fn channel_totals(
    pool: &DbPool,
    app_id: Uuid,
    from: DateTime<Utc>,
    to: DateTime<Utc>,
    limit: i64,
) -> Result<Vec<UsageChannelBucketRow>, sqlx::Error> {
    sqlx::query_as::<_, UsageChannelBucketRow>(
        r#"SELECT app_id, channel_id, MIN(bucket) AS bucket,
                  MAX(peak_participants) AS peak_participants,
                  SUM(participant_minutes) AS participant_minutes,
                  SUM(joins)::bigint AS joins,
                  MAX(unique_users) AS unique_users,
                  SUM(chat_messages)::bigint AS chat_messages,
                  SUM(tts_requests)::bigint AS tts_requests,
                  SUM(tts_characters)::bigint AS tts_characters,
                  SUM(stt_audio_ms)::bigint AS stt_audio_ms,
                  MAX(updated_at) AS updated_at
           FROM usage_channel_buckets
           WHERE app_id = $1 AND bucket >= $2 AND bucket < $3
           GROUP BY app_id, channel_id
           ORDER BY participant_minutes DESC, channel_id ASC LIMIT $4"#,
    )
    .bind(app_id)
    .bind(from)
    .bind(to)
    .bind(limit)
    .fetch_all(pool)
    .await
}

/// Range totals for one application (`None` when it has no buckets in the range).
pub async fn app_totals(
    pool: &DbPool,
    app_id: Uuid,
    from: DateTime<Utc>,
    to: DateTime<Utc>,
) -> Result<Option<UsageTotalsRow>, sqlx::Error> {
    sqlx::query_as::<_, UsageTotalsRow>(&format!(
        "{TOTALS_SELECT} WHERE app_id = $1 AND bucket >= $2 AND bucket < $3 GROUP BY app_id"
    ))
    .bind(app_id)
    .bind(from)
    .bind(to)
    .fetch_optional(pool)
    .await
}

/// Range totals for every application with usage in the range (fleet export).
pub async fn all_app_totals(
    pool: &DbPool,
    from: DateTime<Utc>,
    to: DateTime<Utc>,
) -> Result<Vec<UsageTotalsRow>, sqlx::Error> {
    sqlx::query_as::<_, UsageTotalsRow>(&format!(
        "{TOTALS_SELECT} WHERE bucket >= $1 AND bucket < $2 GROUP BY app_id ORDER BY app_id"
    ))
    .bind(from)
    .bind(to)
    .fetch_all(pool)
    .await
}

const TOTALS_SELECT: &str = r#"SELECT app_id,
           MAX(peak_sessions) AS peak_sessions,
           SUM(session_minutes) AS session_minutes,
           SUM(sessions_started)::bigint AS sessions_started,
           MAX(peak_participants) AS peak_participants,
           SUM(participant_minutes) AS participant_minutes,
           SUM(recording_seconds) AS recording_seconds,
           SUM(media_bytes_in)::bigint AS media_bytes_in,
           SUM(media_bytes_out)::bigint AS media_bytes_out,
           SUM(chat_messages)::bigint AS chat_messages,
           SUM(tts_requests)::bigint AS tts_requests,
           SUM(tts_characters)::bigint AS tts_characters,
           SUM(stt_audio_ms)::bigint AS stt_audio_ms,
           SUM(quality_samples)::bigint AS quality_samples,
           SUM(mos_sum_milli)::bigint AS mos_sum_milli,
           SUM(rtt_sum_ms)::bigint AS rtt_sum_ms,
           SUM(jitter_sum_ms)::bigint AS jitter_sum_ms,
           SUM(loss_sum_permille)::bigint AS loss_sum_permille,
           SUM(poor_quality_samples)::bigint AS poor_quality_samples
    FROM usage_app_buckets"#;

/// Every application's buckets in `[from, to)` for the fleet export, oldest first.
pub async fn all_app_series(
    pool: &DbPool,
    from: DateTime<Utc>,
    to: DateTime<Utc>,
    limit: i64,
) -> Result<Vec<UsageAppBucketRow>, sqlx::Error> {
    sqlx::query_as::<_, UsageAppBucketRow>(
        r#"SELECT * FROM usage_app_buckets
           WHERE bucket >= $1 AND bucket < $2
           ORDER BY bucket ASC, app_id ASC LIMIT $3"#,
    )
    .bind(from)
    .bind(to)
    .bind(limit)
    .fetch_all(pool)
    .await
}

/// Participant-minutes of the application accrued since `since`: finalized buckets from
/// `since` up to the watermark plus the exact overlap of every membership with
/// `[max(since, watermark), now)`, so a quota check sees minutes that are still running.
pub async fn participant_minutes_since(
    pool: &DbPool,
    app_id: Uuid,
    since: DateTime<Utc>,
    watermark: DateTime<Utc>,
) -> Result<f64, sqlx::Error> {
    let live_from = since.max(watermark);
    let finalized = sqlx::query_scalar::<_, Option<f64>>(
        "SELECT SUM(participant_minutes) FROM usage_app_buckets WHERE app_id = $1 AND bucket >= $2 AND bucket < $3",
    )
    .bind(app_id)
    .bind(since)
    .bind(live_from)
    .fetch_one(pool)
    .await?
    .unwrap_or(0.0);
    let live = sqlx::query_scalar::<_, Option<f64>>(
        r#"SELECT (SUM(EXTRACT(EPOCH FROM (COALESCE(m.left_at, NOW()) - GREATEST(m.joined_at, $2)))) / 60.0)::double precision
           FROM channel_memberships m JOIN channels c ON c.id = m.channel_id
           WHERE c.app_id = $1 AND m.joined_at < NOW() AND (m.left_at IS NULL OR m.left_at > $2)"#,
    )
    .bind(app_id)
    .bind(live_from)
    .fetch_one(pool)
    .await?
    .unwrap_or(0.0);
    Ok(finalized + live.max(0.0))
}

/// Drops application buckets older than `before`; returns rows removed.
pub async fn delete_app_buckets_before(
    pool: &DbPool,
    before: DateTime<Utc>,
) -> Result<u64, sqlx::Error> {
    let r = sqlx::query("DELETE FROM usage_app_buckets WHERE bucket < $1")
        .bind(before)
        .execute(pool)
        .await?;
    Ok(r.rows_affected())
}

pub async fn delete_channel_buckets_before(
    pool: &DbPool,
    before: DateTime<Utc>,
) -> Result<u64, sqlx::Error> {
    let r = sqlx::query("DELETE FROM usage_channel_buckets WHERE bucket < $1")
        .bind(before)
        .execute(pool)
        .await?;
    Ok(r.rows_affected())
}
