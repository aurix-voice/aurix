use aurix_common::error::{AurixError, Result};
use aurix_common::types::*;
use aurix_db::models::{ChannelMembershipRow, ChannelRosterRow, LostMembershipRow, SessionRow};
use aurix_db::DbPool;
use chrono::Utc;
use uuid::Uuid;

pub struct SessionManager {
    pool: DbPool,
}

/// Outcome of [`SessionManager::recover_node_state`].
pub struct RecoveredState {
    pub sessions: u64,
    pub memberships: u64,
    /// Channels that lost their last participant to the cleanup; the caller publishes
    /// `ChannelDeactivated` for them once its event consumers are running.
    pub deactivated_channels: Vec<(AppId, ChannelId)>,
}

/// Outcome of [`SessionManager::reap_lost_node`].
pub struct ReapedState {
    pub sessions: u64,
    /// Every membership closed, for the `ParticipantLeft` announcements.
    pub memberships: Vec<LostMembershipRow>,
    pub deactivated_channels: Vec<(AppId, ChannelId)>,
}

/// Outcome of [`SessionManager::migrate_session`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MigratedSession {
    /// Node cleanup had already closed the session (and its memberships).
    pub reaped: bool,
    /// Channels whose membership rows are still open.
    pub open_channels: Vec<ChannelId>,
}

/// Where a session connects from, as recorded on its row.
#[derive(Debug, Clone, Copy)]
pub struct ClientInfo<'a> {
    pub ip_address: &'a str,
    pub user_agent: Option<&'a str>,
}

impl SessionManager {
    pub fn new(pool: DbPool) -> Self {
        Self { pool }
    }

    /// Insert the session row; a positive `max_concurrent` makes the insert conditional on the
    /// application's open-session count (`QuotaExceeded` otherwise).
    pub async fn create_session(
        &self,
        session_id: SessionId,
        user_id: UserId,
        app_id: AppId,
        media_node_id: MediaNodeId,
        client: ClientInfo<'_>,
        max_concurrent: i32,
    ) -> Result<SessionRow> {
        let row = SessionRow {
            id: session_id.0,
            user_id: user_id.0,
            app_id: app_id.0,
            media_node_id: media_node_id.0,
            ip_address: client.ip_address.to_string(),
            user_agent: client.user_agent.map(|s| s.to_string()),
            connected_at: Utc::now(),
            disconnected_at: None,
            disconnect_reason: None,
            quality_stats: None,
        };

        let db =
            |e: aurix_db::DbError| AurixError::Database(format!("Session creation failed: {e}"));
        if max_concurrent <= 0 {
            return aurix_db::queries::create_session(&self.pool, &row)
                .await
                .map_err(db);
        }
        match aurix_db::queries::create_session_within_limit(
            &self.pool,
            &row,
            i64::from(max_concurrent),
        )
        .await
        .map_err(db)?
        {
            Some(row) => Ok(row),
            None => {
                aurix_metrics::QUOTA_REJECTIONS
                    .with_label_values(&["concurrent_sessions"])
                    .inc();
                Err(AurixError::QuotaExceeded(format!(
                    "application is at its concurrent session limit ({max_concurrent})"
                )))
            }
        }
    }

    /// Closes the session row if it is still hosted by `node`. A row re-homed by a cross-node
    /// resume belongs to the new host and is left alone (returns `false`).
    pub async fn close_session(
        &self,
        session_id: SessionId,
        node: MediaNodeId,
        reason: &str,
        quality: Option<serde_json::Value>,
    ) -> Result<bool> {
        aurix_db::queries::close_session(&self.pool, session_id.0, node.0, reason, quality)
            .await
            .map(|n| n > 0)
            .map_err(|e| AurixError::Database(format!("Session close failed: {e}")))
    }

    pub async fn add_channel_membership(
        &self,
        channel_id: ChannelId,
        user_id: UserId,
        session_id: SessionId,
        role: ChannelRole,
        ssrc: u32,
        priority: bool,
    ) -> Result<ChannelMembershipRow> {
        let row = ChannelMembershipRow {
            id: Uuid::now_v7(),
            channel_id: channel_id.0,
            user_id: user_id.0,
            session_id: session_id.0,
            role: format!("{:?}", role).to_lowercase(),
            is_muted: false,
            is_server_muted: false,
            is_priority: priority,
            ssrc: ssrc as i64,
            joined_at: Utc::now(),
            left_at: None,
        };

        aurix_db::queries::add_channel_member(&self.pool, &row)
            .await
            .map_err(|e| AurixError::Database(format!("Membership add failed: {e}")))
    }

    pub async fn remove_channel_membership(
        &self,
        channel_id: ChannelId,
        session_id: SessionId,
    ) -> Result<()> {
        aurix_db::queries::remove_channel_member(&self.pool, channel_id.0, session_id.0)
            .await
            .map_err(|e| AurixError::Database(format!("Membership remove failed: {e}")))
    }

    /// Persists the priority-speaker flag on the user's open memberships in the channel.
    pub async fn set_membership_priority(
        &self,
        app_id: AppId,
        channel_id: ChannelId,
        user_id: UserId,
        priority: bool,
    ) -> Result<u64> {
        aurix_db::queries::set_membership_priority(
            &self.pool,
            app_id.0,
            channel_id.0,
            user_id.0,
            priority,
        )
        .await
        .map_err(|e| AurixError::Database(format!("Membership priority update failed: {e}")))
    }

    pub async fn remove_user_from_channel(
        &self,
        app_id: AppId,
        channel_id: ChannelId,
        user_id: UserId,
    ) -> Result<u64> {
        aurix_db::queries::remove_user_channel_memberships(
            &self.pool,
            app_id.0,
            channel_id.0,
            user_id.0,
        )
        .await
        .map_err(|e| AurixError::Database(format!("Membership remove failed: {e}")))
    }

    /// Mark every open membership of a session as left (disconnect cleanup) — only while the
    /// session is still hosted by `node`.
    pub async fn close_session_memberships(
        &self,
        session_id: SessionId,
        node: MediaNodeId,
    ) -> Result<u64> {
        aurix_db::queries::close_session_memberships(&self.pool, session_id.0, node.0)
            .await
            .map_err(|e| AurixError::Database(format!("Membership cleanup failed: {e}")))
    }

    pub async fn set_server_mute(
        &self,
        app_id: AppId,
        channel_id: ChannelId,
        user_id: UserId,
        muted: bool,
    ) -> Result<u64> {
        aurix_db::queries::set_server_mute(&self.pool, app_id.0, channel_id.0, user_id.0, muted)
            .await
            .map_err(|e| AurixError::Database(format!("Server mute failed: {e}")))
    }

    pub async fn get_session(
        &self,
        app_id: AppId,
        session_id: SessionId,
    ) -> Result<Option<SessionRow>> {
        aurix_db::queries::get_session(&self.pool, app_id.0, session_id.0)
            .await
            .map_err(|e| AurixError::Database(format!("Session lookup failed: {e}")))
    }

    /// Close sessions/memberships left open by a previous crash of this node.
    pub async fn recover_node_state(&self, media_node_id: MediaNodeId) -> Result<RecoveredState> {
        let mut channels =
            aurix_db::queries::close_stale_memberships_for_node(&self.pool, media_node_id.0)
                .await
                .map_err(|e| {
                    AurixError::Database(format!("Stale membership cleanup failed: {e}"))
                })?;
        let memberships = channels.len() as u64;
        channels.sort_unstable();
        channels.dedup();
        let emptied = aurix_db::queries::recount_channel_participants(&self.pool, &channels)
            .await
            .map_err(|e| AurixError::Database(format!("Participant recount failed: {e}")))?;
        let sessions = aurix_db::queries::close_stale_sessions_for_node(
            &self.pool,
            media_node_id.0,
            "node_restart",
        )
        .await
        .map_err(|e| AurixError::Database(format!("Stale session cleanup failed: {e}")))?;
        Ok(RecoveredState {
            sessions,
            memberships,
            deactivated_channels: emptied
                .into_iter()
                .map(|(app, ch)| (AppId::from_uuid(app), ChannelId::from_uuid(ch)))
                .collect(),
        })
    }

    pub async fn get_active_sessions_for_user(&self, user_id: UserId) -> Result<Vec<SessionRow>> {
        aurix_db::queries::get_active_sessions_for_user(&self.pool, user_id.0)
            .await
            .map_err(|e| AurixError::Database(format!("Session lookup failed: {e}")))
    }

    /// Closes what a node that stopped heartbeating still owned: memberships first (so
    /// counters and rosters settle), then the sessions, with reason `node_lost`. Sessions that
    /// were resumed on another node in the meantime point at that node and are left alone.
    pub async fn reap_lost_node(&self, media_node_id: MediaNodeId) -> Result<ReapedState> {
        let memberships =
            aurix_db::queries::close_memberships_for_lost_node(&self.pool, media_node_id.0)
                .await
                .map_err(|e| AurixError::Database(format!("Lost-node membership cleanup: {e}")))?;
        let mut channels: Vec<Uuid> = memberships.iter().map(|m| m.channel_id).collect();
        channels.sort_unstable();
        channels.dedup();
        let emptied = aurix_db::queries::recount_channel_participants(&self.pool, &channels)
            .await
            .map_err(|e| AurixError::Database(format!("Participant recount failed: {e}")))?;
        let sessions = aurix_db::queries::close_stale_sessions_for_node(
            &self.pool,
            media_node_id.0,
            "node_lost",
        )
        .await
        .map_err(|e| AurixError::Database(format!("Lost-node session cleanup: {e}")))?;
        Ok(ReapedState {
            sessions,
            memberships,
            deactivated_channels: emptied
                .into_iter()
                .map(|(app, ch)| (AppId::from_uuid(app), ChannelId::from_uuid(ch)))
                .collect(),
        })
    }

    /// Re-homes a session on this node (cross-node resume). `None` when no such session
    /// exists for the tenant/user or it was closed for a reason other than node loss.
    pub async fn migrate_session(
        &self,
        session_id: SessionId,
        app_id: AppId,
        user_id: UserId,
        to_node: MediaNodeId,
    ) -> Result<Option<MigratedSession>> {
        let moved = aurix_db::queries::migrate_session(
            &self.pool,
            session_id.0,
            app_id.0,
            user_id.0,
            to_node.0,
        )
        .await
        .map_err(|e| AurixError::Database(format!("Session migration failed: {e}")))?;
        Ok(moved.map(|m| MigratedSession {
            reaped: m.reaped,
            open_channels: m
                .open_channels
                .into_iter()
                .map(ChannelId::from_uuid)
                .collect(),
        }))
    }

    pub async fn get_channel_members(
        &self,
        app_id: AppId,
        channel_id: ChannelId,
    ) -> Result<Vec<ChannelMembershipRow>> {
        aurix_db::queries::get_channel_members(&self.pool, app_id.0, channel_id.0)
            .await
            .map_err(|e| AurixError::Database(format!("Member list failed: {e}")))
    }

    /// Members with display name and hosting node (see `queries::get_channel_roster`).
    pub async fn get_channel_roster(
        &self,
        app_id: AppId,
        channel_id: ChannelId,
    ) -> Result<Vec<ChannelRosterRow>> {
        aurix_db::queries::get_channel_roster(&self.pool, app_id.0, channel_id.0)
            .await
            .map_err(|e| AurixError::Database(format!("Roster failed: {e}")))
    }

    pub async fn count_active_sessions(&self, app_id: AppId) -> Result<i64> {
        aurix_db::queries::count_active_sessions(&self.pool, app_id.0)
            .await
            .map_err(|e| AurixError::Database(format!("Count failed: {e}")))
    }
}
