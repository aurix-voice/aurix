use aurix_common::error::{AurixError, Result};
use aurix_common::types::*;
use aurix_db::models::{ChannelMembershipRow, SessionRow};
use aurix_db::DbPool;
use chrono::Utc;
use uuid::Uuid;

pub struct SessionManager {
    pool: DbPool,
}

impl SessionManager {
    pub fn new(pool: DbPool) -> Self {
        Self { pool }
    }

    pub async fn create_session(
        &self,
        session_id: SessionId,
        user_id: UserId,
        app_id: AppId,
        media_node_id: MediaNodeId,
        ip_address: &str,
        user_agent: Option<&str>,
    ) -> Result<SessionRow> {
        let row = SessionRow {
            id: session_id.0,
            user_id: user_id.0,
            app_id: app_id.0,
            media_node_id: media_node_id.0,
            ip_address: ip_address.to_string(),
            user_agent: user_agent.map(|s| s.to_string()),
            connected_at: Utc::now(),
            disconnected_at: None,
            disconnect_reason: None,
            quality_stats: None,
        };

        aurix_db::queries::create_session(&self.pool, &row)
            .await
            .map_err(|e| AurixError::Database(format!("Session creation failed: {e}")))
    }

    pub async fn close_session(
        &self,
        session_id: SessionId,
        reason: &str,
        quality: Option<serde_json::Value>,
    ) -> Result<()> {
        aurix_db::queries::close_session(&self.pool, session_id.0, reason, quality)
            .await
            .map_err(|e| AurixError::Database(format!("Session close failed: {e}")))
    }

    pub async fn add_channel_membership(
        &self,
        channel_id: ChannelId,
        user_id: UserId,
        session_id: SessionId,
        role: ChannelRole,
        ssrc: u32,
    ) -> Result<ChannelMembershipRow> {
        let row = ChannelMembershipRow {
            id: Uuid::now_v7(),
            channel_id: channel_id.0,
            user_id: user_id.0,
            session_id: session_id.0,
            role: format!("{:?}", role).to_lowercase(),
            is_muted: false,
            is_server_muted: false,
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

    /// Mark every open membership of a session as left (disconnect cleanup).
    pub async fn close_session_memberships(&self, session_id: SessionId) -> Result<u64> {
        aurix_db::queries::close_session_memberships(&self.pool, session_id.0)
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
    pub async fn recover_node_state(&self, media_node_id: MediaNodeId) -> Result<(u64, u64)> {
        let m = aurix_db::queries::close_stale_memberships_for_node(&self.pool, media_node_id.0)
            .await
            .map_err(|e| AurixError::Database(format!("Stale membership cleanup failed: {e}")))?;
        let s = aurix_db::queries::close_stale_sessions_for_node(
            &self.pool,
            media_node_id.0,
            "node_restart",
        )
        .await
        .map_err(|e| AurixError::Database(format!("Stale session cleanup failed: {e}")))?;
        Ok((s, m))
    }

    pub async fn get_active_sessions_for_user(&self, user_id: UserId) -> Result<Vec<SessionRow>> {
        aurix_db::queries::get_active_sessions_for_user(&self.pool, user_id.0)
            .await
            .map_err(|e| AurixError::Database(format!("Session lookup failed: {e}")))
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

    pub async fn count_active_sessions(&self, app_id: AppId) -> Result<i64> {
        aurix_db::queries::count_active_sessions(&self.pool, app_id.0)
            .await
            .map_err(|e| AurixError::Database(format!("Count failed: {e}")))
    }
}
