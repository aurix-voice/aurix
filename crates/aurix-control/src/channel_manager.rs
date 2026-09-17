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
        let row = ChannelRow {
            id: Uuid::now_v7(),
            app_id: app_id.0,
            name: name.to_string(),
            channel_type: format!("{:?}", config.channel_type).to_lowercase(),
            config: serde_json::to_value(&config).unwrap_or_default(),
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

    pub async fn get_channel(&self, channel_id: ChannelId) -> Result<Option<ChannelRow>> {
        aurix_db::queries::get_channel(&self.pool, channel_id.0)
            .await
            .map_err(|e| AurixError::Database(format!("Failed to get channel: {e}")))
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

    pub async fn delete_channel(&self, channel_id: ChannelId) -> Result<()> {
        aurix_db::queries::delete_channel(&self.pool, channel_id.0)
            .await
            .map_err(|e| AurixError::Database(format!("Failed to delete channel: {e}")))
    }

    pub async fn update_participant_count(
        &self,
        channel_id: ChannelId,
        delta: i32,
    ) -> Result<()> {
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