//! Usage accounting and per-application quotas.
//!
//! Two flows feed the `usage_*` tables:
//!
//! * **Metered counters** (media bytes, chat messages, TTS, STT audio) are counted on every
//!   node in an in-memory [`UsageMeter`] and flushed to Postgres every
//!   `usage.flush_interval_secs` as additive deltas, so any number of nodes can write the same
//!   bucket concurrently.
//! * **Derived series** (peak concurrent sessions / participants, session and participant
//!   minutes, unique users, active channels, recording seconds) are computed from the
//!   `sessions`, `channel_memberships` and `recordings` ledgers by one aggregator at a time
//!   (Postgres advisory lock). Each run re-derives the buckets from `LOOKBACK` before the
//!   watermark up to the bucket containing the present, so late closes (a lost node's sessions
//!   are ended at its last heartbeat once the reaper notices) and the running bucket converge.
//!
//! Quotas read the same data: concurrent sessions from the open `sessions` rows, monthly
//! participant-minutes from finalized buckets plus the exact overlap of memberships since the
//! watermark, cached per application for `usage.quota_cache_secs`.

use aurix_common::config::UsageConfig;
use aurix_common::error::{AurixError, Result};
use aurix_common::types::{AppId, ChannelId};
use aurix_common::usage::{
    UsageDelta, UsageMeter, UsageMetric, APP_BUCKET_SECS, CHANNEL_BUCKET_SECS,
};
use aurix_db::usage::{self as db, CounterDelta, SCOPE_APP, SCOPE_CHANNEL};
use aurix_db::{DbError, DbPool};
use chrono::{DateTime, Datelike, Duration, TimeZone, Utc};
use dashmap::DashMap;
use parking_lot::Mutex;
use std::sync::{Arc, OnceLock};
use std::time::Instant;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

const USAGE_LOCK_KEY: i64 = 0x4155_5249_5855_5347; // "AURIXUSG"
/// Buckets this far before the watermark are re-derived on every run.
const LOOKBACK: Duration = Duration::minutes(15);
/// Buckets derived per transaction while backfilling.
const APP_BATCH_BUCKETS: i64 = 288;
const CHANNEL_BATCH_BUCKETS: i64 = 24;
const RETENTION_EVERY_SECS: u64 = 3600;

#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct AggregateReport {
    pub app_from: Option<DateTime<Utc>>,
    pub app_to: Option<DateTime<Utc>>,
    pub channel_from: Option<DateTime<Utc>>,
    pub channel_to: Option<DateTime<Utc>>,
    pub app_rows: u64,
    pub channel_rows: u64,
}

/// What a tenant may currently do and how much it has used this month.
#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize)]
pub struct QuotaState {
    pub active_sessions: i64,
    pub max_concurrent_sessions: i64,
    pub month_start: DateTime<Utc>,
    pub participant_minutes_this_month: f64,
    pub monthly_participant_minutes: i64,
}

struct CachedMinutes {
    at: Instant,
    minutes: f64,
}

/// Per-application quota settings (`0` = unlimited).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AppLimits {
    pub max_concurrent_sessions: i32,
    pub monthly_participant_minutes: i64,
}

struct CachedLimits {
    at: Instant,
    limits: AppLimits,
}

pub struct UsageService {
    cfg: UsageConfig,
    pool: DbPool,
    meter: Arc<UsageMeter>,
    minutes_cache: DashMap<AppId, CachedMinutes>,
    limits_cache: DashMap<AppId, CachedLimits>,
    last_retention: Mutex<Option<Instant>>,
    pre_flush: OnceLock<Box<dyn Fn() + Send + Sync>>,
}

impl UsageService {
    pub fn new(cfg: UsageConfig, pool: DbPool) -> Self {
        Self {
            cfg,
            pool,
            meter: Arc::new(UsageMeter::new()),
            minutes_cache: DashMap::new(),
            limits_cache: DashMap::new(),
            pre_flush: OnceLock::new(),
            last_retention: Mutex::new(None),
        }
    }

    pub fn config(&self) -> &UsageConfig {
        &self.cfg
    }

    /// The node-local meter shared with the SFU, chat, speech and STT paths.
    pub fn meter(&self) -> Arc<UsageMeter> {
        self.meter.clone()
    }

    /// Runs before every flush so batching producers (the SFU's per-session byte counters)
    /// can hand their totals to the meter first.
    pub fn set_pre_flush<F>(&self, f: F)
    where
        F: Fn() + Send + Sync + 'static,
    {
        let _ = self.pre_flush.set(Box::new(f));
    }

    /// Counts one metered event for `app_id` (and `channel_id` when channel-scoped).
    pub fn record(
        &self,
        app_id: AppId,
        channel_id: Option<ChannelId>,
        metric: UsageMetric,
        value: u64,
    ) {
        if self.cfg.enabled {
            self.meter.record(app_id, channel_id, metric, value);
        }
    }

    /// Starts the flush and aggregation loops; both stop when `cancel` fires.
    pub fn start(self: &Arc<Self>, cancel: CancellationToken) {
        if !self.cfg.enabled {
            info!("Usage accounting disabled");
            return;
        }
        let svc = self.clone();
        let flush_cancel = cancel.clone();
        tokio::spawn(async move {
            let mut interval =
                tokio::time::interval(std::time::Duration::from_secs(svc.cfg.flush_interval_secs));
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                tokio::select! {
                    _ = interval.tick() => {}
                    _ = flush_cancel.cancelled() => {
                        if let Err(e) = svc.flush_once().await {
                            warn!("Final usage flush failed: {e}");
                        }
                        return;
                    }
                }
                if let Err(e) = svc.flush_once().await {
                    warn!("Usage flush failed (deltas kept for the next flush): {e}");
                }
            }
        });
        let svc = self.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(
                svc.cfg.aggregate_interval_secs,
            ));
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                tokio::select! {
                    _ = interval.tick() => {}
                    _ = cancel.cancelled() => return,
                }
                match svc.aggregate_once().await {
                    Ok(Some(report)) => debug!(?report, "Usage aggregation run"),
                    Ok(None) => {}
                    Err(e) => warn!("Usage aggregation failed: {e}"),
                }
            }
        });
    }

    /// Writes every pending metered delta to Postgres. Deltas are put back on failure.
    pub async fn flush_once(&self) -> Result<usize> {
        if let Some(hook) = self.pre_flush.get() {
            hook();
        }
        let deltas = self.meter.drain();
        if deltas.is_empty() {
            return Ok(0);
        }
        let rows: Vec<CounterDelta> = deltas.iter().map(to_counter).collect();
        match db::add_counters(&self.pool, &rows, CHANNEL_BUCKET_SECS).await {
            Ok(()) => {
                aurix_metrics::USAGE_FLUSHED.inc_by(rows.len() as u64);
                Ok(rows.len())
            }
            Err(e) => {
                self.meter.restore(&deltas);
                Err(AurixError::Database(format!("usage flush: {e}")))
            }
        }
    }

    /// One aggregation pass. `Ok(None)` when another node holds the aggregation lock.
    pub async fn aggregate_once(&self) -> Result<Option<AggregateReport>> {
        let dbe = |e: DbError| AurixError::Database(format!("usage aggregation: {e}"));
        let mut lock_conn = self.pool.acquire().await.map_err(dbe)?;
        if !aurix_db::queries::try_advisory_lock(&mut lock_conn, USAGE_LOCK_KEY)
            .await
            .map_err(dbe)?
        {
            return Ok(None);
        }
        let result = self.aggregate_locked().await;
        if let Err(e) = aurix_db::queries::advisory_unlock(&mut lock_conn, USAGE_LOCK_KEY).await {
            warn!("Usage lock release failed (connection will be recycled): {e}");
            let _ = lock_conn.close().await;
        }
        result.map(Some)
    }

    async fn aggregate_locked(&self) -> Result<AggregateReport> {
        let dbe = |e: DbError| AurixError::Database(format!("usage aggregation: {e}"));
        let now = Utc::now();
        let mut report = AggregateReport::default();
        let Some(earliest) = db::earliest_session(&self.pool).await.map_err(dbe)? else {
            return Ok(report);
        };
        // A fresh deployment backfills from its first session, but never past retention.
        let earliest = earliest.max(now - Duration::days(i64::from(self.cfg.retention_days)));

        for (scope, width, batch) in [
            (SCOPE_APP, APP_BUCKET_SECS, APP_BATCH_BUCKETS),
            (SCOPE_CHANNEL, CHANNEL_BUCKET_SECS, CHANNEL_BATCH_BUCKETS),
        ] {
            let watermark = db::get_watermark(&self.pool, scope).await.map_err(dbe)?;
            let start = match watermark {
                Some(w) => floor_to(w - LOOKBACK, width).max(floor_to(earliest, width)),
                None => floor_to(earliest, width),
            };
            // Up to and including the bucket that contains `now`.
            let end = floor_to(now, width) + Duration::seconds(width);
            let mut from = start;
            let mut rows = 0;
            while from < end {
                let to = (from + Duration::seconds(width * batch)).min(end);
                rows += if scope == SCOPE_APP {
                    db::aggregate_app_buckets(&self.pool, from, to, width).await
                } else {
                    db::aggregate_channel_buckets(&self.pool, from, to, width).await
                }
                .map_err(dbe)?;
                from = to;
                tokio::task::yield_now().await;
            }
            db::set_watermark(&self.pool, scope, floor_to(now, width))
                .await
                .map_err(dbe)?;
            if scope == SCOPE_APP {
                report.app_from = Some(start);
                report.app_to = Some(end);
                report.app_rows = rows;
            } else {
                report.channel_from = Some(start);
                report.channel_to = Some(end);
                report.channel_rows = rows;
            }
        }

        let due = self
            .last_retention
            .lock()
            .is_none_or(|t| t.elapsed().as_secs() >= RETENTION_EVERY_SECS);
        if due {
            let app_cutoff = now - Duration::days(i64::from(self.cfg.retention_days));
            let channel_cutoff = now - Duration::days(i64::from(self.cfg.channel_retention_days));
            let a = db::delete_app_buckets_before(&self.pool, app_cutoff)
                .await
                .map_err(dbe)?;
            let c = db::delete_channel_buckets_before(&self.pool, channel_cutoff)
                .await
                .map_err(dbe)?;
            if a + c > 0 {
                info!("Usage retention removed {a} app and {c} channel buckets");
            }
            *self.last_retention.lock() = Some(Instant::now());
        }
        Ok(report)
    }

    /// First instant of the current UTC calendar month.
    pub fn month_start(now: DateTime<Utc>) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(now.year(), now.month(), 1, 0, 0, 0)
            .single()
            .unwrap_or(now)
    }

    /// Latest finalized application bucket boundary (`None` before the first aggregation).
    pub async fn app_watermark(&self) -> Result<Option<DateTime<Utc>>> {
        db::get_watermark(&self.pool, SCOPE_APP)
            .await
            .map_err(|e| AurixError::Database(format!("usage watermark: {e}")))
    }

    /// Latest finalized channel bucket boundary (`None` before the first aggregation).
    pub async fn channel_watermark(&self) -> Result<Option<DateTime<Utc>>> {
        db::get_watermark(&self.pool, SCOPE_CHANNEL)
            .await
            .map_err(|e| AurixError::Database(format!("usage watermark: {e}")))
    }

    /// Participant-minutes used by `app_id` in the current UTC month (finalized buckets plus
    /// live memberships), cached for `usage.quota_cache_secs`.
    pub async fn participant_minutes_this_month(&self, app_id: AppId) -> Result<f64> {
        let ttl = std::time::Duration::from_secs(self.cfg.quota_cache_secs);
        if let Some(c) = self.minutes_cache.get(&app_id) {
            if c.at.elapsed() < ttl {
                return Ok(c.minutes);
            }
        }
        let minutes = self.participant_minutes_uncached(app_id).await?;
        self.minutes_cache.insert(
            app_id,
            CachedMinutes {
                at: Instant::now(),
                minutes,
            },
        );
        Ok(minutes)
    }

    async fn participant_minutes_uncached(&self, app_id: AppId) -> Result<f64> {
        let dbe = |e: DbError| AurixError::Database(format!("usage quota: {e}"));
        let now = Utc::now();
        let since = Self::month_start(now);
        let watermark = db::get_watermark(&self.pool, SCOPE_APP)
            .await
            .map_err(dbe)?
            .unwrap_or(since);
        db::participant_minutes_since(&self.pool, app_id.0, since, watermark)
            .await
            .map_err(dbe)
    }

    /// Drops the cached month total and limits for `app_id` (after its quota changed). Other
    /// nodes pick the change up when their cache expires (`usage.quota_cache_secs`).
    pub fn forget_cached(&self, app_id: AppId) {
        self.minutes_cache.remove(&app_id);
        self.limits_cache.remove(&app_id);
    }

    /// The application's quota settings, cached for `usage.quota_cache_secs`. An unknown
    /// application (deleted since the token was issued) is `NotFound`.
    pub async fn app_limits(&self, app_id: AppId) -> Result<AppLimits> {
        let ttl = std::time::Duration::from_secs(self.cfg.quota_cache_secs);
        if let Some(c) = self.limits_cache.get(&app_id) {
            if c.at.elapsed() < ttl {
                return Ok(c.limits);
            }
        }
        let app = aurix_db::queries::get_app(&self.pool, app_id.0)
            .await
            .map_err(|e| AurixError::Database(format!("usage quota: {e}")))?
            .ok_or_else(|| {
                AurixError::AuthorizationDenied("Application not found or inactive".into())
            })?;
        let limits = AppLimits {
            max_concurrent_sessions: app.max_concurrent_sessions,
            // Minutes are only known when the aggregator runs.
            monthly_participant_minutes: if self.cfg.enabled {
                app.monthly_participant_minutes
            } else {
                0
            },
        };
        self.limits_cache.insert(
            app_id,
            CachedLimits {
                at: Instant::now(),
                limits,
            },
        );
        Ok(limits)
    }

    /// Refuses a channel join once the application has used `monthly_minutes`
    /// participant-minutes this UTC month (`0` = unlimited).
    pub async fn check_minutes_quota(&self, app_id: AppId, monthly_minutes: i64) -> Result<()> {
        if monthly_minutes <= 0 {
            return Ok(());
        }
        let used = self.participant_minutes_this_month(app_id).await?;
        if used >= monthly_minutes as f64 {
            aurix_metrics::QUOTA_REJECTIONS
                .with_label_values(&["participant_minutes"])
                .inc();
            return Err(AurixError::QuotaExceeded(format!(
                "application has used its monthly participant-minutes ({monthly_minutes})"
            )));
        }
        Ok(())
    }

    pub async fn quota_state(
        &self,
        app_id: AppId,
        max_concurrent: i32,
        monthly_minutes: i64,
    ) -> Result<QuotaState> {
        let active = aurix_db::queries::count_active_sessions(&self.pool, app_id.0)
            .await
            .map_err(|e| AurixError::Database(format!("usage quota: {e}")))?;
        Ok(QuotaState {
            active_sessions: active,
            max_concurrent_sessions: i64::from(max_concurrent),
            month_start: Self::month_start(Utc::now()),
            participant_minutes_this_month: self.participant_minutes_uncached(app_id).await?,
            monthly_participant_minutes: monthly_minutes,
        })
    }
}

fn to_counter(d: &UsageDelta) -> CounterDelta {
    CounterDelta {
        app_id: d.key.app_id.0,
        channel_id: d.key.channel_id.map(|c| c.0),
        bucket: d.key.bucket,
        metric: d.key.metric.as_str(),
        value: i64::try_from(d.value).unwrap_or(i64::MAX),
    }
}

/// Start of the `width`-second bucket containing `t`.
pub fn floor_to(t: DateTime<Utc>, width: i64) -> DateTime<Utc> {
    aurix_common::usage::bucket_start(t, width)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn month_start_is_first_midnight_utc() {
        let t = Utc.with_ymd_and_hms(2026, 9, 19, 7, 50, 3).unwrap();
        assert_eq!(
            UsageService::month_start(t),
            Utc.with_ymd_and_hms(2026, 9, 1, 0, 0, 0).unwrap()
        );
    }

    #[test]
    fn floor_aligns_to_epoch_multiples() {
        let t = Utc.with_ymd_and_hms(2026, 9, 19, 7, 53, 3).unwrap();
        assert_eq!(
            floor_to(t, 300),
            Utc.with_ymd_and_hms(2026, 9, 19, 7, 50, 0).unwrap()
        );
        assert_eq!(
            floor_to(t, 3600),
            Utc.with_ymd_and_hms(2026, 9, 19, 7, 0, 0).unwrap()
        );
    }
}
