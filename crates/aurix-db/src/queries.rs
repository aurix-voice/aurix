use crate::models::*;
use crate::DbPool;
use chrono::{DateTime, Utc};
use uuid::Uuid;

// ── App Queries ──

pub async fn create_app(pool: &DbPool, app: &AppRow) -> Result<AppRow, sqlx::Error> {
    sqlx::query_as::<_, AppRow>(
        r#"INSERT INTO apps (id, name, description, owner_id, api_key_hash, api_secret_hash, active, max_channels, max_participants_per_channel, created_at, updated_at)
           VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11)
           RETURNING *"#
    )
    .bind(app.id).bind(&app.name).bind(&app.description).bind(app.owner_id)
    .bind(&app.api_key_hash).bind(&app.api_secret_hash).bind(app.active)
    .bind(app.max_channels).bind(app.max_participants_per_channel)
    .bind(app.created_at).bind(app.updated_at)
    .fetch_one(pool).await
}

pub async fn get_app(pool: &DbPool, id: Uuid) -> Result<Option<AppRow>, sqlx::Error> {
    sqlx::query_as::<_, AppRow>("SELECT * FROM apps WHERE id = $1 AND active = true")
        .bind(id)
        .fetch_optional(pool)
        .await
}

pub async fn list_apps(pool: &DbPool, limit: i64, offset: i64) -> Result<Vec<AppRow>, sqlx::Error> {
    sqlx::query_as::<_, AppRow>(
        "SELECT * FROM apps WHERE active = true ORDER BY created_at DESC LIMIT $1 OFFSET $2",
    )
    .bind(limit)
    .bind(offset)
    .fetch_all(pool)
    .await
}

pub async fn count_apps(pool: &DbPool) -> Result<i64, sqlx::Error> {
    let row: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM apps WHERE active = true")
        .fetch_one(pool)
        .await?;
    Ok(row.0)
}

pub async fn update_app_key_hash(
    pool: &DbPool,
    app_id: uuid::Uuid,
    key_hash: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE apps SET api_key_hash = $1, updated_at = NOW() WHERE id = $2")
        .bind(key_hash)
        .bind(app_id)
        .execute(pool)
        .await?;
    Ok(())
}

/// Updates an app's editable settings; `None` keeps the current value.
pub async fn update_app(
    pool: &DbPool,
    app_id: uuid::Uuid,
    name: Option<&str>,
    description: Option<Option<&str>>,
    max_channels: Option<i32>,
    max_participants_per_channel: Option<i32>,
) -> Result<Option<AppRow>, sqlx::Error> {
    sqlx::query_as::<_, AppRow>(
        r#"UPDATE apps SET
               name = COALESCE($2, name),
               description = CASE WHEN $3 THEN $4 ELSE description END,
               max_channels = COALESCE($5, max_channels),
               max_participants_per_channel = COALESCE($6, max_participants_per_channel),
               updated_at = NOW()
           WHERE id = $1 AND active = true
           RETURNING *"#,
    )
    .bind(app_id)
    .bind(name)
    .bind(description.is_some())
    .bind(description.flatten())
    .bind(max_channels)
    .bind(max_participants_per_channel)
    .fetch_optional(pool)
    .await
}

pub async fn delete_app(pool: &DbPool, app_id: uuid::Uuid) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE apps SET active = false, updated_at = NOW() WHERE id = $1")
        .bind(app_id)
        .execute(pool)
        .await?;
    Ok(())
}

// ── User Queries ──

pub async fn upsert_user(pool: &DbPool, user: &UserRow) -> Result<UserRow, sqlx::Error> {
    sqlx::query_as::<_, UserRow>(
        r#"INSERT INTO users (id, app_id, external_id, display_name, metadata, is_banned, device_ids, total_session_minutes, created_at, updated_at)
           VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)
           ON CONFLICT (app_id, external_id) DO UPDATE SET
             display_name = EXCLUDED.display_name,
             metadata = COALESCE(EXCLUDED.metadata, users.metadata),
             updated_at = NOW()
           RETURNING *"#
    )
    .bind(user.id).bind(user.app_id).bind(&user.external_id).bind(&user.display_name)
    .bind(&user.metadata).bind(user.is_banned).bind(&user.device_ids)
    .bind(user.total_session_minutes).bind(user.created_at).bind(user.updated_at)
    .fetch_one(pool).await
}

pub async fn get_user(
    pool: &DbPool,
    app_id: Uuid,
    id: Uuid,
) -> Result<Option<UserRow>, sqlx::Error> {
    sqlx::query_as::<_, UserRow>("SELECT * FROM users WHERE app_id = $1 AND id = $2")
        .bind(app_id)
        .bind(id)
        .fetch_optional(pool)
        .await
}

pub async fn get_user_by_external_id(
    pool: &DbPool,
    app_id: Uuid,
    external_id: &str,
) -> Result<Option<UserRow>, sqlx::Error> {
    sqlx::query_as::<_, UserRow>("SELECT * FROM users WHERE app_id = $1 AND external_id = $2")
        .bind(app_id)
        .bind(external_id)
        .fetch_optional(pool)
        .await
}

pub async fn search_users(
    pool: &DbPool,
    app_id: Uuid,
    query: &str,
    limit: i64,
    offset: i64,
) -> Result<Vec<UserRow>, sqlx::Error> {
    let pattern = format!("%{}%", query);
    sqlx::query_as::<_, UserRow>(
        "SELECT * FROM users WHERE app_id = $1 AND (display_name ILIKE $2 OR external_id ILIKE $2) ORDER BY created_at DESC LIMIT $3 OFFSET $4"
    )
    .bind(app_id).bind(&pattern).bind(limit).bind(offset)
    .fetch_all(pool).await
}

pub async fn ban_user(
    pool: &DbPool,
    app_id: Uuid,
    user_id: Uuid,
    reason: &str,
    expires_at: Option<DateTime<Utc>>,
) -> Result<u64, sqlx::Error> {
    let r = sqlx::query("UPDATE users SET is_banned = true, ban_reason = $3, ban_expires_at = $4, updated_at = NOW() WHERE app_id = $1 AND id = $2")
        .bind(app_id).bind(user_id).bind(reason).bind(expires_at)
        .execute(pool).await?;
    Ok(r.rows_affected())
}

pub async fn unban_user(pool: &DbPool, app_id: Uuid, user_id: Uuid) -> Result<u64, sqlx::Error> {
    let r = sqlx::query("UPDATE users SET is_banned = false, ban_reason = NULL, ban_expires_at = NULL, updated_at = NOW() WHERE app_id = $1 AND id = $2")
        .bind(app_id).bind(user_id).execute(pool).await?;
    Ok(r.rows_affected())
}

/// Clear `is_banned` on users whose ban has expired. Returns number of rows updated.
pub async fn expire_user_bans(pool: &DbPool) -> Result<u64, sqlx::Error> {
    let r = sqlx::query("UPDATE users SET is_banned = false, ban_reason = NULL, ban_expires_at = NULL, updated_at = NOW() WHERE is_banned = true AND ban_expires_at IS NOT NULL AND ban_expires_at <= NOW()")
        .execute(pool).await?;
    Ok(r.rows_affected())
}

pub async fn add_user_session_minutes(
    pool: &DbPool,
    user_id: Uuid,
    minutes: i64,
) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE users SET total_session_minutes = total_session_minutes + $2, last_seen_at = NOW(), updated_at = NOW() WHERE id = $1")
        .bind(user_id).bind(minutes).execute(pool).await?;
    Ok(())
}

pub async fn update_user_last_seen(pool: &DbPool, user_id: Uuid) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE users SET last_seen_at = NOW(), updated_at = NOW() WHERE id = $1")
        .bind(user_id)
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn count_users(pool: &DbPool, app_id: Uuid) -> Result<i64, sqlx::Error> {
    let row: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM users WHERE app_id = $1")
        .bind(app_id)
        .fetch_one(pool)
        .await?;
    Ok(row.0)
}

// ── User erasure / export ──

/// Removes a user and everything keyed to them in one transaction: open and past sessions
/// with their memberships, stored chat (sent and received), cross-mute blocks in both
/// directions and recording rows (artifacts are the caller's job). Moderation events about the
/// user and their bans are kept unless `purge_moderation`; reports they filed are anonymised.
/// A tombstone is written so tokens minted before the deletion cannot re-create the user.
/// Returns `None` when the user does not exist in `app_id`.
pub async fn erase_user(
    pool: &DbPool,
    app_id: Uuid,
    user_id: Uuid,
    purge_moderation: bool,
) -> Result<Option<UserErasureCounts>, sqlx::Error> {
    let mut tx = pool.begin().await?;
    let exists: Option<(Uuid,)> =
        sqlx::query_as("SELECT id FROM users WHERE app_id = $1 AND id = $2 FOR UPDATE")
            .bind(app_id)
            .bind(user_id)
            .fetch_optional(&mut *tx)
            .await?;
    if exists.is_none() {
        tx.rollback().await?;
        return Ok(None);
    }
    let channel_memberships = delete_user_rows(
        &mut tx,
        "DELETE FROM channel_memberships cm USING channels c \
         WHERE cm.channel_id = c.id AND c.app_id = $1 AND cm.user_id = $2",
        app_id,
        user_id,
    )
    .await?;
    let sessions = delete_user_rows(
        &mut tx,
        "DELETE FROM sessions WHERE app_id = $1 AND user_id = $2",
        app_id,
        user_id,
    )
    .await?;
    let chat_messages = delete_user_rows(
        &mut tx,
        "DELETE FROM chat_messages WHERE app_id = $1 AND (from_user_id = $2 OR to_user_id = $2)",
        app_id,
        user_id,
    )
    .await?;
    let user_blocks = delete_user_rows(
        &mut tx,
        "DELETE FROM user_blocks WHERE app_id = $1 AND (user_id = $2 OR blocked_user_id = $2)",
        app_id,
        user_id,
    )
    .await?;
    let recordings = delete_user_rows(
        &mut tx,
        "DELETE FROM recordings WHERE app_id = $1 AND user_id = $2",
        app_id,
        user_id,
    )
    .await?;
    sqlx::query(
        "UPDATE moderation_events SET reporter_user_id = NULL WHERE app_id = $1 AND reporter_user_id = $2",
    )
    .bind(app_id)
    .bind(user_id)
    .execute(&mut *tx)
    .await?;
    let (moderation_events, bans) = if purge_moderation {
        (
            delete_user_rows(
                &mut tx,
                "DELETE FROM moderation_events WHERE app_id = $1 AND target_user_id = $2",
                app_id,
                user_id,
            )
            .await?,
            delete_user_rows(
                &mut tx,
                "DELETE FROM bans WHERE app_id = $1 AND user_id = $2",
                app_id,
                user_id,
            )
            .await?,
        )
    } else {
        (0, 0)
    };
    sqlx::query(
        r#"INSERT INTO user_tombstones (app_id, user_id, deleted_at) VALUES ($1, $2, NOW())
           ON CONFLICT (app_id, user_id) DO UPDATE SET deleted_at = NOW()"#,
    )
    .bind(app_id)
    .bind(user_id)
    .execute(&mut *tx)
    .await?;
    let users = delete_user_rows(
        &mut tx,
        "DELETE FROM users WHERE app_id = $1 AND id = $2",
        app_id,
        user_id,
    )
    .await?;
    tx.commit().await?;
    Ok(Some(UserErasureCounts {
        channel_memberships,
        sessions,
        chat_messages,
        user_blocks,
        recordings,
        moderation_events,
        bans,
        users,
    }))
}

async fn delete_user_rows(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    sql: &str,
    app_id: Uuid,
    user_id: Uuid,
) -> Result<u64, sqlx::Error> {
    Ok(sqlx::query(sql)
        .bind(app_id)
        .bind(user_id)
        .execute(&mut **tx)
        .await?
        .rows_affected())
}

/// When `user_id` was erased from `app_id` (most recent erasure), if ever.
pub async fn get_user_tombstone(
    pool: &DbPool,
    app_id: Uuid,
    user_id: Uuid,
) -> Result<Option<DateTime<Utc>>, sqlx::Error> {
    sqlx::query_scalar::<_, DateTime<Utc>>(
        "SELECT deleted_at FROM user_tombstones WHERE app_id = $1 AND user_id = $2",
    )
    .bind(app_id)
    .bind(user_id)
    .fetch_optional(pool)
    .await
}

pub async fn delete_tombstones_before(
    pool: &DbPool,
    cutoff: DateTime<Utc>,
) -> Result<u64, sqlx::Error> {
    let r = sqlx::query("DELETE FROM user_tombstones WHERE deleted_at < $1")
        .bind(cutoff)
        .execute(pool)
        .await?;
    Ok(r.rows_affected())
}

pub async fn export_user_sessions(
    pool: &DbPool,
    app_id: Uuid,
    user_id: Uuid,
    limit: i64,
) -> Result<Vec<SessionRow>, sqlx::Error> {
    sqlx::query_as::<_, SessionRow>(
        "SELECT * FROM sessions WHERE app_id = $1 AND user_id = $2 ORDER BY connected_at DESC LIMIT $3",
    )
    .bind(app_id).bind(user_id).bind(limit)
    .fetch_all(pool).await
}

pub async fn export_user_memberships(
    pool: &DbPool,
    user_id: Uuid,
    limit: i64,
) -> Result<Vec<ChannelMembershipRow>, sqlx::Error> {
    sqlx::query_as::<_, ChannelMembershipRow>(
        "SELECT * FROM channel_memberships WHERE user_id = $1 ORDER BY joined_at DESC LIMIT $2",
    )
    .bind(user_id)
    .bind(limit)
    .fetch_all(pool)
    .await
}

pub async fn export_user_chat_messages(
    pool: &DbPool,
    app_id: Uuid,
    user_id: Uuid,
    limit: i64,
) -> Result<Vec<ChatMessageRow>, sqlx::Error> {
    sqlx::query_as::<_, ChatMessageRow>(
        r#"SELECT * FROM chat_messages WHERE app_id = $1 AND (from_user_id = $2 OR to_user_id = $2)
           ORDER BY sent_at DESC LIMIT $3"#,
    )
    .bind(app_id)
    .bind(user_id)
    .bind(limit)
    .fetch_all(pool)
    .await
}

pub async fn export_user_bans(
    pool: &DbPool,
    app_id: Uuid,
    user_id: Uuid,
    limit: i64,
) -> Result<Vec<BanRow>, sqlx::Error> {
    sqlx::query_as::<_, BanRow>(
        "SELECT * FROM bans WHERE app_id = $1 AND user_id = $2 ORDER BY created_at DESC LIMIT $3",
    )
    .bind(app_id)
    .bind(user_id)
    .bind(limit)
    .fetch_all(pool)
    .await
}

pub async fn export_moderation_events_about(
    pool: &DbPool,
    app_id: Uuid,
    user_id: Uuid,
    limit: i64,
) -> Result<Vec<ModerationEventRow>, sqlx::Error> {
    sqlx::query_as::<_, ModerationEventRow>(
        "SELECT * FROM moderation_events WHERE app_id = $1 AND target_user_id = $2 ORDER BY created_at DESC LIMIT $3",
    )
    .bind(app_id).bind(user_id).bind(limit)
    .fetch_all(pool).await
}

pub async fn export_moderation_events_reported_by(
    pool: &DbPool,
    app_id: Uuid,
    user_id: Uuid,
    limit: i64,
) -> Result<Vec<ModerationEventRow>, sqlx::Error> {
    sqlx::query_as::<_, ModerationEventRow>(
        "SELECT * FROM moderation_events WHERE app_id = $1 AND reporter_user_id = $2 ORDER BY created_at DESC LIMIT $3",
    )
    .bind(app_id).bind(user_id).bind(limit)
    .fetch_all(pool).await
}

pub async fn list_recordings_for_user(
    pool: &DbPool,
    app_id: Uuid,
    user_id: Uuid,
    limit: i64,
) -> Result<Vec<RecordingRow>, sqlx::Error> {
    sqlx::query_as::<_, RecordingRow>(
        "SELECT * FROM recordings WHERE app_id = $1 AND user_id = $2 ORDER BY started_at DESC LIMIT $3",
    )
    .bind(app_id).bind(user_id).bind(limit)
    .fetch_all(pool).await
}

// ── Retention sweeps (batched; each call removes at most `batch` parent rows) ──

/// Session-level advisory lock so a cluster-wide job runs on one node at a time. The lock is
/// tied to `conn`; drop the connection (or call [`advisory_unlock`]) to release it.
pub async fn try_advisory_lock(
    conn: &mut sqlx::PgConnection,
    key: i64,
) -> Result<bool, sqlx::Error> {
    sqlx::query_scalar::<_, bool>("SELECT pg_try_advisory_lock($1)")
        .bind(key)
        .fetch_one(conn)
        .await
}

pub async fn advisory_unlock(conn: &mut sqlx::PgConnection, key: i64) -> Result<(), sqlx::Error> {
    sqlx::query("SELECT pg_advisory_unlock($1)")
        .bind(key)
        .execute(conn)
        .await?;
    Ok(())
}

/// Closed sessions older than `cutoff`, together with their membership rows.
pub async fn delete_closed_sessions_before(
    pool: &DbPool,
    cutoff: DateTime<Utc>,
    batch: i64,
) -> Result<u64, sqlx::Error> {
    let r = sqlx::query(
        r#"WITH victims AS (
               SELECT id FROM sessions WHERE disconnected_at IS NOT NULL AND disconnected_at < $1 LIMIT $2
           ),
           members AS (
               DELETE FROM channel_memberships WHERE session_id IN (SELECT id FROM victims)
           )
           DELETE FROM sessions WHERE id IN (SELECT id FROM victims)"#,
    )
    .bind(cutoff)
    .bind(batch)
    .execute(pool)
    .await?;
    Ok(r.rows_affected())
}

pub async fn delete_resolved_moderation_events_before(
    pool: &DbPool,
    cutoff: DateTime<Utc>,
    batch: i64,
) -> Result<u64, sqlx::Error> {
    let r = sqlx::query(
        r#"DELETE FROM moderation_events WHERE id IN (
               SELECT id FROM moderation_events WHERE resolved_at IS NOT NULL AND resolved_at < $1 LIMIT $2
           )"#,
    )
    .bind(cutoff)
    .bind(batch)
    .execute(pool)
    .await?;
    Ok(r.rows_affected())
}

pub async fn delete_audit_log_before(
    pool: &DbPool,
    cutoff: DateTime<Utc>,
    batch: i64,
) -> Result<u64, sqlx::Error> {
    let r = sqlx::query(
        "DELETE FROM audit_log WHERE id IN (SELECT id FROM audit_log WHERE created_at < $1 LIMIT $2)",
    )
    .bind(cutoff)
    .bind(batch)
    .execute(pool)
    .await?;
    Ok(r.rows_affected())
}

pub async fn delete_analytics_before(
    pool: &DbPool,
    cutoff: DateTime<Utc>,
    batch: i64,
) -> Result<u64, sqlx::Error> {
    let r = sqlx::query(
        r#"DELETE FROM analytics_snapshots WHERE id IN (
               SELECT id FROM analytics_snapshots WHERE timestamp < $1 LIMIT $2
           )"#,
    )
    .bind(cutoff)
    .bind(batch)
    .execute(pool)
    .await?;
    Ok(r.rows_affected())
}

/// Users not seen since `cutoff` (never-seen users count from creation) with no open session
/// and no standing ban, as `(app_id, user_id)`.
pub async fn list_inactive_users(
    pool: &DbPool,
    cutoff: DateTime<Utc>,
    limit: i64,
) -> Result<Vec<(Uuid, Uuid)>, sqlx::Error> {
    sqlx::query_as::<_, (Uuid, Uuid)>(
        r#"SELECT u.app_id, u.id FROM users u
           WHERE COALESCE(u.last_seen_at, u.created_at) < $1
             AND NOT u.is_banned
             AND NOT EXISTS (SELECT 1 FROM sessions s WHERE s.user_id = u.id AND s.disconnected_at IS NULL)
           ORDER BY COALESCE(u.last_seen_at, u.created_at)
           LIMIT $2"#,
    )
    .bind(cutoff)
    .bind(limit)
    .fetch_all(pool)
    .await
}

// ── Channel Queries ──

pub async fn create_channel(pool: &DbPool, ch: &ChannelRow) -> Result<ChannelRow, sqlx::Error> {
    sqlx::query_as::<_, ChannelRow>(
        r#"INSERT INTO channels (id, app_id, name, channel_type, config, max_participants, is_persistent, ad_hoc, active_participants, created_at, updated_at)
           VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11)
           RETURNING *"#
    )
    .bind(ch.id).bind(ch.app_id).bind(&ch.name).bind(&ch.channel_type)
    .bind(&ch.config).bind(ch.max_participants).bind(ch.is_persistent).bind(ch.ad_hoc)
    .bind(ch.active_participants).bind(ch.created_at).bind(ch.updated_at)
    .fetch_one(pool).await
}

/// Creates an ad-hoc channel or revives a soft-deleted one with the same derived id. Returns
/// `(row, created)`; `created` is false when a live row already existed (concurrent joiners).
/// A live non-ad-hoc row with that id is left untouched and reported as not created.
pub async fn ensure_ad_hoc_channel(
    pool: &DbPool,
    ch: &ChannelRow,
) -> Result<(ChannelRow, bool), sqlx::Error> {
    let mut tx = pool.begin().await?;
    let existing = sqlx::query_as::<_, ChannelRow>(
        "SELECT * FROM channels WHERE id = $1 AND app_id = $2 FOR UPDATE",
    )
    .bind(ch.id)
    .bind(ch.app_id)
    .fetch_optional(&mut *tx)
    .await?;
    let (row, created) = match existing {
        Some(row) if row.deleted_at.is_none() => (row, false),
        Some(_) => {
            let row = sqlx::query_as::<_, ChannelRow>(
                r#"UPDATE channels
                   SET name = $3, channel_type = $4, config = $5, max_participants = $6,
                       ad_hoc = true, deleted_at = NULL, updated_at = NOW(),
                       active_participants = (SELECT COUNT(*) FROM channel_memberships m
                                              WHERE m.channel_id = channels.id AND m.left_at IS NULL)
                   WHERE id = $1 AND app_id = $2
                   RETURNING *"#,
            )
            .bind(ch.id)
            .bind(ch.app_id)
            .bind(&ch.name)
            .bind(&ch.channel_type)
            .bind(&ch.config)
            .bind(ch.max_participants)
            .fetch_one(&mut *tx)
            .await?;
            (row, true)
        }
        None => {
            let row = sqlx::query_as::<_, ChannelRow>(
                r#"INSERT INTO channels (id, app_id, name, channel_type, config, max_participants, is_persistent, ad_hoc, active_participants, created_at, updated_at)
                   VALUES ($1, $2, $3, $4, $5, $6, false, true, 0, NOW(), NOW())
                   RETURNING *"#,
            )
            .bind(ch.id)
            .bind(ch.app_id)
            .bind(&ch.name)
            .bind(&ch.channel_type)
            .bind(&ch.config)
            .bind(ch.max_participants)
            .fetch_one(&mut *tx)
            .await?;
            (row, true)
        }
    };
    tx.commit().await?;
    Ok((row, created))
}

/// Soft-deletes an ad-hoc channel that has no active participants. Returns true when the row
/// was deleted by this call (so exactly one node announces `channel.destroyed`).
pub async fn delete_empty_ad_hoc_channel(
    pool: &DbPool,
    app_id: Uuid,
    channel_id: Uuid,
) -> Result<bool, sqlx::Error> {
    let r = sqlx::query(
        r#"UPDATE channels SET deleted_at = NOW(), updated_at = NOW()
           WHERE app_id = $1 AND id = $2 AND ad_hoc AND deleted_at IS NULL
             AND active_participants <= 0
             AND NOT EXISTS (SELECT 1 FROM channel_memberships m WHERE m.channel_id = channels.id AND m.left_at IS NULL)"#,
    )
    .bind(app_id)
    .bind(channel_id)
    .execute(pool)
    .await?;
    Ok(r.rows_affected() > 0)
}

/// Undoes a soft delete of an ad-hoc channel that a participant entered concurrently (the
/// release saw an empty channel just before the join was persisted). Returns true when the row
/// was revived by this call.
pub async fn revive_ad_hoc_channel(
    pool: &DbPool,
    app_id: Uuid,
    channel_id: Uuid,
) -> Result<bool, sqlx::Error> {
    let r = sqlx::query(
        r#"UPDATE channels SET deleted_at = NULL, updated_at = NOW()
           WHERE app_id = $1 AND id = $2 AND ad_hoc AND deleted_at IS NOT NULL"#,
    )
    .bind(app_id)
    .bind(channel_id)
    .execute(pool)
    .await?;
    Ok(r.rows_affected() > 0)
}

pub async fn get_channel(
    pool: &DbPool,
    app_id: Uuid,
    id: Uuid,
) -> Result<Option<ChannelRow>, sqlx::Error> {
    sqlx::query_as::<_, ChannelRow>(
        "SELECT * FROM channels WHERE app_id = $1 AND id = $2 AND deleted_at IS NULL",
    )
    .bind(app_id)
    .bind(id)
    .fetch_optional(pool)
    .await
}

/// Cross-tenant lookup for internal (non-API) use only, e.g. resolving a channel row from a
/// media-plane identifier. Callers MUST compare `app_id` before acting on the result.
pub async fn get_channel_any_app(
    pool: &DbPool,
    id: Uuid,
) -> Result<Option<ChannelRow>, sqlx::Error> {
    sqlx::query_as::<_, ChannelRow>("SELECT * FROM channels WHERE id = $1 AND deleted_at IS NULL")
        .bind(id)
        .fetch_optional(pool)
        .await
}

pub async fn list_channels(
    pool: &DbPool,
    app_id: Uuid,
    limit: i64,
    offset: i64,
) -> Result<Vec<ChannelRow>, sqlx::Error> {
    sqlx::query_as::<_, ChannelRow>(
        "SELECT * FROM channels WHERE app_id = $1 AND deleted_at IS NULL ORDER BY created_at DESC LIMIT $2 OFFSET $3"
    )
    .bind(app_id).bind(limit).bind(offset)
    .fetch_all(pool).await
}

pub async fn list_active_channels(
    pool: &DbPool,
    app_id: Uuid,
    limit: i64,
    offset: i64,
) -> Result<Vec<ChannelRow>, sqlx::Error> {
    sqlx::query_as::<_, ChannelRow>(
        "SELECT * FROM channels WHERE app_id = $1 AND deleted_at IS NULL AND active_participants > 0 ORDER BY active_participants DESC LIMIT $2 OFFSET $3"
    )
    .bind(app_id).bind(limit).bind(offset)
    .fetch_all(pool).await
}

/// Adjusts the live counter and returns the new value, so callers can detect the
/// `0 → 1` (activated) and `1 → 0` (deactivated) edges exactly once under the row lock.
pub async fn update_channel_participant_count(
    pool: &DbPool,
    channel_id: Uuid,
    delta: i32,
) -> Result<i32, sqlx::Error> {
    let count: Option<i32> = sqlx::query_scalar(
        "UPDATE channels SET active_participants = GREATEST(0, active_participants + $2), updated_at = NOW() WHERE id = $1 RETURNING active_participants",
    )
        .bind(channel_id).bind(delta)
        .fetch_optional(pool).await?;
    Ok(count.unwrap_or(0))
}

/// Re-derives `active_participants` from open memberships for the given channels; run after
/// stale memberships of a crashed node were closed so the counter does not drift.
/// Recomputes `active_participants` for `channel_ids` and returns `(app_id, channel_id)` of
/// the channels that ended up empty.
pub async fn recount_channel_participants(
    pool: &DbPool,
    channel_ids: &[Uuid],
) -> Result<Vec<(Uuid, Uuid)>, sqlx::Error> {
    if channel_ids.is_empty() {
        return Ok(Vec::new());
    }
    let rows = sqlx::query_as::<_, (Uuid, Uuid, i32)>(
        r#"UPDATE channels c SET active_participants = (
               SELECT COUNT(*) FROM channel_memberships m WHERE m.channel_id = c.id AND m.left_at IS NULL
           ), updated_at = NOW()
           WHERE c.id = ANY($1)
           RETURNING c.app_id, c.id, c.active_participants"#,
    )
    .bind(channel_ids)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .filter(|(_, _, n)| *n == 0)
        .map(|(app, id, _)| (app, id))
        .collect())
}

pub async fn delete_channel(
    pool: &DbPool,
    app_id: Uuid,
    channel_id: Uuid,
) -> Result<u64, sqlx::Error> {
    let r = sqlx::query("UPDATE channels SET deleted_at = NOW(), updated_at = NOW() WHERE app_id = $1 AND id = $2 AND deleted_at IS NULL")
        .bind(app_id).bind(channel_id).execute(pool).await?;
    Ok(r.rows_affected())
}

pub async fn update_channel_config(
    pool: &DbPool,
    app_id: Uuid,
    channel_id: Uuid,
    config: &serde_json::Value,
    max_participants: i32,
) -> Result<u64, sqlx::Error> {
    let r = sqlx::query("UPDATE channels SET config = $3, max_participants = $4, updated_at = NOW() WHERE app_id = $1 AND id = $2 AND deleted_at IS NULL")
        .bind(app_id).bind(channel_id).bind(config).bind(max_participants).execute(pool).await?;
    Ok(r.rows_affected())
}

pub async fn count_channels(pool: &DbPool, app_id: Uuid) -> Result<i64, sqlx::Error> {
    let row: (i64,) =
        sqlx::query_as("SELECT COUNT(*) FROM channels WHERE app_id = $1 AND deleted_at IS NULL")
            .bind(app_id)
            .fetch_one(pool)
            .await?;
    Ok(row.0)
}

pub async fn count_active_channels(pool: &DbPool, app_id: Uuid) -> Result<i64, sqlx::Error> {
    let row: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM channels WHERE app_id = $1 AND deleted_at IS NULL AND active_participants > 0")
        .bind(app_id).fetch_one(pool).await?;
    Ok(row.0)
}

// ── Session Queries ──

pub async fn create_session(pool: &DbPool, s: &SessionRow) -> Result<SessionRow, sqlx::Error> {
    sqlx::query_as::<_, SessionRow>(
        r#"INSERT INTO sessions (id, user_id, app_id, media_node_id, ip_address, user_agent, connected_at)
           VALUES ($1, $2, $3, $4, $5, $6, $7) RETURNING *"#
    )
    .bind(s.id).bind(s.user_id).bind(s.app_id).bind(s.media_node_id)
    .bind(&s.ip_address).bind(&s.user_agent).bind(s.connected_at)
    .fetch_one(pool).await
}

pub async fn close_session(
    pool: &DbPool,
    session_id: Uuid,
    reason: &str,
    quality: Option<serde_json::Value>,
) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE sessions SET disconnected_at = NOW(), disconnect_reason = $2, quality_stats = $3 WHERE id = $1")
        .bind(session_id).bind(reason).bind(quality)
        .execute(pool).await?;
    Ok(())
}

pub async fn get_session(
    pool: &DbPool,
    app_id: Uuid,
    id: Uuid,
) -> Result<Option<SessionRow>, sqlx::Error> {
    sqlx::query_as::<_, SessionRow>("SELECT * FROM sessions WHERE app_id = $1 AND id = $2")
        .bind(app_id)
        .bind(id)
        .fetch_optional(pool)
        .await
}

/// Close every session that is still marked open for a media node (used on node startup
/// so that a crash does not leave phantom active sessions).
pub async fn close_stale_sessions_for_node(
    pool: &DbPool,
    media_node_id: Uuid,
    reason: &str,
) -> Result<u64, sqlx::Error> {
    let r = sqlx::query("UPDATE sessions SET disconnected_at = NOW(), disconnect_reason = $2 WHERE media_node_id = $1 AND disconnected_at IS NULL")
        .bind(media_node_id).bind(reason).execute(pool).await?;
    Ok(r.rows_affected())
}

/// Closes memberships left open by a previous crash of this node; returns the channel of every
/// closed membership so the participant counters can be recomputed.
pub async fn close_stale_memberships_for_node(
    pool: &DbPool,
    media_node_id: Uuid,
) -> Result<Vec<Uuid>, sqlx::Error> {
    sqlx::query_scalar::<_, Uuid>(
        "UPDATE channel_memberships SET left_at = NOW() WHERE left_at IS NULL AND session_id IN (SELECT id FROM sessions WHERE media_node_id = $1) RETURNING channel_id"
    ).bind(media_node_id).fetch_all(pool).await
}

pub async fn get_active_sessions_for_user(
    pool: &DbPool,
    user_id: Uuid,
) -> Result<Vec<SessionRow>, sqlx::Error> {
    sqlx::query_as::<_, SessionRow>(
        "SELECT * FROM sessions WHERE user_id = $1 AND disconnected_at IS NULL",
    )
    .bind(user_id)
    .fetch_all(pool)
    .await
}

pub async fn count_active_sessions(pool: &DbPool, app_id: Uuid) -> Result<i64, sqlx::Error> {
    let row: (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM sessions WHERE app_id = $1 AND disconnected_at IS NULL",
    )
    .bind(app_id)
    .fetch_one(pool)
    .await?;
    Ok(row.0)
}

// ── Channel Membership Queries ──

pub async fn add_channel_member(
    pool: &DbPool,
    m: &ChannelMembershipRow,
) -> Result<ChannelMembershipRow, sqlx::Error> {
    sqlx::query_as::<_, ChannelMembershipRow>(
        r#"INSERT INTO channel_memberships (id, channel_id, user_id, session_id, role, is_muted, is_server_muted, ssrc, joined_at)
           VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9) RETURNING *"#
    )
    .bind(m.id).bind(m.channel_id).bind(m.user_id).bind(m.session_id)
    .bind(&m.role).bind(m.is_muted).bind(m.is_server_muted)
    .bind(m.ssrc).bind(m.joined_at)
    .fetch_one(pool).await
}

pub async fn remove_channel_member(
    pool: &DbPool,
    channel_id: Uuid,
    session_id: Uuid,
) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE channel_memberships SET left_at = NOW() WHERE channel_id = $1 AND session_id = $2 AND left_at IS NULL")
        .bind(channel_id).bind(session_id)
        .execute(pool).await?;
    Ok(())
}

/// Close every open membership of `user_id` in a channel (kick). Tenant-checked via channels.
pub async fn remove_user_channel_memberships(
    pool: &DbPool,
    app_id: Uuid,
    channel_id: Uuid,
    user_id: Uuid,
) -> Result<u64, sqlx::Error> {
    let r = sqlx::query(
        r#"UPDATE channel_memberships m SET left_at = NOW()
           FROM channels c
           WHERE c.id = m.channel_id AND c.app_id = $1 AND m.channel_id = $2 AND m.user_id = $3 AND m.left_at IS NULL"#
    )
    .bind(app_id).bind(channel_id).bind(user_id)
    .execute(pool).await?;
    Ok(r.rows_affected())
}

pub async fn close_session_memberships(
    pool: &DbPool,
    session_id: Uuid,
) -> Result<u64, sqlx::Error> {
    let r = sqlx::query(
        "UPDATE channel_memberships SET left_at = NOW() WHERE session_id = $1 AND left_at IS NULL",
    )
    .bind(session_id)
    .execute(pool)
    .await?;
    Ok(r.rows_affected())
}

/// Active members of a channel, verified to belong to `app_id` via the channels table.
pub async fn get_channel_members(
    pool: &DbPool,
    app_id: Uuid,
    channel_id: Uuid,
) -> Result<Vec<ChannelMembershipRow>, sqlx::Error> {
    sqlx::query_as::<_, ChannelMembershipRow>(
        r#"SELECT m.* FROM channel_memberships m
           JOIN channels c ON c.id = m.channel_id
           WHERE c.app_id = $1 AND m.channel_id = $2 AND m.left_at IS NULL"#,
    )
    .bind(app_id)
    .bind(channel_id)
    .fetch_all(pool)
    .await
}

/// Active members of a channel with their display name and the node hosting their session
/// (for the roster a joining participant receives, including members on other nodes).
pub async fn get_channel_roster(
    pool: &DbPool,
    app_id: Uuid,
    channel_id: Uuid,
) -> Result<Vec<ChannelRosterRow>, sqlx::Error> {
    sqlx::query_as::<_, ChannelRosterRow>(
        r#"SELECT m.user_id, m.session_id, s.media_node_id, u.display_name, m.role,
                  m.is_muted, m.is_server_muted, m.ssrc
           FROM channel_memberships m
           JOIN channels c ON c.id = m.channel_id
           JOIN sessions s ON s.id = m.session_id
           JOIN users u ON u.id = m.user_id
           WHERE c.app_id = $1 AND m.channel_id = $2 AND m.left_at IS NULL
             AND s.disconnected_at IS NULL"#,
    )
    .bind(app_id)
    .bind(channel_id)
    .fetch_all(pool)
    .await
}

pub async fn set_server_mute(
    pool: &DbPool,
    app_id: Uuid,
    channel_id: Uuid,
    user_id: Uuid,
    muted: bool,
) -> Result<u64, sqlx::Error> {
    let r = sqlx::query(
        r#"UPDATE channel_memberships m SET is_server_muted = $4
           FROM channels c
           WHERE c.id = m.channel_id AND c.app_id = $1 AND m.channel_id = $2 AND m.user_id = $3 AND m.left_at IS NULL"#
    )
    .bind(app_id).bind(channel_id).bind(user_id).bind(muted)
    .execute(pool).await?;
    Ok(r.rows_affected())
}

pub async fn get_user_channels(
    pool: &DbPool,
    user_id: Uuid,
) -> Result<Vec<ChannelMembershipRow>, sqlx::Error> {
    sqlx::query_as::<_, ChannelMembershipRow>(
        "SELECT * FROM channel_memberships WHERE user_id = $1 AND left_at IS NULL",
    )
    .bind(user_id)
    .fetch_all(pool)
    .await
}

// ── User block (cross-mute) Queries ──

/// Adds a block; returns `false` if it already existed.
pub async fn add_user_block(
    pool: &DbPool,
    app_id: Uuid,
    user_id: Uuid,
    blocked_user_id: Uuid,
) -> Result<bool, sqlx::Error> {
    let r = sqlx::query(
        "INSERT INTO user_blocks (app_id, user_id, blocked_user_id) VALUES ($1, $2, $3) ON CONFLICT DO NOTHING",
    )
    .bind(app_id).bind(user_id).bind(blocked_user_id)
    .execute(pool).await?;
    Ok(r.rows_affected() > 0)
}

pub async fn remove_user_block(
    pool: &DbPool,
    app_id: Uuid,
    user_id: Uuid,
    blocked_user_id: Uuid,
) -> Result<bool, sqlx::Error> {
    let r = sqlx::query(
        "DELETE FROM user_blocks WHERE app_id = $1 AND user_id = $2 AND blocked_user_id = $3",
    )
    .bind(app_id)
    .bind(user_id)
    .bind(blocked_user_id)
    .execute(pool)
    .await?;
    Ok(r.rows_affected() > 0)
}

/// Removes every block `user_id` placed; returns the users that were blocked.
pub async fn clear_user_blocks(
    pool: &DbPool,
    app_id: Uuid,
    user_id: Uuid,
) -> Result<Vec<Uuid>, sqlx::Error> {
    sqlx::query_scalar::<_, Uuid>(
        "DELETE FROM user_blocks WHERE app_id = $1 AND user_id = $2 RETURNING blocked_user_id",
    )
    .bind(app_id)
    .bind(user_id)
    .fetch_all(pool)
    .await
}

/// Users blocked by `user_id`.
pub async fn list_user_blocks(
    pool: &DbPool,
    app_id: Uuid,
    user_id: Uuid,
) -> Result<Vec<Uuid>, sqlx::Error> {
    sqlx::query_scalar::<_, Uuid>(
        "SELECT blocked_user_id FROM user_blocks WHERE app_id = $1 AND user_id = $2 ORDER BY created_at",
    )
    .bind(app_id).bind(user_id)
    .fetch_all(pool).await
}

/// Users who blocked `user_id`.
pub async fn list_user_blocked_by(
    pool: &DbPool,
    app_id: Uuid,
    user_id: Uuid,
) -> Result<Vec<Uuid>, sqlx::Error> {
    sqlx::query_scalar::<_, Uuid>(
        "SELECT user_id FROM user_blocks WHERE app_id = $1 AND blocked_user_id = $2",
    )
    .bind(app_id)
    .bind(user_id)
    .fetch_all(pool)
    .await
}

pub async fn count_user_blocks(
    pool: &DbPool,
    app_id: Uuid,
    user_id: Uuid,
) -> Result<i64, sqlx::Error> {
    sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*) FROM user_blocks WHERE app_id = $1 AND user_id = $2",
    )
    .bind(app_id)
    .bind(user_id)
    .fetch_one(pool)
    .await
}

// ── Ban Queries ──

pub async fn create_ban(pool: &DbPool, ban: &BanRow) -> Result<BanRow, sqlx::Error> {
    sqlx::query_as::<_, BanRow>(
        r#"INSERT INTO bans (id, app_id, user_id, device_id, ip_address, scope, reason, issued_by, expires_at, created_at)
           VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10) RETURNING *"#
    )
    .bind(ban.id).bind(ban.app_id).bind(ban.user_id).bind(&ban.device_id)
    .bind(&ban.ip_address).bind(&ban.scope).bind(&ban.reason)
    .bind(ban.issued_by).bind(ban.expires_at).bind(ban.created_at)
    .fetch_one(pool).await
}

pub async fn get_active_bans_for_user(
    pool: &DbPool,
    app_id: Uuid,
    user_id: Uuid,
) -> Result<Vec<BanRow>, sqlx::Error> {
    sqlx::query_as::<_, BanRow>(
        "SELECT * FROM bans WHERE app_id = $1 AND user_id = $2 AND revoked_at IS NULL AND (expires_at IS NULL OR expires_at > NOW())"
    )
    .bind(app_id).bind(user_id).fetch_all(pool).await
}

pub async fn get_ban(
    pool: &DbPool,
    app_id: Uuid,
    ban_id: Uuid,
) -> Result<Option<BanRow>, sqlx::Error> {
    sqlx::query_as::<_, BanRow>("SELECT * FROM bans WHERE app_id = $1 AND id = $2")
        .bind(app_id)
        .bind(ban_id)
        .fetch_optional(pool)
        .await
}

pub async fn list_active_bans(
    pool: &DbPool,
    app_id: Uuid,
    limit: i64,
    offset: i64,
) -> Result<Vec<BanRow>, sqlx::Error> {
    sqlx::query_as::<_, BanRow>(
        "SELECT * FROM bans WHERE app_id = $1 AND revoked_at IS NULL AND (expires_at IS NULL OR expires_at > NOW()) ORDER BY created_at DESC LIMIT $2 OFFSET $3"
    )
    .bind(app_id).bind(limit).bind(offset).fetch_all(pool).await
}

pub async fn list_bans(
    pool: &DbPool,
    app_id: Uuid,
    user_id: Option<Uuid>,
    limit: i64,
    offset: i64,
) -> Result<Vec<BanRow>, sqlx::Error> {
    sqlx::query_as::<_, BanRow>(
        "SELECT * FROM bans WHERE app_id = $1 AND ($2::uuid IS NULL OR user_id = $2) ORDER BY created_at DESC LIMIT $3 OFFSET $4"
    )
    .bind(app_id).bind(user_id).bind(limit).bind(offset).fetch_all(pool).await
}

pub async fn revoke_ban(
    pool: &DbPool,
    app_id: Uuid,
    ban_id: Uuid,
    revoked_by: Uuid,
) -> Result<u64, sqlx::Error> {
    let r = sqlx::query("UPDATE bans SET revoked_at = NOW(), revoked_by = $3 WHERE app_id = $1 AND id = $2 AND revoked_at IS NULL")
        .bind(app_id).bind(ban_id).bind(revoked_by).execute(pool).await?;
    Ok(r.rows_affected())
}

// ── Moderation Event Queries ──

pub async fn create_moderation_event(
    pool: &DbPool,
    ev: &ModerationEventRow,
) -> Result<ModerationEventRow, sqlx::Error> {
    sqlx::query_as::<_, ModerationEventRow>(
        r#"INSERT INTO moderation_events (id, app_id, channel_id, target_user_id, reporter_user_id, moderator_user_id, event_type, reason, evidence, recording_id, status, created_at)
           VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12) RETURNING *"#
    )
    .bind(ev.id).bind(ev.app_id).bind(ev.channel_id).bind(ev.target_user_id)
    .bind(ev.reporter_user_id).bind(ev.moderator_user_id)
    .bind(&ev.event_type).bind(&ev.reason).bind(&ev.evidence)
    .bind(ev.recording_id).bind(&ev.status).bind(ev.created_at)
    .fetch_one(pool).await
}

pub async fn list_moderation_events(
    pool: &DbPool,
    app_id: Uuid,
    status: Option<&str>,
    limit: i64,
    offset: i64,
) -> Result<Vec<ModerationEventRow>, sqlx::Error> {
    match status {
        Some(s) => {
            sqlx::query_as::<_, ModerationEventRow>(
                "SELECT * FROM moderation_events WHERE app_id = $1 AND status = $2 ORDER BY created_at DESC LIMIT $3 OFFSET $4"
            ).bind(app_id).bind(s).bind(limit).bind(offset).fetch_all(pool).await
        }
        None => {
            sqlx::query_as::<_, ModerationEventRow>(
                "SELECT * FROM moderation_events WHERE app_id = $1 ORDER BY created_at DESC LIMIT $2 OFFSET $3"
            ).bind(app_id).bind(limit).bind(offset).fetch_all(pool).await
        }
    }
}

/// Incidents raised by the safety pipeline (`event_type` `safety.voice` / `safety.text`),
/// newest first, optionally narrowed to one user, one source or one status.
pub async fn list_safety_incidents(
    pool: &DbPool,
    app_id: Uuid,
    user_id: Option<Uuid>,
    event_type: Option<&str>,
    status: Option<&str>,
    limit: i64,
    offset: i64,
) -> Result<Vec<ModerationEventRow>, sqlx::Error> {
    sqlx::query_as::<_, ModerationEventRow>(
        r#"SELECT * FROM moderation_events
           WHERE app_id = $1 AND event_type LIKE 'safety.%'
             AND ($2::uuid IS NULL OR target_user_id = $2)
             AND ($3::text IS NULL OR event_type = $3)
             AND ($4::text IS NULL OR status = $4)
           ORDER BY created_at DESC LIMIT $5 OFFSET $6"#,
    )
    .bind(app_id)
    .bind(user_id)
    .bind(event_type)
    .bind(status)
    .bind(limit)
    .bind(offset)
    .fetch_all(pool)
    .await
}

/// `(score, created_at)` of the user's safety incidents since `since`, newest first, for
/// decayed risk scoring.
pub async fn list_safety_incident_scores(
    pool: &DbPool,
    app_id: Uuid,
    user_id: Uuid,
    since: DateTime<Utc>,
    limit: i64,
) -> Result<Vec<(f64, DateTime<Utc>)>, sqlx::Error> {
    sqlx::query_as::<_, (f64, DateTime<Utc>)>(
        r#"SELECT COALESCE((evidence->>'score')::float8, 0), created_at FROM moderation_events
           WHERE app_id = $1 AND target_user_id = $2 AND event_type LIKE 'safety.%'
             AND created_at > $3
           ORDER BY created_at DESC LIMIT $4"#,
    )
    .bind(app_id)
    .bind(user_id)
    .bind(since)
    .bind(limit)
    .fetch_all(pool)
    .await
}

/// Channels of `app_id` the user currently has an open membership in.
pub async fn get_user_channels_in_app(
    pool: &DbPool,
    app_id: Uuid,
    user_id: Uuid,
) -> Result<Vec<ChannelMembershipRow>, sqlx::Error> {
    sqlx::query_as::<_, ChannelMembershipRow>(
        r#"SELECT m.* FROM channel_memberships m
           JOIN channels c ON c.id = m.channel_id
           WHERE c.app_id = $1 AND m.user_id = $2 AND m.left_at IS NULL"#,
    )
    .bind(app_id)
    .bind(user_id)
    .fetch_all(pool)
    .await
}

pub async fn get_moderation_event(
    pool: &DbPool,
    app_id: Uuid,
    id: Uuid,
) -> Result<Option<ModerationEventRow>, sqlx::Error> {
    sqlx::query_as::<_, ModerationEventRow>(
        "SELECT * FROM moderation_events WHERE app_id = $1 AND id = $2",
    )
    .bind(app_id)
    .bind(id)
    .fetch_optional(pool)
    .await
}

pub async fn resolve_moderation_event(
    pool: &DbPool,
    app_id: Uuid,
    event_id: Uuid,
    moderator_id: Uuid,
    resolution: &str,
) -> Result<u64, sqlx::Error> {
    let r = sqlx::query("UPDATE moderation_events SET status = 'resolved', moderator_user_id = $3, resolution = $4, resolved_at = NOW() WHERE app_id = $1 AND id = $2 AND status <> 'resolved'")
        .bind(app_id).bind(event_id).bind(moderator_id).bind(resolution)
        .execute(pool).await?;
    Ok(r.rows_affected())
}

// ── Recording Queries ──

pub async fn create_recording(
    pool: &DbPool,
    rec: &RecordingRow,
) -> Result<RecordingRow, sqlx::Error> {
    sqlx::query_as::<_, RecordingRow>(
        r#"INSERT INTO recordings (id, app_id, channel_id, session_id, user_id, file_path, file_size_bytes, duration_secs, format, encrypted, encryption_key_id, started_at, ended_at, expires_at, created_at, kind)
           VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16) RETURNING *"#
    )
    .bind(rec.id).bind(rec.app_id).bind(rec.channel_id).bind(rec.session_id)
    .bind(rec.user_id).bind(&rec.file_path).bind(rec.file_size_bytes)
    .bind(rec.duration_secs).bind(&rec.format).bind(rec.encrypted)
    .bind(&rec.encryption_key_id).bind(rec.started_at).bind(rec.ended_at).bind(rec.expires_at)
    .bind(rec.created_at).bind(&rec.kind)
    .fetch_one(pool).await
}

pub async fn get_recording(
    pool: &DbPool,
    app_id: Uuid,
    id: Uuid,
) -> Result<Option<RecordingRow>, sqlx::Error> {
    sqlx::query_as::<_, RecordingRow>("SELECT * FROM recordings WHERE app_id = $1 AND id = $2")
        .bind(app_id)
        .bind(id)
        .fetch_optional(pool)
        .await
}

pub async fn list_recordings(
    pool: &DbPool,
    app_id: Uuid,
    channel_id: Option<Uuid>,
    limit: i64,
    offset: i64,
) -> Result<Vec<RecordingRow>, sqlx::Error> {
    sqlx::query_as::<_, RecordingRow>(
        "SELECT * FROM recordings WHERE app_id = $1 AND ($2::uuid IS NULL OR channel_id = $2) ORDER BY started_at DESC LIMIT $3 OFFSET $4"
    )
    .bind(app_id).bind(channel_id).bind(limit).bind(offset).fetch_all(pool).await
}

/// Returns `false` when the row no longer exists (erased while the recording was live).
pub async fn finish_recording(
    pool: &DbPool,
    id: Uuid,
    size: i64,
    duration: f64,
) -> Result<bool, sqlx::Error> {
    let r = sqlx::query("UPDATE recordings SET ended_at = NOW(), file_size_bytes = $2, duration_secs = $3 WHERE id = $1")
        .bind(id).bind(size).bind(duration).execute(pool).await?;
    Ok(r.rows_affected() > 0)
}

pub async fn delete_expired_recordings(pool: &DbPool) -> Result<u64, sqlx::Error> {
    let result = sqlx::query("DELETE FROM recordings WHERE expires_at < NOW()")
        .execute(pool)
        .await?;
    Ok(result.rows_affected())
}

pub async fn list_expired_recordings(
    pool: &DbPool,
    limit: i64,
) -> Result<Vec<RecordingRow>, sqlx::Error> {
    sqlx::query_as::<_, RecordingRow>("SELECT * FROM recordings WHERE expires_at < NOW() LIMIT $1")
        .bind(limit)
        .fetch_all(pool)
        .await
}

pub async fn delete_recording(pool: &DbPool, id: uuid::Uuid) -> Result<(), sqlx::Error> {
    sqlx::query("DELETE FROM recordings WHERE id = $1")
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}

// ── Chat Queries ──

pub async fn insert_chat_message(pool: &DbPool, m: &ChatMessageRow) -> Result<(), sqlx::Error> {
    sqlx::query(
        r#"INSERT INTO chat_messages (id, app_id, channel_id, from_user_id, display_name, to_user_id, text, metadata, sent_at)
           VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)"#,
    )
    .bind(m.id).bind(m.app_id).bind(m.channel_id).bind(m.from_user_id)
    .bind(&m.display_name).bind(m.to_user_id).bind(&m.text).bind(&m.metadata)
    .bind(m.sent_at)
    .execute(pool).await?;
    Ok(())
}

/// Newest-first page of a channel's messages, optionally strictly older than `before`.
pub async fn list_channel_messages(
    pool: &DbPool,
    app_id: Uuid,
    channel_id: Uuid,
    before: Option<DateTime<Utc>>,
    limit: i64,
) -> Result<Vec<ChatMessageRow>, sqlx::Error> {
    sqlx::query_as::<_, ChatMessageRow>(
        r#"SELECT * FROM chat_messages
           WHERE app_id = $1 AND channel_id = $2 AND ($3::timestamptz IS NULL OR sent_at < $3)
           ORDER BY sent_at DESC LIMIT $4"#,
    )
    .bind(app_id)
    .bind(channel_id)
    .bind(before)
    .bind(limit)
    .fetch_all(pool)
    .await
}

/// Newest-first page of everything a user sent or was sent directly (moderation evidence,
/// data export).
pub async fn list_user_messages(
    pool: &DbPool,
    app_id: Uuid,
    user_id: Uuid,
    before: Option<DateTime<Utc>>,
    limit: i64,
) -> Result<Vec<ChatMessageRow>, sqlx::Error> {
    sqlx::query_as::<_, ChatMessageRow>(
        r#"SELECT * FROM chat_messages
           WHERE app_id = $1 AND (from_user_id = $2 OR to_user_id = $2)
             AND ($3::timestamptz IS NULL OR sent_at < $3)
           ORDER BY sent_at DESC LIMIT $4"#,
    )
    .bind(app_id)
    .bind(user_id)
    .bind(before)
    .bind(limit)
    .fetch_all(pool)
    .await
}

pub async fn delete_chat_messages_before(
    pool: &DbPool,
    cutoff: DateTime<Utc>,
) -> Result<u64, sqlx::Error> {
    let r = sqlx::query("DELETE FROM chat_messages WHERE sent_at < $1")
        .bind(cutoff)
        .execute(pool)
        .await?;
    Ok(r.rows_affected())
}

// ── Webhook Queries ──

pub async fn insert_webhook(
    pool: &DbPool,
    w: &WebhookSubscriptionRow,
) -> Result<WebhookSubscriptionRow, sqlx::Error> {
    sqlx::query_as::<_, WebhookSubscriptionRow>(
        r#"INSERT INTO webhook_subscriptions (id, app_id, url, secret, events, description, enabled, created_at, updated_at)
           VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $8) RETURNING *"#,
    )
    .bind(w.id).bind(w.app_id).bind(&w.url).bind(&w.secret).bind(&w.events)
    .bind(&w.description).bind(w.enabled).bind(w.created_at)
    .fetch_one(pool).await
}

pub async fn get_webhook(
    pool: &DbPool,
    app_id: Uuid,
    id: Uuid,
) -> Result<Option<WebhookSubscriptionRow>, sqlx::Error> {
    sqlx::query_as::<_, WebhookSubscriptionRow>(
        "SELECT * FROM webhook_subscriptions WHERE app_id = $1 AND id = $2",
    )
    .bind(app_id)
    .bind(id)
    .fetch_optional(pool)
    .await
}

pub async fn list_webhooks(
    pool: &DbPool,
    app_id: Uuid,
) -> Result<Vec<WebhookSubscriptionRow>, sqlx::Error> {
    sqlx::query_as::<_, WebhookSubscriptionRow>(
        "SELECT * FROM webhook_subscriptions WHERE app_id = $1 ORDER BY created_at",
    )
    .bind(app_id)
    .fetch_all(pool)
    .await
}

pub async fn list_enabled_webhooks(
    pool: &DbPool,
    app_id: Uuid,
) -> Result<Vec<WebhookSubscriptionRow>, sqlx::Error> {
    sqlx::query_as::<_, WebhookSubscriptionRow>(
        "SELECT * FROM webhook_subscriptions WHERE app_id = $1 AND enabled ORDER BY created_at",
    )
    .bind(app_id)
    .fetch_all(pool)
    .await
}

pub async fn count_webhooks(pool: &DbPool, app_id: Uuid) -> Result<i64, sqlx::Error> {
    let row: (i64,) =
        sqlx::query_as("SELECT COUNT(*) FROM webhook_subscriptions WHERE app_id = $1")
            .bind(app_id)
            .fetch_one(pool)
            .await?;
    Ok(row.0)
}

/// Partial update; `None` keeps the current value. Re-enabling resets the failure counter.
pub async fn update_webhook(
    pool: &DbPool,
    app_id: Uuid,
    id: Uuid,
    url: Option<&str>,
    events: Option<&[String]>,
    description: Option<Option<&str>>,
    enabled: Option<bool>,
) -> Result<Option<WebhookSubscriptionRow>, sqlx::Error> {
    sqlx::query_as::<_, WebhookSubscriptionRow>(
        r#"UPDATE webhook_subscriptions SET
               url = COALESCE($3, url),
               events = COALESCE($4, events),
               description = CASE WHEN $5 THEN $6 ELSE description END,
               enabled = COALESCE($7, enabled),
               consecutive_failures = CASE WHEN $7 THEN 0 ELSE consecutive_failures END,
               updated_at = NOW()
           WHERE app_id = $1 AND id = $2 RETURNING *"#,
    )
    .bind(app_id)
    .bind(id)
    .bind(url)
    .bind(events)
    .bind(description.is_some())
    .bind(description.flatten())
    .bind(enabled)
    .fetch_optional(pool)
    .await
}

pub async fn rotate_webhook_secret(
    pool: &DbPool,
    app_id: Uuid,
    id: Uuid,
    secret: &str,
) -> Result<Option<WebhookSubscriptionRow>, sqlx::Error> {
    sqlx::query_as::<_, WebhookSubscriptionRow>(
        "UPDATE webhook_subscriptions SET secret = $3, updated_at = NOW() WHERE app_id = $1 AND id = $2 RETURNING *",
    )
    .bind(app_id)
    .bind(id)
    .bind(secret)
    .fetch_optional(pool)
    .await
}

pub async fn delete_webhook(pool: &DbPool, app_id: Uuid, id: Uuid) -> Result<bool, sqlx::Error> {
    let r = sqlx::query("DELETE FROM webhook_subscriptions WHERE app_id = $1 AND id = $2")
        .bind(app_id)
        .bind(id)
        .execute(pool)
        .await?;
    Ok(r.rows_affected() > 0)
}

/// Enqueues a delivery unless the subscription already has `max_pending` undelivered rows
/// (dead endpoint). Returns `false` when dropped.
pub async fn enqueue_webhook_delivery(
    pool: &DbPool,
    d: &WebhookDeliveryRow,
    max_pending: i64,
) -> Result<bool, sqlx::Error> {
    let r = sqlx::query(
        r#"INSERT INTO webhook_deliveries (id, subscription_id, app_id, event_id, event_type, payload, status, attempts, next_attempt_at, created_at)
           SELECT $1, $2, $3, $4, $5, $6, 'pending', 0, $7, $7
           WHERE (SELECT COUNT(*) FROM webhook_deliveries WHERE subscription_id = $2 AND status = 'pending') < $8"#,
    )
    .bind(d.id).bind(d.subscription_id).bind(d.app_id).bind(d.event_id).bind(&d.event_type)
    .bind(&d.payload).bind(d.created_at).bind(max_pending)
    .execute(pool).await?;
    Ok(r.rows_affected() > 0)
}

/// Leases up to `limit` due deliveries for `lease` seconds (skipping rows another node holds)
/// and counts the attempt. Rows whose lease expired are picked up again, so a node crash
/// mid-delivery costs at most one duplicate.
pub async fn lease_due_webhook_deliveries(
    pool: &DbPool,
    limit: i64,
    lease_secs: i64,
) -> Result<Vec<WebhookDeliveryRow>, sqlx::Error> {
    sqlx::query_as::<_, WebhookDeliveryRow>(
        r#"UPDATE webhook_deliveries d
           SET leased_until = NOW() + make_interval(secs => $2), attempts = d.attempts + 1
           FROM (
               SELECT id FROM webhook_deliveries
               WHERE status = 'pending' AND next_attempt_at <= NOW()
                 AND (leased_until IS NULL OR leased_until < NOW())
               ORDER BY next_attempt_at
               LIMIT $1
               FOR UPDATE SKIP LOCKED
           ) due
           WHERE d.id = due.id
           RETURNING d.*"#,
    )
    .bind(limit)
    .bind(lease_secs as f64)
    .fetch_all(pool)
    .await
}

pub async fn mark_webhook_delivered(
    pool: &DbPool,
    id: Uuid,
    status: i16,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        r#"UPDATE webhook_deliveries SET status = 'delivered', delivered_at = NOW(), leased_until = NULL,
               last_status = $2, last_error = NULL WHERE id = $1"#,
    )
    .bind(id)
    .bind(status)
    .execute(pool)
    .await?;
    Ok(())
}

/// Records a failed attempt: schedules the retry at `next_attempt_at`, or gives up (`failed`)
/// when `None`.
pub async fn mark_webhook_attempt_failed(
    pool: &DbPool,
    id: Uuid,
    status: Option<i16>,
    error: &str,
    next_attempt_at: Option<DateTime<Utc>>,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        r#"UPDATE webhook_deliveries SET
               status = CASE WHEN $4::timestamptz IS NULL THEN 'failed' ELSE 'pending' END,
               next_attempt_at = COALESCE($4, next_attempt_at),
               leased_until = NULL, last_status = $2, last_error = $3
           WHERE id = $1"#,
    )
    .bind(id)
    .bind(status)
    .bind(error)
    .bind(next_attempt_at)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn record_webhook_result(
    pool: &DbPool,
    subscription_id: Uuid,
    ok: bool,
    status: Option<i16>,
    error: Option<&str>,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        r#"UPDATE webhook_subscriptions SET
               last_delivery_at = NOW(), last_status = $3, last_error = $4,
               consecutive_failures = CASE WHEN $2 THEN 0 ELSE consecutive_failures + 1 END
           WHERE id = $1"#,
    )
    .bind(subscription_id)
    .bind(ok)
    .bind(status)
    .bind(error)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn list_webhook_deliveries(
    pool: &DbPool,
    app_id: Uuid,
    subscription_id: Uuid,
    status: Option<&str>,
    limit: i64,
    offset: i64,
) -> Result<Vec<WebhookDeliveryRow>, sqlx::Error> {
    sqlx::query_as::<_, WebhookDeliveryRow>(
        r#"SELECT * FROM webhook_deliveries
           WHERE app_id = $1 AND subscription_id = $2 AND ($3::text IS NULL OR status = $3)
           ORDER BY created_at DESC LIMIT $4 OFFSET $5"#,
    )
    .bind(app_id)
    .bind(subscription_id)
    .bind(status)
    .bind(limit)
    .bind(offset)
    .fetch_all(pool)
    .await
}

pub async fn get_webhook_delivery(
    pool: &DbPool,
    app_id: Uuid,
    subscription_id: Uuid,
    id: Uuid,
) -> Result<Option<WebhookDeliveryRow>, sqlx::Error> {
    sqlx::query_as::<_, WebhookDeliveryRow>(
        "SELECT * FROM webhook_deliveries WHERE app_id = $1 AND subscription_id = $2 AND id = $3",
    )
    .bind(app_id)
    .bind(subscription_id)
    .bind(id)
    .fetch_optional(pool)
    .await
}

/// Puts a delivered/failed row back in the queue for an immediate attempt.
pub async fn requeue_webhook_delivery(
    pool: &DbPool,
    app_id: Uuid,
    subscription_id: Uuid,
    id: Uuid,
) -> Result<Option<WebhookDeliveryRow>, sqlx::Error> {
    sqlx::query_as::<_, WebhookDeliveryRow>(
        r#"UPDATE webhook_deliveries SET status = 'pending', attempts = 0, next_attempt_at = NOW(),
               leased_until = NULL, delivered_at = NULL
           WHERE app_id = $1 AND subscription_id = $2 AND id = $3 RETURNING *"#,
    )
    .bind(app_id)
    .bind(subscription_id)
    .bind(id)
    .fetch_optional(pool)
    .await
}

pub async fn delete_finished_webhook_deliveries_before(
    pool: &DbPool,
    cutoff: DateTime<Utc>,
) -> Result<u64, sqlx::Error> {
    let r =
        sqlx::query("DELETE FROM webhook_deliveries WHERE status <> 'pending' AND created_at < $1")
            .bind(cutoff)
            .execute(pool)
            .await?;
    Ok(r.rows_affected())
}

/// Open memberships of every live channel in the app (with the member's display name and the
/// channel type), for `webhook.resync` snapshots.
pub async fn list_active_channel_members(
    pool: &DbPool,
    app_id: Uuid,
) -> Result<Vec<ActiveMemberRow>, sqlx::Error> {
    sqlx::query_as::<_, ActiveMemberRow>(
        r#"SELECT m.channel_id, c.channel_type, m.user_id, u.display_name, m.session_id,
                  m.role, m.is_muted, m.is_server_muted, m.ssrc, m.joined_at
           FROM channel_memberships m
           JOIN channels c ON c.id = m.channel_id
           JOIN users u ON u.id = m.user_id
           WHERE c.app_id = $1 AND c.deleted_at IS NULL AND m.left_at IS NULL
           ORDER BY m.channel_id, m.joined_at"#,
    )
    .bind(app_id)
    .fetch_all(pool)
    .await
}

// ── Media Node Queries ──

pub async fn upsert_media_node(
    pool: &DbPool,
    node: &MediaNodeRow,
) -> Result<MediaNodeRow, sqlx::Error> {
    sqlx::query_as::<_, MediaNodeRow>(
        r#"INSERT INTO media_nodes (id, region, address, media_port, api_port, capacity, active_channels, active_participants, cpu_usage, memory_usage, bandwidth_in_mbps, bandwidth_out_mbps, healthy, version, last_heartbeat, registered_at, cascade_port, ws_url, api_url, latitude, longitude)
           VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16, $17, $18, $19, $20, $21)
           ON CONFLICT (id) DO UPDATE SET
             region = EXCLUDED.region, address = EXCLUDED.address, media_port = EXCLUDED.media_port,
             api_port = EXCLUDED.api_port, cascade_port = EXCLUDED.cascade_port, version = EXCLUDED.version,
             ws_url = EXCLUDED.ws_url, api_url = EXCLUDED.api_url,
             latitude = EXCLUDED.latitude, longitude = EXCLUDED.longitude,
             active_channels = EXCLUDED.active_channels, active_participants = EXCLUDED.active_participants,
             cpu_usage = EXCLUDED.cpu_usage, memory_usage = EXCLUDED.memory_usage,
             bandwidth_in_mbps = EXCLUDED.bandwidth_in_mbps, bandwidth_out_mbps = EXCLUDED.bandwidth_out_mbps,
             healthy = EXCLUDED.healthy, last_heartbeat = EXCLUDED.last_heartbeat
           RETURNING *"#
    )
    .bind(node.id).bind(&node.region).bind(&node.address).bind(node.media_port)
    .bind(node.api_port).bind(node.capacity).bind(node.active_channels)
    .bind(node.active_participants).bind(node.cpu_usage).bind(node.memory_usage)
    .bind(node.bandwidth_in_mbps).bind(node.bandwidth_out_mbps)
    .bind(node.healthy).bind(&node.version).bind(node.last_heartbeat).bind(node.registered_at)
    .bind(node.cascade_port)
    .bind(&node.ws_url).bind(&node.api_url).bind(node.latitude).bind(node.longitude)
    .fetch_one(pool).await
}

/// `(channel_id, media_node_id)` pairs for every live membership of the given channels that
/// is hosted on a node other than `local_node`. Drives automatic cascade topology.
pub async fn remote_nodes_for_channels(
    pool: &DbPool,
    channel_ids: &[Uuid],
    local_node: Uuid,
) -> Result<Vec<(Uuid, Uuid)>, sqlx::Error> {
    if channel_ids.is_empty() {
        return Ok(Vec::new());
    }
    sqlx::query_as::<_, (Uuid, Uuid)>(
        r#"SELECT DISTINCT m.channel_id, s.media_node_id
           FROM channel_memberships m
           JOIN sessions s ON s.id = m.session_id
           WHERE m.channel_id = ANY($1)
             AND m.left_at IS NULL
             AND s.disconnected_at IS NULL
             AND s.media_node_id <> $2"#,
    )
    .bind(channel_ids)
    .bind(local_node)
    .fetch_all(pool)
    .await
}

pub async fn get_healthy_media_nodes(
    pool: &DbPool,
    region: Option<&str>,
) -> Result<Vec<MediaNodeRow>, sqlx::Error> {
    match region {
        Some(r) => {
            sqlx::query_as::<_, MediaNodeRow>(
                "SELECT * FROM media_nodes WHERE healthy = true AND region = $1 ORDER BY active_participants ASC"
            ).bind(r).fetch_all(pool).await
        }
        None => {
            sqlx::query_as::<_, MediaNodeRow>(
                "SELECT * FROM media_nodes WHERE healthy = true ORDER BY active_participants ASC"
            ).fetch_all(pool).await
        }
    }
}

pub async fn get_all_media_nodes(pool: &DbPool) -> Result<Vec<MediaNodeRow>, sqlx::Error> {
    sqlx::query_as::<_, MediaNodeRow>("SELECT * FROM media_nodes ORDER BY region, address")
        .fetch_all(pool)
        .await
}

pub async fn mark_node_unhealthy(pool: &DbPool, node_id: Uuid) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE media_nodes SET healthy = false WHERE id = $1")
        .bind(node_id)
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn delete_media_node(pool: &DbPool, node_id: Uuid) -> Result<(), sqlx::Error> {
    sqlx::query("DELETE FROM media_nodes WHERE id = $1")
        .bind(node_id)
        .execute(pool)
        .await?;
    Ok(())
}

// ── Audit Log Queries ──

pub async fn insert_audit_log(pool: &DbPool, entry: &AuditLogRow) -> Result<(), sqlx::Error> {
    sqlx::query(
        r#"INSERT INTO audit_log (id, app_id, actor_id, action, target_type, target_id, details, ip_address, previous_hash, hash, created_at)
           VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11)"#
    )
    .bind(entry.id).bind(entry.app_id).bind(entry.actor_id)
    .bind(&entry.action).bind(&entry.target_type).bind(&entry.target_id)
    .bind(&entry.details).bind(&entry.ip_address)
    .bind(&entry.previous_hash).bind(&entry.hash).bind(entry.created_at)
    .execute(pool).await?;
    Ok(())
}

pub async fn list_audit_logs(
    pool: &DbPool,
    app_id: Option<Uuid>,
    limit: i64,
    offset: i64,
) -> Result<Vec<AuditLogRow>, sqlx::Error> {
    match app_id {
        Some(aid) => sqlx::query_as::<_, AuditLogRow>(
            "SELECT * FROM audit_log WHERE app_id = $1 ORDER BY created_at DESC LIMIT $2 OFFSET $3",
        )
        .bind(aid)
        .bind(limit)
        .bind(offset)
        .fetch_all(pool)
        .await,
        None => {
            sqlx::query_as::<_, AuditLogRow>(
                "SELECT * FROM audit_log ORDER BY created_at DESC LIMIT $1 OFFSET $2",
            )
            .bind(limit)
            .bind(offset)
            .fetch_all(pool)
            .await
        }
    }
}

// ── API Key Queries ──

pub async fn create_api_key(pool: &DbPool, key: &ApiKeyRow) -> Result<ApiKeyRow, sqlx::Error> {
    sqlx::query_as::<_, ApiKeyRow>(
        r#"INSERT INTO api_keys (id, app_id, name, key_prefix, key_hash, permissions, rate_limit, active, expires_at, created_at)
           VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10) RETURNING *"#
    )
    .bind(key.id).bind(key.app_id).bind(&key.name).bind(&key.key_prefix)
    .bind(&key.key_hash).bind(&key.permissions).bind(key.rate_limit)
    .bind(key.active).bind(key.expires_at).bind(key.created_at)
    .fetch_one(pool).await
}

/// Fetch by prefix regardless of state; the caller decides how to treat inactive/expired keys.
pub async fn get_api_key_by_prefix(
    pool: &DbPool,
    prefix: &str,
) -> Result<Option<ApiKeyRow>, sqlx::Error> {
    sqlx::query_as::<_, ApiKeyRow>(
        "SELECT * FROM api_keys WHERE key_prefix = $1 ORDER BY active DESC, created_at DESC LIMIT 1"
    )
        .bind(prefix).fetch_optional(pool).await
}

pub async fn revoke_api_key(pool: &DbPool, app_id: Uuid, key_id: Uuid) -> Result<u64, sqlx::Error> {
    let r = sqlx::query("UPDATE api_keys SET active = false, revoked_at = NOW() WHERE app_id = $1 AND id = $2 AND active = true")
        .bind(app_id).bind(key_id).execute(pool).await?;
    Ok(r.rows_affected())
}

pub async fn list_api_keys(pool: &DbPool, app_id: Uuid) -> Result<Vec<ApiKeyRow>, sqlx::Error> {
    sqlx::query_as::<_, ApiKeyRow>(
        "SELECT * FROM api_keys WHERE app_id = $1 ORDER BY created_at DESC",
    )
    .bind(app_id)
    .fetch_all(pool)
    .await
}

pub async fn touch_api_key(pool: &DbPool, key_id: Uuid) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE api_keys SET last_used_at = NOW() WHERE id = $1")
        .bind(key_id)
        .execute(pool)
        .await?;
    Ok(())
}

// ── Analytics Queries ──

pub async fn insert_analytics_snapshot(
    pool: &DbPool,
    snap: &AnalyticsSnapshotRow,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        r#"INSERT INTO analytics_snapshots (id, app_id, timestamp, active_users, active_channels, peak_concurrent, total_minutes, bandwidth_gb, avg_latency_ms, avg_packet_loss, error_count)
           VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11)"#
    )
    .bind(snap.id).bind(snap.app_id).bind(snap.timestamp)
    .bind(snap.active_users).bind(snap.active_channels).bind(snap.peak_concurrent)
    .bind(snap.total_minutes).bind(snap.bandwidth_gb)
    .bind(snap.avg_latency_ms).bind(snap.avg_packet_loss).bind(snap.error_count)
    .execute(pool).await?;
    Ok(())
}

pub async fn get_analytics(
    pool: &DbPool,
    app_id: Uuid,
    from: DateTime<Utc>,
    to: DateTime<Utc>,
) -> Result<Vec<AnalyticsSnapshotRow>, sqlx::Error> {
    sqlx::query_as::<_, AnalyticsSnapshotRow>(
        "SELECT * FROM analytics_snapshots WHERE app_id = $1 AND timestamp >= $2 AND timestamp <= $3 ORDER BY timestamp ASC"
    )
    .bind(app_id).bind(from).bind(to).fetch_all(pool).await
}

// ── Admin User Queries ──

pub async fn create_admin_user(
    pool: &DbPool,
    admin: &AdminUserRow,
) -> Result<AdminUserRow, sqlx::Error> {
    sqlx::query_as::<_, AdminUserRow>(
        r#"INSERT INTO admin_users (id, email, password_hash, display_name, role, active, created_at, updated_at)
           VALUES ($1, $2, $3, $4, $5, $6, $7, $8) RETURNING *"#
    )
    .bind(admin.id).bind(&admin.email).bind(&admin.password_hash)
    .bind(&admin.display_name).bind(&admin.role).bind(admin.active)
    .bind(admin.created_at).bind(admin.updated_at)
    .fetch_one(pool).await
}

pub async fn count_admin_users(pool: &DbPool) -> Result<i64, sqlx::Error> {
    let row: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM admin_users WHERE active = true")
        .fetch_one(pool)
        .await?;
    Ok(row.0)
}

pub async fn get_admin_by_id(pool: &DbPool, id: Uuid) -> Result<Option<AdminUserRow>, sqlx::Error> {
    sqlx::query_as::<_, AdminUserRow>("SELECT * FROM admin_users WHERE id = $1 AND active = true")
        .bind(id)
        .fetch_optional(pool)
        .await
}

pub async fn get_admin_by_email(
    pool: &DbPool,
    email: &str,
) -> Result<Option<AdminUserRow>, sqlx::Error> {
    sqlx::query_as::<_, AdminUserRow>(
        "SELECT * FROM admin_users WHERE email = $1 AND active = true",
    )
    .bind(email)
    .fetch_optional(pool)
    .await
}

pub async fn update_admin_login(pool: &DbPool, admin_id: Uuid) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE admin_users SET last_login_at = NOW() WHERE id = $1")
        .bind(admin_id)
        .execute(pool)
        .await?;
    Ok(())
}
