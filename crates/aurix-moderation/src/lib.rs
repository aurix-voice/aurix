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

    /// Issues a ban. `device` / `ip_address` scopes also match later accounts seen from the same
    /// device or address; when the matching field is omitted it is taken from what the server
    /// has seen of the user — every device they connected with (one ban row per device) or the
    /// address of their most recent session — and the call fails when nothing is known yet.
    #[allow(clippy::too_many_arguments)]
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
        let scope_name = scope_name(scope);

        let mut targets: Vec<(Option<String>, Option<String>)> = match scope {
            BanScope::Account => vec![(None, None)],
            BanScope::Device => match device_id {
                Some(d) => vec![(Some(d.to_string()), None)],
                None => {
                    let user = aurix_db::queries::get_user(&self.pool, app_id.0, user_id.0)
                        .await
                        .map_err(|e| AurixError::Database(format!("User lookup failed: {e}")))?
                        .ok_or_else(|| AurixError::UserNotFound(user_id.to_string()))?;
                    if user.device_ids.is_empty() {
                        return Err(AurixError::Validation(
                            "device scope: no device is known for this user yet; pass device_id"
                                .into(),
                        ));
                    }
                    user.device_ids
                        .into_iter()
                        .map(|d| (Some(d), None))
                        .collect()
                }
            },
            BanScope::IpAddress => {
                let ip = match ip_address {
                    Some(ip) => ip.to_string(),
                    None => aurix_db::queries::latest_session_ip_for_user(
                        &self.pool, app_id.0, user_id.0,
                    )
                    .await
                    .map_err(|e| AurixError::Database(format!("Session lookup failed: {e}")))?
                    .ok_or_else(|| {
                        AurixError::Validation(
                            "ip_address scope: the user has never connected; pass ip_address"
                                .into(),
                        )
                    })?,
                };
                vec![(None, Some(ip))]
            }
        };

        let mut first: Option<BanRow> = None;
        for (device_id, ip_address) in targets.drain(..) {
            let ban = BanRow {
                id: Uuid::now_v7(),
                app_id: app_id.0,
                user_id: Some(user_id.0),
                device_id,
                ip_address,
                scope: scope_name.to_string(),
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
            first.get_or_insert(created);
        }

        // Also update user record
        aurix_db::queries::ban_user(&self.pool, app_id.0, user_id.0, reason, expires_at)
            .await
            .map_err(|e| AurixError::Database(format!("User ban update failed: {e}")))?;

        Ok(first.expect("at least one ban target"))
    }

    /// Revoke one ban. The ban must belong to `app_id`; the user's `is_banned` flag is cleared
    /// only when no other active ban remains for them.
    pub async fn unban_user(
        &self,
        app_id: AppId,
        ban_id: Uuid,
        revoked_by: UserId,
    ) -> Result<UserId> {
        let ban = aurix_db::queries::get_ban(&self.pool, app_id.0, ban_id)
            .await
            .map_err(|e| AurixError::Database(format!("Ban lookup failed: {e}")))?
            .ok_or_else(|| AurixError::Moderation("Ban not found".into()))?;
        let affected = aurix_db::queries::revoke_ban(&self.pool, app_id.0, ban_id, revoked_by.0)
            .await
            .map_err(|e| AurixError::Database(format!("Ban revoke failed: {e}")))?;
        if affected == 0 {
            return Err(AurixError::Conflict("Ban already revoked".into()));
        }
        let Some(user_id) = ban.user_id else {
            return Err(AurixError::Moderation("Ban has no user target".into()));
        };
        let remaining = aurix_db::queries::get_active_bans_for_user(&self.pool, app_id.0, user_id)
            .await
            .map_err(|e| AurixError::Database(format!("Ban check failed: {e}")))?;
        if remaining.is_empty() {
            aurix_db::queries::unban_user(&self.pool, app_id.0, user_id)
                .await
                .map_err(|e| AurixError::Database(format!("User unban failed: {e}")))?;
        }
        Ok(UserId(user_id))
    }

    /// Revoke every active ban for a user within an app.
    pub async fn unban_user_all(
        &self,
        app_id: AppId,
        user_id: UserId,
        revoked_by: UserId,
    ) -> Result<usize> {
        let bans = aurix_db::queries::get_active_bans_for_user(&self.pool, app_id.0, user_id.0)
            .await
            .map_err(|e| AurixError::Database(format!("Ban check failed: {e}")))?;
        for ban in &bans {
            aurix_db::queries::revoke_ban(&self.pool, app_id.0, ban.id, revoked_by.0)
                .await
                .map_err(|e| AurixError::Database(format!("Ban revoke failed: {e}")))?;
        }
        aurix_db::queries::unban_user(&self.pool, app_id.0, user_id.0)
            .await
            .map_err(|e| AurixError::Database(format!("User unban failed: {e}")))?;
        Ok(bans.len())
    }

    pub async fn list_bans(
        &self,
        app_id: AppId,
        user_id: Option<UserId>,
        limit: i64,
        offset: i64,
    ) -> Result<Vec<BanRow>> {
        aurix_db::queries::list_bans(&self.pool, app_id.0, user_id.map(|u| u.0), limit, offset)
            .await
            .map_err(|e| AurixError::Database(format!("Ban list failed: {e}")))
    }

    /// Whether a connection from `device_id` / `ip` as `user_id` is refused: the user's own
    /// bans, or a `device` / `ip_address` ban issued against another account from the same
    /// device or address.
    pub async fn is_banned_for(
        &self,
        app_id: AppId,
        user_id: UserId,
        device_id: Option<&str>,
        ip: Option<&str>,
    ) -> Result<bool> {
        let bans = aurix_db::queries::get_active_bans_matching(
            &self.pool, app_id.0, user_id.0, device_id, ip,
        )
        .await
        .map_err(|e| AurixError::Database(format!("Ban check failed: {e}")))?;
        Ok(!bans.is_empty())
    }

    pub async fn is_banned(&self, app_id: AppId, user_id: UserId) -> Result<bool> {
        let bans = aurix_db::queries::get_active_bans_for_user(&self.pool, app_id.0, user_id.0)
            .await
            .map_err(|e| AurixError::Database(format!("Ban check failed: {e}")))?;
        Ok(!bans.is_empty())
    }

    #[allow(clippy::too_many_arguments)]
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
                    match client
                        .post(&url)
                        .json(&payload)
                        .timeout(std::time::Duration::from_secs(10))
                        .send()
                        .await
                    {
                        Ok(resp) if resp.status().is_success() => {
                            tracing::info!("Webhook delivered on attempt {}", attempt + 1);
                            return;
                        }
                        Ok(resp) => {
                            tracing::warn!(
                                "Webhook returned {} on attempt {}",
                                resp.status(),
                                attempt + 1
                            );
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
                tracing::error!(
                    "Webhook delivery failed after 4 attempts for event {}",
                    payload["event_id"]
                );
            });
        }

        Ok(created)
    }

    pub async fn resolve_event(
        &self,
        app_id: AppId,
        event_id: Uuid,
        moderator_id: UserId,
        resolution: &str,
    ) -> Result<()> {
        let affected = aurix_db::queries::resolve_moderation_event(
            &self.pool,
            app_id.0,
            event_id,
            moderator_id.0,
            resolution,
        )
        .await
        .map_err(|e| AurixError::Database(format!("Event resolution failed: {e}")))?;
        if affected == 0 {
            return Err(AurixError::Moderation(
                "Moderation event not found or already resolved".into(),
            ));
        }
        Ok(())
    }

    pub async fn get_event(
        &self,
        app_id: AppId,
        event_id: Uuid,
    ) -> Result<Option<ModerationEventRow>> {
        aurix_db::queries::get_moderation_event(&self.pool, app_id.0, event_id)
            .await
            .map_err(|e| AurixError::Database(format!("Event lookup failed: {e}")))
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

/// Wire name of a ban scope (`account` / `device` / `ip_address`), as stored in `bans.scope`.
pub fn scope_name(scope: BanScope) -> &'static str {
    match scope {
        BanScope::Account => "account",
        BanScope::Device => "device",
        BanScope::IpAddress => "ip_address",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scope_names_match_the_wire_format() {
        for s in [BanScope::Account, BanScope::Device, BanScope::IpAddress] {
            let wire = serde_json::to_value(s).unwrap();
            assert_eq!(wire.as_str().unwrap(), scope_name(s));
        }
    }
}
