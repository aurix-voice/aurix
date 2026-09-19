use aurix_common::error::{AurixError, Result};
use aurix_common::types::*;
use aurix_db::models::ChannelRow;
use aurix_db::DbPool;
use chrono::Utc;
use uuid::Uuid;

pub struct ChannelManager {
    pool: DbPool,
}

impl ChannelManager {
    pub fn new(pool: DbPool) -> Self {
        Self { pool }
    }

    pub async fn create_channel(
        &self,
        app_id: AppId,
        name: &str,
        config: ChannelConfig,
    ) -> Result<ChannelRow> {
        let name = name.trim();
        if name.is_empty() || name.len() > 128 {
            return Err(AurixError::Validation(
                "Channel name must be 1..=128 characters".into(),
            ));
        }
        if config.max_participants == 0 {
            return Err(AurixError::Validation(
                "max_participants must be > 0".into(),
            ));
        }
        let row = ChannelRow {
            id: Uuid::now_v7(),
            app_id: app_id.0,
            name: name.to_string(),
            channel_type: format!("{:?}", config.channel_type).to_lowercase(),
            config: serde_json::to_value(&config)
                .map_err(|e| AurixError::Validation(format!("Invalid channel config: {e}")))?,
            max_participants: config.max_participants as i32,
            is_persistent: false,
            active_participants: 0,
            created_at: Utc::now(),
            updated_at: Utc::now(),
            deleted_at: None,
        };

        aurix_db::queries::create_channel(&self.pool, &row)
            .await
            .map_err(|e| AurixError::Database(format!("Failed to create channel: {e}")))
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
