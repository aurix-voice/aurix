//! User erasure, data export and the cluster-wide retention sweep.
//!
//! Erasure (`DELETE /v1/users/:id`, or the inactivity rule) removes every row keyed to the
//! user in one transaction, deletes their recordings (files/objects first, rows with the
//! transaction), writes a tombstone so session tokens minted before the deletion cannot
//! silently re-create the user, and publishes `user.deleted` so every node closes the user's
//! live sessions. Moderation history (events about the user, bans) is kept by default because
//! it is the operator's evidence, not the player's data; `purge_moderation` removes it too.
//!
//! The sweep runs on every node but only one executes at a time (Postgres advisory lock), in
//! bounded batches so it never holds long locks on hot tables.

use crate::event_bus::{EventBus, ServerEvent};
use crate::redis_store::RedisStore;
use aurix_common::audit::AuditLogger;
use aurix_common::config::RetentionConfig;
use aurix_common::error::{AurixError, Result};
use aurix_common::sink::UserMediaPurger;
use aurix_common::types::{AppId, AuditAction, UserId};
use aurix_db::models::{
    BanRow, ChannelMembershipRow, ChatMessageRow, ModerationEventRow, RecordingRow, SessionRow,
    UserErasureCounts, UserRow,
};
use aurix_db::{DbError, DbPool};
use chrono::{DateTime, Utc};
use parking_lot::RwLock;
use serde::Serialize;
use std::sync::Arc;
use std::time::Duration;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

/// Advisory-lock key of the retention sweep (arbitrary, unique within Aurix).
const RETENTION_LOCK_KEY: i64 = 0x4155_5249_5845_5254; // "AURIXERT"

/// Rows per collection in an export; the response marks collections that hit it.
pub const EXPORT_LIMIT: i64 = 10_000;

/// Idle users erased per sweep iteration (each erasure is its own transaction).
const INACTIVE_USERS_PER_ITERATION: i64 = 200;

pub struct DeleteUserRequest {
    pub app_id: AppId,
    pub user_id: UserId,
    pub actor: UserId,
    pub ip: Option<String>,
    /// Also remove moderation events about the user and their bans.
    pub purge_moderation: bool,
    /// Set by the inactivity sweep (reflected in the event and audit entry).
    pub automatic: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct UserDeletion {
    pub user_id: UserId,
    pub deleted_at: DateTime<Utc>,
    pub recordings_removed: u64,
    pub rows_removed: UserErasureCounts,
}

#[derive(Debug, Clone, Serialize)]
pub struct UserBlocks {
    pub blocking: Vec<UserId>,
    pub blocked_by: Vec<UserId>,
}

#[derive(Debug, Clone, Serialize)]
pub struct UserModerationExport {
    pub about_user: Vec<ModerationEventRow>,
    pub reported_by_user: Vec<ModerationEventRow>,
}

/// Everything Aurix stores about one user, for data-portability requests.
#[derive(Debug, Clone, Serialize)]
pub struct UserExport {
    pub format: &'static str,
    pub exported_at: DateTime<Utc>,
    pub app_id: AppId,
    pub user: UserRow,
    pub sessions: Vec<SessionRow>,
    pub channel_memberships: Vec<ChannelMembershipRow>,
    pub chat_messages: Vec<ChatMessageRow>,
    pub blocks: UserBlocks,
    pub bans: Vec<BanRow>,
    pub moderation: UserModerationExport,
    pub recordings: Vec<RecordingRow>,
    /// Collections cut at [`EXPORT_LIMIT`] rows (newest first).
    pub truncated: Vec<&'static str>,
}

pub struct UserLifecycle {
    pool: DbPool,
    events: Arc<EventBus>,
    audit: Arc<AuditLogger>,
    redis: Option<Arc<RedisStore>>,
    media: RwLock<Option<Arc<dyn UserMediaPurger>>>,
}

impl UserLifecycle {
    pub fn new(
        pool: DbPool,
        events: Arc<EventBus>,
        audit: Arc<AuditLogger>,
        redis: Option<Arc<RedisStore>>,
    ) -> Self {
        Self {
            pool,
            events,
            audit,
            redis,
            media: RwLock::new(None),
        }
    }

    /// Registers the recording subsystem (constructed after the control plane).
    pub fn set_media_purger(&self, purger: Arc<dyn UserMediaPurger>) {
        *self.media.write() = Some(purger);
    }

    /// Erases a user. `Ok(None)` when no such user exists in the app.
    pub async fn delete_user(&self, req: DeleteUserRequest) -> Result<Option<UserDeletion>> {
        let db = |e: DbError| AurixError::Database(format!("User erasure failed: {e}"));
        if aurix_db::queries::get_user(&self.pool, req.app_id.0, req.user_id.0)
            .await
            .map_err(db)?
            .is_none()
        {
            return Ok(None);
        }

        let purger = self.media.read().clone();
        let recordings_removed = match purger {
            Some(p) => p.purge_user_media(req.app_id, req.user_id).await?,
            None => 0,
        };

        let Some(rows_removed) = aurix_db::queries::erase_user(
            &self.pool,
            req.app_id.0,
            req.user_id.0,
            req.purge_moderation,
        )
        .await
        .map_err(db)?
        else {
            return Ok(None);
        };
        let deleted_at = Utc::now();

        if let Some(redis) = &self.redis {
            if let Err(e) = redis.forget_user(req.user_id).await {
                warn!("Redis cleanup after erasing {} failed: {e}", req.user_id);
            }
        }

        self.events.publish(ServerEvent::UserDeleted {
            app_id: req.app_id,
            user_id: req.user_id,
            deleted_by: req.actor,
            automatic: req.automatic,
            timestamp: deleted_at,
        });
        self.audit.log(
            Some(req.app_id),
            req.actor,
            AuditAction::UserDeleted,
            "user",
            &req.user_id.to_string(),
            serde_json::json!({
                "automatic": req.automatic,
                "purge_moderation": req.purge_moderation,
                "recordings_removed": recordings_removed,
                "rows_removed": rows_removed,
            }),
            req.ip,
        );
        info!(
            "User {} erased from app {} ({} sessions, {} chat messages, {} recordings)",
            req.user_id,
            req.app_id,
            rows_removed.sessions,
            rows_removed.chat_messages,
            recordings_removed
        );
        Ok(Some(UserDeletion {
            user_id: req.user_id,
            deleted_at,
            recordings_removed,
            rows_removed,
        }))
    }

    /// Collects everything stored about a user. `Ok(None)` when the user does not exist.
    pub async fn export_user(
        &self,
        app_id: AppId,
        user_id: UserId,
        actor: UserId,
        ip: Option<String>,
    ) -> Result<Option<UserExport>> {
        let db = |e: DbError| AurixError::Database(format!("User export failed: {e}"));
        let pool = &self.pool;
        let (a, u) = (app_id.0, user_id.0);
        let Some(user) = aurix_db::queries::get_user(pool, a, u).await.map_err(db)? else {
            return Ok(None);
        };
        let mut truncated = Vec::new();
        let mut track = |name: &'static str, len: usize| {
            if len as i64 >= EXPORT_LIMIT {
                truncated.push(name);
            }
        };

        let sessions = aurix_db::queries::export_user_sessions(pool, a, u, EXPORT_LIMIT)
            .await
            .map_err(db)?;
        track("sessions", sessions.len());
        let channel_memberships = aurix_db::queries::export_user_memberships(pool, u, EXPORT_LIMIT)
            .await
            .map_err(db)?;
        track("channel_memberships", channel_memberships.len());
        let chat_messages = aurix_db::queries::export_user_chat_messages(pool, a, u, EXPORT_LIMIT)
            .await
            .map_err(db)?;
        track("chat_messages", chat_messages.len());
        let blocking = aurix_db::queries::list_user_blocks(pool, a, u)
            .await
            .map_err(db)?
            .into_iter()
            .map(UserId::from_uuid)
            .collect();
        let blocked_by = aurix_db::queries::list_user_blocked_by(pool, a, u)
            .await
            .map_err(db)?
            .into_iter()
            .map(UserId::from_uuid)
            .collect();
        let bans = aurix_db::queries::export_user_bans(pool, a, u, EXPORT_LIMIT)
            .await
            .map_err(db)?;
        track("bans", bans.len());
        let about_user =
            aurix_db::queries::export_moderation_events_about(pool, a, u, EXPORT_LIMIT)
                .await
                .map_err(db)?;
        track("moderation.about_user", about_user.len());
        let reported_by_user =
            aurix_db::queries::export_moderation_events_reported_by(pool, a, u, EXPORT_LIMIT)
                .await
                .map_err(db)?;
        track("moderation.reported_by_user", reported_by_user.len());
        let recordings = aurix_db::queries::list_recordings_for_user(pool, a, u, EXPORT_LIMIT)
            .await
            .map_err(db)?;
        track("recordings", recordings.len());

        self.audit.log(
            Some(app_id),
            actor,
            AuditAction::UserDataExported,
            "user",
            &user_id.to_string(),
            serde_json::json!({ "truncated": truncated }),
            ip,
        );
        Ok(Some(UserExport {
            format: "aurix.user_export.v1",
            exported_at: Utc::now(),
            app_id,
            user,
            sessions,
            channel_memberships,
            chat_messages,
            blocks: UserBlocks {
                blocking,
                blocked_by,
            },
            bans,
            moderation: UserModerationExport {
                about_user,
                reported_by_user,
            },
            recordings,
            truncated,
        }))
    }
}

/// Rows removed by one sweep, per rule.
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize)]
pub struct SweepReport {
    pub sessions: u64,
    pub moderation_events: u64,
    pub audit_log: u64,
    pub analytics: u64,
    pub tombstones: u64,
    pub inactive_users: u64,
}

impl SweepReport {
    pub fn total(&self) -> u64 {
        self.sessions
            + self.moderation_events
            + self.audit_log
            + self.analytics
            + self.tombstones
            + self.inactive_users
    }
}

pub struct RetentionService {
    cfg: RetentionConfig,
    pool: DbPool,
    users: Arc<UserLifecycle>,
    audit: Arc<AuditLogger>,
}

impl RetentionService {
    pub fn new(
        cfg: RetentionConfig,
        pool: DbPool,
        users: Arc<UserLifecycle>,
        audit: Arc<AuditLogger>,
    ) -> Self {
        Self {
            cfg,
            pool,
            users,
            audit,
        }
    }

    pub fn enabled(&self) -> bool {
        self.cfg.enabled
    }

    /// Runs the sweep every `interval_secs` until cancelled.
    pub async fn run(self: Arc<Self>, cancel: CancellationToken) {
        if !self.cfg.enabled {
            return;
        }
        let period = Duration::from_secs(self.cfg.interval_secs);
        // First pass one period after boot so a fleet restart does not sweep at once;
        // operators can force a pass with `POST /admin/retention/sweep`.
        let mut interval = tokio::time::interval_at(tokio::time::Instant::now() + period, period);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                _ = interval.tick() => {}
                _ = cancel.cancelled() => return,
            }
            match self.sweep_once().await {
                Ok(Some(report)) if report.total() > 0 => {
                    info!(
                        "Retention sweep removed {} rows: {report:?}",
                        report.total()
                    );
                }
                Ok(_) => {}
                Err(e) => warn!("Retention sweep failed: {e}"),
            }
        }
    }

    /// One full pass. `Ok(None)` when another node holds the sweep lock.
    pub async fn sweep_once(&self) -> Result<Option<SweepReport>> {
        let db = |e: DbError| AurixError::Database(format!("Retention sweep failed: {e}"));
        let mut lock_conn = self.pool.acquire().await.map_err(db)?;
        if !aurix_db::queries::try_advisory_lock(&mut lock_conn, RETENTION_LOCK_KEY)
            .await
            .map_err(db)?
        {
            return Ok(None);
        }
        let result = self.sweep_locked().await;
        if let Err(e) = aurix_db::queries::advisory_unlock(&mut lock_conn, RETENTION_LOCK_KEY).await
        {
            warn!("Retention lock release failed (connection will be recycled): {e}");
            // A connection whose lock state is unknown must not return to the pool.
            let _ = lock_conn.close().await;
        }
        let report = result?;
        if report.total() > 0 {
            self.audit.log(
                None,
                UserId::from_uuid(uuid::Uuid::nil()),
                AuditAction::RetentionSweep,
                "retention",
                "sweep",
                serde_json::to_value(&report).unwrap_or(serde_json::Value::Null),
                None,
            );
        }
        Ok(Some(report))
    }

    async fn sweep_locked(&self) -> Result<SweepReport> {
        let db = |e: DbError| AurixError::Database(format!("Retention sweep failed: {e}"));
        let now = Utc::now();
        let batch = i64::from(self.cfg.batch_size);
        let mut report = SweepReport::default();

        if let Some(cutoff) = cutoff(now, self.cfg.sessions_days) {
            report.sessions = drain(|| {
                aurix_db::queries::delete_closed_sessions_before(&self.pool, cutoff, batch)
            })
            .await
            .map_err(db)?;
        }
        if let Some(cutoff) = cutoff(now, self.cfg.moderation_events_days) {
            report.moderation_events = drain(|| {
                aurix_db::queries::delete_resolved_moderation_events_before(
                    &self.pool, cutoff, batch,
                )
            })
            .await
            .map_err(db)?;
        }
        if let Some(cutoff) = cutoff(now, self.cfg.audit_log_days) {
            report.audit_log =
                drain(|| aurix_db::queries::delete_audit_log_before(&self.pool, cutoff, batch))
                    .await
                    .map_err(db)?;
        }
        if let Some(cutoff) = cutoff(now, self.cfg.analytics_days) {
            report.analytics =
                drain(|| aurix_db::queries::delete_analytics_before(&self.pool, cutoff, batch))
                    .await
                    .map_err(db)?;
        }
        if let Some(cutoff) = cutoff(now, self.cfg.tombstones_days) {
            report.tombstones = aurix_db::queries::delete_tombstones_before(&self.pool, cutoff)
                .await
                .map_err(db)?;
        }
        if let Some(cutoff) = cutoff(now, self.cfg.inactive_users_days) {
            report.inactive_users = self.erase_inactive_users(cutoff).await?;
        }
        Ok(report)
    }

    async fn erase_inactive_users(&self, cutoff: DateTime<Utc>) -> Result<u64> {
        let db = |e: DbError| AurixError::Database(format!("Retention sweep failed: {e}"));
        let mut erased = 0u64;
        loop {
            let idle = aurix_db::queries::list_inactive_users(
                &self.pool,
                cutoff,
                INACTIVE_USERS_PER_ITERATION,
            )
            .await
            .map_err(db)?;
            if idle.is_empty() {
                return Ok(erased);
            }
            let mut progressed = false;
            for (app_id, user_id) in idle {
                let done = self
                    .users
                    .delete_user(DeleteUserRequest {
                        app_id: AppId::from_uuid(app_id),
                        user_id: UserId::from_uuid(user_id),
                        actor: UserId::from_uuid(uuid::Uuid::nil()),
                        ip: None,
                        purge_moderation: false,
                        automatic: true,
                    })
                    .await?;
                if done.is_some() {
                    erased += 1;
                    progressed = true;
                }
            }
            if !progressed {
                return Ok(erased);
            }
        }
    }
}

fn cutoff(now: DateTime<Utc>, days: u32) -> Option<DateTime<Utc>> {
    (days > 0).then(|| now - chrono::Duration::days(i64::from(days)))
}

/// Repeats a batched delete until it removes nothing more; returns the total.
async fn drain<F, Fut>(mut step: F) -> std::result::Result<u64, DbError>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = std::result::Result<u64, DbError>>,
{
    let mut total = 0u64;
    loop {
        let n = step().await?;
        total += n;
        if n == 0 {
            return Ok(total);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zero_days_disables_rule() {
        let now = Utc::now();
        assert!(cutoff(now, 0).is_none());
        assert_eq!(cutoff(now, 2), Some(now - chrono::Duration::days(2)));
    }

    #[tokio::test]
    async fn drain_sums_until_empty_batch() {
        let mut batches = vec![5u64, 3].into_iter();
        let total = drain(|| {
            let n = batches.next().unwrap_or(0);
            async move { Ok(n) }
        })
        .await
        .unwrap();
        assert_eq!(total, 8);
    }

    #[test]
    fn report_total_sums_every_rule() {
        let r = SweepReport {
            sessions: 1,
            moderation_events: 2,
            audit_log: 3,
            analytics: 4,
            tombstones: 5,
            inactive_users: 6,
        };
        assert_eq!(r.total(), 21);
    }
}
