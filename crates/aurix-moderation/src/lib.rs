use aurix_common::error::{AurixError, Result};
use aurix_common::types::*;
use aurix_db::models::{BanRow, ModerationEventRow};
use aurix_db::DbPool;
use chrono::Utc;
use uuid::Uuid;

pub struct ModerationService {
    pool: DbPool,
    content_webhook: Option<String>,
}

impl ModerationService {
    pub fn new(pool: DbPool, content_webhook: Option<String>) -> Self {
        Self {
            pool,
            content_webhook,
        }
    }

    pub async fn ban_user(
        &self,
        app_id: AppId,
        user_id: UserId,
        scope: BanScope,
        reason: &str,
        issued_by: UserId,
        duration: Option<chrono::Duration>,
        device_id: Option<&str>,
        ip_address: Option<&str>,
    ) -> Result<BanRow> {
        let expires_at = duration.map(|d| Utc::now() + d);

        let ban = BanRow {
            id: Uuid::now_v7(),
            app_id: app_id.0,
            user_id: Some(user_id.0),
            device_id: device_id.map(|s| s.to_string()),
            ip_address: ip_address.map(|s| s.to_string()),
            scope: format!("{:?}", scope).to_lowercase(),
            reason: reason.to_string(),
            issued_by: issued_by.0,
            expires_at,
            created_at: Utc::now(),
            revoked_at: None,
            revoked_by: None,
        };

        let created = aurix_db::queries::create_ban(&self.pool, &ban)
            .await
            .map_err(|e| AurixError::Database(format!("Ban creation failed: {e}")))?;

        // Also update user record
        aurix_db::queries::ban_user(&self.pool, user_id.0, reason, expires_at)
            .await
            .map_err(|e| AurixError::Database(format!("User ban update failed: {e}")))?;

        Ok(created)
    }

    pub async fn unban_user(
        &self,
        ban_id: Uuid,
        user_id: UserId,
        revoked_by: UserId,
    ) -> Result<()> {
        aurix_db::queries::revoke_ban(&self.pool, ban_id, revoked_by.0)
            .await
            .map_err(|e| AurixError::Database(format!("Ban revoke failed: {e}")))?;

        // Check if user has any remaining active bans
        let remaining = aurix_db::queries::get_active_bans_for_user(
            &self.pool,
            Uuid::nil(), // We'd need app_id here
            user_id.0,
        )
        .await
        .unwrap_or_default();

        if remaining.is_empty() {
            aurix_db::queries::unban_user(&self.pool, user_id.0)
                .await
                .map_err(|e| AurixError::Database(format!("User unban failed: {e}")))?;
        }

        Ok(())
    }

    pub async fn is_banned(&self, app_id: AppId, user_id: UserId) -> Result<bool> {
        let bans = aurix_db::queries::get_active_bans_for_user(&self.pool, app_id.0, user_id.0)
            .await
            .map_err(|e| AurixError::Database(format!("Ban check failed: {e}")))?;
        Ok(!bans.is_empty())
    }

    pub async fn report_user(
        &self,
        app_id: AppId,
        channel_id: Option<ChannelId>,
        target_user_id: UserId,
        reporter_user_id: UserId,
        reason: &str,
        evidence: Option<serde_json::Value>,
        recording_id: Option<Uuid>,
    ) -> Result<ModerationEventRow> {
        let event = ModerationEventRow {
            id: Uuid::now_v7(),
            app_id: app_id.0,
            channel_id: channel_id.map(|c| c.0),
            target_user_id: target_user_id.0,
            reporter_user_id: Some(reporter_user_id.0),
            moderator_user_id: None,
            event_type: "report".to_string(),
            reason: reason.to_string(),
            evidence,
            recording_id,
            status: "pending".to_string(),
            resolution: None,
            created_at: Utc::now(),
            resolved_at: None,
        };

        let created = aurix_db::queries::create_moderation_event(&self.pool, &event)
            .await
            .map_err(|e| AurixError::Database(format!("Report creation failed: {e}")))?;

        // Notify content analysis webhook if configured
        if let Some(ref webhook_url) = self.content_webhook {
            let payload = serde_json::json!({
                "event_id": created.id,
                "app_id": app_id.0,
                "target_user_id": target_user_id.0,
                "reporter_user_id": reporter_user_id.0,
                "reason": reason,
            });
            let url = webhook_url.clone();
            tokio::spawn(async move {
                let client = reqwest::Client::new();
                let mut delay = std::time::Duration::from_secs(1);
                for attempt in 0..4u32 {
                    match client.post(&url).json(&payload).timeout(std::time::Duration::from_secs(10)).send().await {
                        Ok(resp) if resp.status().is_success() => {
                            tracing::info!("Webhook delivered on attempt {}", attempt + 1);
                            return;
                        }
                        Ok(resp) => {
                            tracing::warn!("Webhook returned {} on attempt {}", resp.status(), attempt + 1);
                        }
                        Err(e) => {
                            tracing::warn!("Webhook failed on attempt {}: {}", attempt + 1, e);
                        }
                    }
                    if attempt < 3 {
                        tokio::time::sleep(delay).await;
                        delay *= 2; // exponential backoff: 1s, 2s, 4s
                    }
                }
                tracing::error!("Webhook delivery failed after 4 attempts for event {}", payload["event_id"]);
            });
        }

        Ok(created)
    }

    pub async fn resolve_event(
        &self,
        event_id: Uuid,
        moderator_id: UserId,
        resolution: &str,
    ) -> Result<()> {
        aurix_db::queries::resolve_moderation_event(
            &self.pool,
            event_id,
            moderator_id.0,
            resolution,
        )
        .await
        .map_err(|e| AurixError::Database(format!("Event resolution failed: {e}")))
    }

    pub async fn list_events(
        &self,
        app_id: AppId,
        status: Option<&str>,
        limit: i64,
        offset: i64,
    ) -> Result<Vec<ModerationEventRow>> {
        aurix_db::queries::list_moderation_events(&self.pool, app_id.0, status, limit, offset)
            .await
            .map_err(|e| AurixError::Database(format!("Event list failed: {e}")))
    }
}