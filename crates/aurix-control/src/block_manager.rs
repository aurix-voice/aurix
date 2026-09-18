//! Persistent cross-mute ("block list"). A block is mutual for audio: neither party hears the
//! other in any channel until the block is removed. Blocks survive sessions and are loaded into
//! the media session's receiver preferences at login.

use aurix_common::error::{AurixError, Result};
use aurix_common::types::*;
use aurix_db::DbPool;

/// Per-user cap; keeps the login-time load and the per-packet lookups bounded.
pub const MAX_BLOCKS_PER_USER: i64 = 1000;

pub struct BlockManager {
    pool: DbPool,
}

/// Blocks relevant to one user, as loaded at login.
#[derive(Debug, Default, Clone)]
pub struct UserBlocks {
    pub blocked: Vec<UserId>,
    pub blocked_by: Vec<UserId>,
}

impl BlockManager {
    pub fn new(pool: DbPool) -> Self {
        Self { pool }
    }

    /// Returns `true` if a new block was created, `false` if it already existed.
    pub async fn block(&self, app_id: AppId, user_id: UserId, target: UserId) -> Result<bool> {
        if user_id == target {
            return Err(AurixError::Validation("Cannot block yourself".into()));
        }
        let count = aurix_db::queries::count_user_blocks(&self.pool, app_id.0, user_id.0)
            .await
            .map_err(db)?;
        if count >= MAX_BLOCKS_PER_USER {
            return Err(AurixError::Validation(format!(
                "Block list is full ({MAX_BLOCKS_PER_USER})"
            )));
        }
        aurix_db::queries::add_user_block(&self.pool, app_id.0, user_id.0, target.0)
            .await
            .map_err(db)
    }

    pub async fn unblock(&self, app_id: AppId, user_id: UserId, target: UserId) -> Result<bool> {
        aurix_db::queries::remove_user_block(&self.pool, app_id.0, user_id.0, target.0)
            .await
            .map_err(db)
    }

    /// Removes every block placed by `user_id`; returns the users that were unblocked.
    pub async fn clear(&self, app_id: AppId, user_id: UserId) -> Result<Vec<UserId>> {
        Ok(
            aurix_db::queries::clear_user_blocks(&self.pool, app_id.0, user_id.0)
                .await
                .map_err(db)?
                .into_iter()
                .map(UserId)
                .collect(),
        )
    }

    pub async fn list(&self, app_id: AppId, user_id: UserId) -> Result<Vec<UserId>> {
        Ok(
            aurix_db::queries::list_user_blocks(&self.pool, app_id.0, user_id.0)
                .await
                .map_err(db)?
                .into_iter()
                .map(UserId)
                .collect(),
        )
    }

    pub async fn load_for_user(&self, app_id: AppId, user_id: UserId) -> Result<UserBlocks> {
        let blocked = self.list(app_id, user_id).await?;
        let blocked_by = aurix_db::queries::list_user_blocked_by(&self.pool, app_id.0, user_id.0)
            .await
            .map_err(db)?
            .into_iter()
            .map(UserId)
            .collect();
        Ok(UserBlocks {
            blocked,
            blocked_by,
        })
    }
}

fn db(e: impl std::fmt::Display) -> AurixError {
    AurixError::Database(format!("user_blocks: {e}"))
}
