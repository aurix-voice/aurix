use aurix_common::error::{AurixError, Result};
use aurix_common::types::*;
use aurix_db::models::ChannelRow;
use aurix_db::DbPool;
use chrono::Utc;
use uuid::Uuid;

pub struct ChannelManager {
    pool: DbPool,
}

/// Outcome of resolving a join against an ad-hoc grant.
pub struct AdHocResolution {
    pub config: ChannelConfig,
    /// The channel is ad-hoc (created by a join grant, released when empty).
    pub ad_hoc: bool,
    /// True when this call created (or revived) the channel: the caller announces
    /// `channel.created`.
    pub created: bool,
}

impl ChannelManager {
    pub fn new(pool: DbPool) -> Self {
        Self { pool }
    }

    fn validate_name(name: &str) -> Result<&str> {
        let name = name.trim();
        if name.is_empty() || name.len() > 128 {
            return Err(AurixError::Validation(
                "Channel name must be 1..=128 characters".into(),
            ));
        }
        Ok(name)
    }

    fn row_for(app_id: AppId, id: Uuid, name: &str, config: &ChannelConfig) -> Result<ChannelRow> {
        if config.max_participants == 0 {
            return Err(AurixError::Validation(
                "max_participants must be > 0".into(),
            ));
        }
        Ok(ChannelRow {
            id,
            app_id: app_id.0,
            name: name.to_string(),
            channel_type: format!("{:?}", config.channel_type).to_lowercase(),
            config: serde_json::to_value(config)
                .map_err(|e| AurixError::Validation(format!("Invalid channel config: {e}")))?,
            max_participants: config.max_participants as i32,
            is_persistent: false,
            ad_hoc: false,
            active_participants: 0,
            created_at: Utc::now(),
            updated_at: Utc::now(),
            deleted_at: None,
        })
    }

    pub async fn create_channel(
        &self,
        app_id: AppId,
        name: &str,
        config: ChannelConfig,
    ) -> Result<ChannelRow> {
        let name = Self::validate_name(name)?;
        let row = Self::row_for(app_id, Uuid::now_v7(), name, &config)?;
        aurix_db::queries::create_channel(&self.pool, &row)
            .await
            .map_err(|e| AurixError::Database(format!("Failed to create channel: {e}")))
    }

    /// Validates an ad-hoc template against the app's limits (channel quota, participants per
    /// channel) and clamps `max_participants`. Used when issuing tokens so a bad grant fails at
    /// the game server, not at the player's join.
    pub fn validate_ad_hoc(
        template: &AdHocChannel,
        max_participants_per_channel: u32,
    ) -> Result<ChannelConfig> {
        Self::validate_name(&template.name)?;
        let mut config = template.channel_config();
        if config.max_participants == 0 {
            return Err(AurixError::Validation(
                "max_participants must be > 0".into(),
            ));
        }
        if config.max_participants > max_participants_per_channel {
            config.max_participants = max_participants_per_channel;
        }
        Ok(config)
    }

    /// Loads the channel a join targets, creating it from the grant's ad-hoc template when it
    /// does not exist. The id must be the one derived from the template name, so a token cannot
    /// re-create a channel under a foreign id. Enforces the app's channel quota on creation.
    pub async fn resolve_join(
        &self,
        app_id: AppId,
        channel_id: ChannelId,
        ad_hoc: Option<&AdHocChannel>,
    ) -> Result<AdHocResolution> {
        if let Some(row) = self.get_channel(app_id, channel_id).await? {
            return Ok(AdHocResolution {
                config: Self::config_from_row(&row),
                ad_hoc: row.ad_hoc,
                created: false,
            });
        }
        let template = match ad_hoc {
            Some(t) if t.channel_id(app_id) == channel_id => t,
            _ => return Err(AurixError::ChannelNotFound(channel_id.to_string())),
        };
        let app = aurix_db::queries::get_app(&self.pool, app_id.0)
            .await
            .map_err(|e| AurixError::Database(format!("Failed to load app: {e}")))?
            .ok_or_else(|| AurixError::AuthorizationDenied("Application is inactive".into()))?;
        let config = Self::validate_ad_hoc(template, app.max_participants_per_channel as u32)?;
        let count = self.count_channels(app_id).await?;
        if count >= app.max_channels as i64 {
            return Err(AurixError::Conflict(
                "Channel quota reached for this application".into(),
            ));
        }
        let name = Self::validate_name(&template.name)?;
        let row = Self::row_for(app_id, channel_id.0, name, &config)?;
        let (row, created) = aurix_db::queries::ensure_ad_hoc_channel(&self.pool, &row)
            .await
            .map_err(|e| AurixError::Database(format!("Failed to create ad-hoc channel: {e}")))?;
        Ok(AdHocResolution {
            config: Self::config_from_row(&row),
            ad_hoc: true,
            created,
        })
    }

    /// After a join into an ad-hoc channel was persisted: undo a release that raced with it
    /// (the releasing node saw the channel empty an instant before this participant was
    /// recorded). Returns true when the row was revived (announce `channel.created` again).
    pub async fn revive_if_released(&self, app_id: AppId, channel_id: ChannelId) -> Result<bool> {
        aurix_db::queries::revive_ad_hoc_channel(&self.pool, app_id.0, channel_id.0)
            .await
            .map_err(|e| AurixError::Database(format!("Failed to revive channel: {e}")))
    }

    /// Called when a channel's participant count dropped to zero. Ad-hoc channels are
    /// soft-deleted; returns true when this call removed the row (announce `channel.destroyed`).
    pub async fn release_if_ad_hoc(&self, app_id: AppId, channel_id: ChannelId) -> Result<bool> {
        aurix_db::queries::delete_empty_ad_hoc_channel(&self.pool, app_id.0, channel_id.0)
            .await
            .map_err(|e| AurixError::Database(format!("Failed to release channel: {e}")))
    }

    /// Tenant-scoped lookup: returns `None` for channels of other apps.
    pub async fn get_channel(
        &self,
        app_id: AppId,
        channel_id: ChannelId,
    ) -> Result<Option<ChannelRow>> {
        aurix_db::queries::get_channel(&self.pool, app_id.0, channel_id.0)
            .await
            .map_err(|e| AurixError::Database(format!("Failed to get channel: {e}")))
    }

    /// Like `get_channel` but converts a miss into `ChannelNotFound`.
    pub async fn require_channel(
        &self,
        app_id: AppId,
        channel_id: ChannelId,
    ) -> Result<ChannelRow> {
        self.get_channel(app_id, channel_id)
            .await?
            .ok_or_else(|| AurixError::ChannelNotFound(channel_id.to_string()))
    }

    /// Persisted channel configuration (falls back to defaults for legacy rows with
    /// malformed JSON, but always applies the row's `max_participants`).
    pub async fn load_channel_config(
        &self,
        app_id: AppId,
        channel_id: ChannelId,
    ) -> Result<ChannelConfig> {
        let row = self.require_channel(app_id, channel_id).await?;
        Ok(Self::config_from_row(&row))
    }

    pub fn config_from_row(row: &ChannelRow) -> ChannelConfig {
        let mut config: ChannelConfig =
            serde_json::from_value(row.config.clone()).unwrap_or_default();
        if row.max_participants > 0 {
            config.max_participants = row.max_participants as u32;
        }
        config
    }

    pub async fn update_channel_config(
        &self,
        app_id: AppId,
        channel_id: ChannelId,
        config: &ChannelConfig,
    ) -> Result<()> {
        let json = serde_json::to_value(config)
            .map_err(|e| AurixError::Validation(format!("Invalid channel config: {e}")))?;
        let affected = aurix_db::queries::update_channel_config(
            &self.pool,
            app_id.0,
            channel_id.0,
            &json,
            config.max_participants as i32,
        )
        .await
        .map_err(|e| AurixError::Database(format!("Failed to update channel: {e}")))?;
        if affected == 0 {
            return Err(AurixError::ChannelNotFound(channel_id.to_string()));
        }
        Ok(())
    }

    pub async fn list_channels(
        &self,
        app_id: AppId,
        limit: i64,
        offset: i64,
    ) -> Result<Vec<ChannelRow>> {
        aurix_db::queries::list_channels(&self.pool, app_id.0, limit, offset)
            .await
            .map_err(|e| AurixError::Database(format!("Failed to list channels: {e}")))
    }

    pub async fn list_active_channels(
        &self,
        app_id: AppId,
        limit: i64,
        offset: i64,
    ) -> Result<Vec<ChannelRow>> {
        aurix_db::queries::list_active_channels(&self.pool, app_id.0, limit, offset)
            .await
            .map_err(|e| AurixError::Database(format!("Failed to list active channels: {e}")))
    }

    pub async fn delete_channel(&self, app_id: AppId, channel_id: ChannelId) -> Result<()> {
        let affected = aurix_db::queries::delete_channel(&self.pool, app_id.0, channel_id.0)
            .await
            .map_err(|e| AurixError::Database(format!("Failed to delete channel: {e}")))?;
        if affected == 0 {
            return Err(AurixError::ChannelNotFound(channel_id.to_string()));
        }
        Ok(())
    }

    /// Returns the channel's participant count after the change.
    pub async fn update_participant_count(&self, channel_id: ChannelId, delta: i32) -> Result<i32> {
        aurix_db::queries::update_channel_participant_count(&self.pool, channel_id.0, delta)
            .await
            .map_err(|e| AurixError::Database(format!("Failed to update count: {e}")))
    }

    pub async fn count_channels(&self, app_id: AppId) -> Result<i64> {
        aurix_db::queries::count_channels(&self.pool, app_id.0)
            .await
            .map_err(|e| AurixError::Database(format!("Failed to count channels: {e}")))
    }

    pub async fn count_active_channels(&self, app_id: AppId) -> Result<i64> {
        aurix_db::queries::count_active_channels(&self.pool, app_id.0)
            .await
            .map_err(|e| AurixError::Database(format!("Failed to count active: {e}")))
    }
}
