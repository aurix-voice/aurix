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
        .bind(id).fetch_optional(pool).await
}

pub async fn list_apps(pool: &DbPool, limit: i64, offset: i64) -> Result<Vec<AppRow>, sqlx::Error> {
    sqlx::query_as::<_, AppRow>("SELECT * FROM apps WHERE active = true ORDER BY created_at DESC LIMIT $1 OFFSET $2")
        .bind(limit).bind(offset).fetch_all(pool).await
}

pub async fn count_apps(pool: &DbPool) -> Result<i64, sqlx::Error> {
    let row: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM apps WHERE active = true")
        .fetch_one(pool).await?;
    Ok(row.0)
}

pub async fn update_app_key_hash(pool: &DbPool, app_id: uuid::Uuid, key_hash: &str) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE apps SET api_key_hash = $1, updated_at = NOW() WHERE id = $2")
        .bind(key_hash)
        .bind(app_id)
        .execute(pool)
        .await?;
    Ok(())
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

pub async fn get_user(pool: &DbPool, app_id: Uuid, id: Uuid) -> Result<Option<UserRow>, sqlx::Error> {
    sqlx::query_as::<_, UserRow>("SELECT * FROM users WHERE app_id = $1 AND id = $2")
        .bind(app_id).bind(id).fetch_optional(pool).await
}

pub async fn get_user_by_external_id(pool: &DbPool, app_id: Uuid, external_id: &str) -> Result<Option<UserRow>, sqlx::Error> {
    sqlx::query_as::<_, UserRow>("SELECT * FROM users WHERE app_id = $1 AND external_id = $2")
        .bind(app_id).bind(external_id).fetch_optional(pool).await
}

pub async fn search_users(pool: &DbPool, app_id: Uuid, query: &str, limit: i64, offset: i64) -> Result<Vec<UserRow>, sqlx::Error> {
    let pattern = format!("%{}%", query);
    sqlx::query_as::<_, UserRow>(
        "SELECT * FROM users WHERE app_id = $1 AND (display_name ILIKE $2 OR external_id ILIKE $2) ORDER BY created_at DESC LIMIT $3 OFFSET $4"
    )
    .bind(app_id).bind(&pattern).bind(limit).bind(offset)
    .fetch_all(pool).await
}

pub async fn ban_user(pool: &DbPool, app_id: Uuid, user_id: Uuid, reason: &str, expires_at: Option<DateTime<Utc>>) -> Result<u64, sqlx::Error> {
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

pub async fn add_user_session_minutes(pool: &DbPool, user_id: Uuid, minutes: i64) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE users SET total_session_minutes = total_session_minutes + $2, last_seen_at = NOW(), updated_at = NOW() WHERE id = $1")
        .bind(user_id).bind(minutes).execute(pool).await?;
    Ok(())
}

pub async fn update_user_last_seen(pool: &DbPool, user_id: Uuid) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE users SET last_seen_at = NOW(), updated_at = NOW() WHERE id = $1")
        .bind(user_id).execute(pool).await?;
    Ok(())
}

pub async fn count_users(pool: &DbPool, app_id: Uuid) -> Result<i64, sqlx::Error> {
    let row: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM users WHERE app_id = $1")
        .bind(app_id).fetch_one(pool).await?;
    Ok(row.0)
}

// ── Channel Queries ──

pub async fn create_channel(pool: &DbPool, ch: &ChannelRow) -> Result<ChannelRow, sqlx::Error> {
    sqlx::query_as::<_, ChannelRow>(
        r#"INSERT INTO channels (id, app_id, name, channel_type, config, max_participants, is_persistent, active_participants, created_at, updated_at)
           VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)
           RETURNING *"#
    )
    .bind(ch.id).bind(ch.app_id).bind(&ch.name).bind(&ch.channel_type)
    .bind(&ch.config).bind(ch.max_participants).bind(ch.is_persistent)
    .bind(ch.active_participants).bind(ch.created_at).bind(ch.updated_at)
    .fetch_one(pool).await
}

pub async fn get_channel(pool: &DbPool, app_id: Uuid, id: Uuid) -> Result<Option<ChannelRow>, sqlx::Error> {
    sqlx::query_as::<_, ChannelRow>("SELECT * FROM channels WHERE app_id = $1 AND id = $2 AND deleted_at IS NULL")
        .bind(app_id).bind(id).fetch_optional(pool).await
}

/// Cross-tenant lookup for internal (non-API) use only, e.g. resolving a channel row from a
/// media-plane identifier. Callers MUST compare `app_id` before acting on the result.
pub async fn get_channel_any_app(pool: &DbPool, id: Uuid) -> Result<Option<ChannelRow>, sqlx::Error> {
    sqlx::query_as::<_, ChannelRow>("SELECT * FROM channels WHERE id = $1 AND deleted_at IS NULL")
        .bind(id).fetch_optional(pool).await
}

pub async fn list_channels(pool: &DbPool, app_id: Uuid, limit: i64, offset: i64) -> Result<Vec<ChannelRow>, sqlx::Error> {
    sqlx::query_as::<_, ChannelRow>(
        "SELECT * FROM channels WHERE app_id = $1 AND deleted_at IS NULL ORDER BY created_at DESC LIMIT $2 OFFSET $3"
    )
    .bind(app_id).bind(limit).bind(offset)
    .fetch_all(pool).await
}

pub async fn list_active_channels(pool: &DbPool, app_id: Uuid, limit: i64, offset: i64) -> Result<Vec<ChannelRow>, sqlx::Error> {
    sqlx::query_as::<_, ChannelRow>(
        "SELECT * FROM channels WHERE app_id = $1 AND deleted_at IS NULL AND active_participants > 0 ORDER BY active_participants DESC LIMIT $2 OFFSET $3"
    )
    .bind(app_id).bind(limit).bind(offset)
    .fetch_all(pool).await
}

pub async fn update_channel_participant_count(pool: &DbPool, channel_id: Uuid, delta: i32) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE channels SET active_participants = GREATEST(0, active_participants + $2), updated_at = NOW() WHERE id = $1")
        .bind(channel_id).bind(delta)
        .execute(pool).await?;
    Ok(())
}

pub async fn delete_channel(pool: &DbPool, app_id: Uuid, channel_id: Uuid) -> Result<u64, sqlx::Error> {
    let r = sqlx::query("UPDATE channels SET deleted_at = NOW(), updated_at = NOW() WHERE app_id = $1 AND id = $2 AND deleted_at IS NULL")
        .bind(app_id).bind(channel_id).execute(pool).await?;
    Ok(r.rows_affected())
}

pub async fn update_channel_config(pool: &DbPool, app_id: Uuid, channel_id: Uuid, config: &serde_json::Value, max_participants: i32) -> Result<u64, sqlx::Error> {
    let r = sqlx::query("UPDATE channels SET config = $3, max_participants = $4, updated_at = NOW() WHERE app_id = $1 AND id = $2 AND deleted_at IS NULL")
        .bind(app_id).bind(channel_id).bind(config).bind(max_participants).execute(pool).await?;
    Ok(r.rows_affected())
}

pub async fn count_channels(pool: &DbPool, app_id: Uuid) -> Result<i64, sqlx::Error> {
    let row: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM channels WHERE app_id = $1 AND deleted_at IS NULL")
        .bind(app_id).fetch_one(pool).await?;
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

pub async fn close_session(pool: &DbPool, session_id: Uuid, reason: &str, quality: Option<serde_json::Value>) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE sessions SET disconnected_at = NOW(), disconnect_reason = $2, quality_stats = $3 WHERE id = $1")
        .bind(session_id).bind(reason).bind(quality)
        .execute(pool).await?;
    Ok(())
}

pub async fn get_session(pool: &DbPool, app_id: Uuid, id: Uuid) -> Result<Option<SessionRow>, sqlx::Error> {
    sqlx::query_as::<_, SessionRow>("SELECT * FROM sessions WHERE app_id = $1 AND id = $2")
        .bind(app_id).bind(id).fetch_optional(pool).await
}

/// Close every session that is still marked open for a media node (used on node startup
/// so that a crash does not leave phantom active sessions).
pub async fn close_stale_sessions_for_node(pool: &DbPool, media_node_id: Uuid, reason: &str) -> Result<u64, sqlx::Error> {
    let r = sqlx::query("UPDATE sessions SET disconnected_at = NOW(), disconnect_reason = $2 WHERE media_node_id = $1 AND disconnected_at IS NULL")
        .bind(media_node_id).bind(reason).execute(pool).await?;
    Ok(r.rows_affected())
}

pub async fn close_stale_memberships_for_node(pool: &DbPool, media_node_id: Uuid) -> Result<u64, sqlx::Error> {
    let r = sqlx::query(
        "UPDATE channel_memberships SET left_at = NOW() WHERE left_at IS NULL AND session_id IN (SELECT id FROM sessions WHERE media_node_id = $1)"
    ).bind(media_node_id).execute(pool).await?;
    Ok(r.rows_affected())
}

pub async fn get_active_sessions_for_user(pool: &DbPool, user_id: Uuid) -> Result<Vec<SessionRow>, sqlx::Error> {
    sqlx::query_as::<_, SessionRow>("SELECT * FROM sessions WHERE user_id = $1 AND disconnected_at IS NULL")
        .bind(user_id).fetch_all(pool).await
}

pub async fn count_active_sessions(pool: &DbPool, app_id: Uuid) -> Result<i64, sqlx::Error> {
    let row: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM sessions WHERE app_id = $1 AND disconnected_at IS NULL")
        .bind(app_id).fetch_one(pool).await?;
    Ok(row.0)
}

// ── Channel Membership Queries ──

pub async fn add_channel_member(pool: &DbPool, m: &ChannelMembershipRow) -> Result<ChannelMembershipRow, sqlx::Error> {
    sqlx::query_as::<_, ChannelMembershipRow>(
        r#"INSERT INTO channel_memberships (id, channel_id, user_id, session_id, role, is_muted, is_server_muted, ssrc, joined_at)
           VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9) RETURNING *"#
    )
    .bind(m.id).bind(m.channel_id).bind(m.user_id).bind(m.session_id)
    .bind(&m.role).bind(m.is_muted).bind(m.is_server_muted)
    .bind(m.ssrc).bind(m.joined_at)
    .fetch_one(pool).await
}

pub async fn remove_channel_member(pool: &DbPool, channel_id: Uuid, session_id: Uuid) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE channel_memberships SET left_at = NOW() WHERE channel_id = $1 AND session_id = $2 AND left_at IS NULL")
        .bind(channel_id).bind(session_id)
        .execute(pool).await?;
    Ok(())
}

pub async fn close_session_memberships(pool: &DbPool, session_id: Uuid) -> Result<u64, sqlx::Error> {
    let r = sqlx::query("UPDATE channel_memberships SET left_at = NOW() WHERE session_id = $1 AND left_at IS NULL")
        .bind(session_id).execute(pool).await?;
    Ok(r.rows_affected())
}

/// Active members of a channel, verified to belong to `app_id` via the channels table.
pub async fn get_channel_members(pool: &DbPool, app_id: Uuid, channel_id: Uuid) -> Result<Vec<ChannelMembershipRow>, sqlx::Error> {
    sqlx::query_as::<_, ChannelMembershipRow>(
        r#"SELECT m.* FROM channel_memberships m
           JOIN channels c ON c.id = m.channel_id
           WHERE c.app_id = $1 AND m.channel_id = $2 AND m.left_at IS NULL"#
    )
    .bind(app_id).bind(channel_id).fetch_all(pool).await
}

pub async fn set_server_mute(pool: &DbPool, app_id: Uuid, channel_id: Uuid, user_id: Uuid, muted: bool) -> Result<u64, sqlx::Error> {
    let r = sqlx::query(
        r#"UPDATE channel_memberships m SET is_server_muted = $4
           FROM channels c
           WHERE c.id = m.channel_id AND c.app_id = $1 AND m.channel_id = $2 AND m.user_id = $3 AND m.left_at IS NULL"#
    )
    .bind(app_id).bind(channel_id).bind(user_id).bind(muted)
    .execute(pool).await?;
    Ok(r.rows_affected())
}

pub async fn get_user_channels(pool: &DbPool, user_id: Uuid) -> Result<Vec<ChannelMembershipRow>, sqlx::Error> {
    sqlx::query_as::<_, ChannelMembershipRow>(
        "SELECT * FROM channel_memberships WHERE user_id = $1 AND left_at IS NULL"
    )
    .bind(user_id).fetch_all(pool).await
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

pub async fn get_active_bans_for_user(pool: &DbPool, app_id: Uuid, user_id: Uuid) -> Result<Vec<BanRow>, sqlx::Error> {
    sqlx::query_as::<_, BanRow>(
        "SELECT * FROM bans WHERE app_id = $1 AND user_id = $2 AND revoked_at IS NULL AND (expires_at IS NULL OR expires_at > NOW())"
    )
    .bind(app_id).bind(user_id).fetch_all(pool).await
}

pub async fn get_ban(pool: &DbPool, app_id: Uuid, ban_id: Uuid) -> Result<Option<BanRow>, sqlx::Error> {
    sqlx::query_as::<_, BanRow>("SELECT * FROM bans WHERE app_id = $1 AND id = $2")
        .bind(app_id).bind(ban_id).fetch_optional(pool).await
}

pub async fn list_active_bans(pool: &DbPool, app_id: Uuid, limit: i64, offset: i64) -> Result<Vec<BanRow>, sqlx::Error> {
    sqlx::query_as::<_, BanRow>(
        "SELECT * FROM bans WHERE app_id = $1 AND revoked_at IS NULL AND (expires_at IS NULL OR expires_at > NOW()) ORDER BY created_at DESC LIMIT $2 OFFSET $3"
    )
    .bind(app_id).bind(limit).bind(offset).fetch_all(pool).await
}

pub async fn revoke_ban(pool: &DbPool, app_id: Uuid, ban_id: Uuid, revoked_by: Uuid) -> Result<u64, sqlx::Error> {
    let r = sqlx::query("UPDATE bans SET revoked_at = NOW(), revoked_by = $3 WHERE app_id = $1 AND id = $2 AND revoked_at IS NULL")
        .bind(app_id).bind(ban_id).bind(revoked_by).execute(pool).await?;
    Ok(r.rows_affected())
}

// ── Moderation Event Queries ──

pub async fn create_moderation_event(pool: &DbPool, ev: &ModerationEventRow) -> Result<ModerationEventRow, sqlx::Error> {
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

pub async fn list_moderation_events(pool: &DbPool, app_id: Uuid, status: Option<&str>, limit: i64, offset: i64) -> Result<Vec<ModerationEventRow>, sqlx::Error> {
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

pub async fn resolve_moderation_event(pool: &DbPool, app_id: Uuid, event_id: Uuid, moderator_id: Uuid, resolution: &str) -> Result<u64, sqlx::Error> {
    let r = sqlx::query("UPDATE moderation_events SET status = 'resolved', moderator_user_id = $3, resolution = $4, resolved_at = NOW() WHERE app_id = $1 AND id = $2 AND status <> 'resolved'")
        .bind(app_id).bind(event_id).bind(moderator_id).bind(resolution)
        .execute(pool).await?;
    Ok(r.rows_affected())
}

// ── Recording Queries ──

pub async fn create_recording(pool: &DbPool, rec: &RecordingRow) -> Result<RecordingRow, sqlx::Error> {
    sqlx::query_as::<_, RecordingRow>(
        r#"INSERT INTO recordings (id, app_id, channel_id, session_id, user_id, file_path, file_size_bytes, duration_secs, format, encrypted, encryption_key_id, started_at, expires_at, created_at)
           VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14) RETURNING *"#
    )
    .bind(rec.id).bind(rec.app_id).bind(rec.channel_id).bind(rec.session_id)
    .bind(rec.user_id).bind(&rec.file_path).bind(rec.file_size_bytes)
    .bind(rec.duration_secs).bind(&rec.format).bind(rec.encrypted)
    .bind(&rec.encryption_key_id).bind(rec.started_at).bind(rec.expires_at)
    .bind(rec.created_at)
    .fetch_one(pool).await
}

pub async fn get_recording(pool: &DbPool, app_id: Uuid, id: Uuid) -> Result<Option<RecordingRow>, sqlx::Error> {
    sqlx::query_as::<_, RecordingRow>("SELECT * FROM recordings WHERE app_id = $1 AND id = $2")
        .bind(app_id).bind(id).fetch_optional(pool).await
}

pub async fn list_recordings(pool: &DbPool, app_id: Uuid, channel_id: Option<Uuid>, limit: i64, offset: i64) -> Result<Vec<RecordingRow>, sqlx::Error> {
    sqlx::query_as::<_, RecordingRow>(
        "SELECT * FROM recordings WHERE app_id = $1 AND ($2::uuid IS NULL OR channel_id = $2) ORDER BY started_at DESC LIMIT $3 OFFSET $4"
    )
    .bind(app_id).bind(channel_id).bind(limit).bind(offset).fetch_all(pool).await
}

pub async fn finish_recording(pool: &DbPool, id: Uuid, size: i64, duration: f64) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE recordings SET ended_at = NOW(), file_size_bytes = $2, duration_secs = $3 WHERE id = $1")
        .bind(id).bind(size).bind(duration).execute(pool).await?;
    Ok(())
}

pub async fn delete_expired_recordings(pool: &DbPool) -> Result<u64, sqlx::Error> {
    let result = sqlx::query("DELETE FROM recordings WHERE expires_at < NOW()")
        .execute(pool).await?;
    Ok(result.rows_affected())
}

pub async fn list_expired_recordings(pool: &DbPool, limit: i64) -> Result<Vec<RecordingRow>, sqlx::Error> {
    sqlx::query_as::<_, RecordingRow>(
        "SELECT * FROM recordings WHERE expires_at < NOW() LIMIT $1"
    )
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

// ── Media Node Queries ──

pub async fn upsert_media_node(pool: &DbPool, node: &MediaNodeRow) -> Result<MediaNodeRow, sqlx::Error> {
    sqlx::query_as::<_, MediaNodeRow>(
        r#"INSERT INTO media_nodes (id, region, address, media_port, api_port, capacity, active_channels, active_participants, cpu_usage, memory_usage, bandwidth_in_mbps, bandwidth_out_mbps, healthy, version, last_heartbeat, registered_at)
           VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16)
           ON CONFLICT (id) DO UPDATE SET
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
    .fetch_one(pool).await
}

pub async fn get_healthy_media_nodes(pool: &DbPool, region: Option<&str>) -> Result<Vec<MediaNodeRow>, sqlx::Error> {
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
        .fetch_all(pool).await
}

pub async fn mark_node_unhealthy(pool: &DbPool, node_id: Uuid) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE media_nodes SET healthy = false WHERE id = $1")
        .bind(node_id).execute(pool).await?;
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

pub async fn list_audit_logs(pool: &DbPool, app_id: Option<Uuid>, limit: i64, offset: i64) -> Result<Vec<AuditLogRow>, sqlx::Error> {
    match app_id {
        Some(aid) => {
            sqlx::query_as::<_, AuditLogRow>(
                "SELECT * FROM audit_log WHERE app_id = $1 ORDER BY created_at DESC LIMIT $2 OFFSET $3"
            ).bind(aid).bind(limit).bind(offset).fetch_all(pool).await
        }
        None => {
            sqlx::query_as::<_, AuditLogRow>(
                "SELECT * FROM audit_log ORDER BY created_at DESC LIMIT $1 OFFSET $2"
            ).bind(limit).bind(offset).fetch_all(pool).await
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
pub async fn get_api_key_by_prefix(pool: &DbPool, prefix: &str) -> Result<Option<ApiKeyRow>, sqlx::Error> {
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
    sqlx::query_as::<_, ApiKeyRow>("SELECT * FROM api_keys WHERE app_id = $1 ORDER BY created_at DESC")
        .bind(app_id).fetch_all(pool).await
}

pub async fn touch_api_key(pool: &DbPool, key_id: Uuid) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE api_keys SET last_used_at = NOW() WHERE id = $1")
        .bind(key_id).execute(pool).await?;
    Ok(())
}

// ── Analytics Queries ──

pub async fn insert_analytics_snapshot(pool: &DbPool, snap: &AnalyticsSnapshotRow) -> Result<(), sqlx::Error> {
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

pub async fn get_analytics(pool: &DbPool, app_id: Uuid, from: DateTime<Utc>, to: DateTime<Utc>) -> Result<Vec<AnalyticsSnapshotRow>, sqlx::Error> {
    sqlx::query_as::<_, AnalyticsSnapshotRow>(
        "SELECT * FROM analytics_snapshots WHERE app_id = $1 AND timestamp >= $2 AND timestamp <= $3 ORDER BY timestamp ASC"
    )
    .bind(app_id).bind(from).bind(to).fetch_all(pool).await
}

// ── Admin User Queries ──

pub async fn create_admin_user(pool: &DbPool, admin: &AdminUserRow) -> Result<AdminUserRow, sqlx::Error> {
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
        .fetch_one(pool).await?;
    Ok(row.0)
}

pub async fn get_admin_by_id(pool: &DbPool, id: Uuid) -> Result<Option<AdminUserRow>, sqlx::Error> {
    sqlx::query_as::<_, AdminUserRow>("SELECT * FROM admin_users WHERE id = $1 AND active = true")
        .bind(id).fetch_optional(pool).await
}

pub async fn get_admin_by_email(pool: &DbPool, email: &str) -> Result<Option<AdminUserRow>, sqlx::Error> {
    sqlx::query_as::<_, AdminUserRow>("SELECT * FROM admin_users WHERE email = $1 AND active = true")
        .bind(email).fetch_optional(pool).await
}

pub async fn update_admin_login(pool: &DbPool, admin_id: Uuid) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE admin_users SET last_login_at = NOW() WHERE id = $1")
        .bind(admin_id).execute(pool).await?;
    Ok(())
}