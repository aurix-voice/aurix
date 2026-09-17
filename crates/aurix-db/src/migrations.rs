// Migrations are handled by sqlx::migrate! macro pointing to the migrations directory.
// This module provides programmatic migration utilities.

use crate::DbPool;

pub async fn check_migration_status(pool: &DbPool) -> Result<bool, sqlx::Error> {
    let row: Option<(i64,)> = sqlx::query_as(
        "SELECT COUNT(*) FROM _sqlx_migrations WHERE success = true"
    )
    .fetch_optional(pool)
    .await?;
    Ok(row.is_some())
}

pub async fn get_current_version(pool: &DbPool) -> Result<i64, sqlx::Error> {
    let row: Option<(i64,)> = sqlx::query_as(
        "SELECT MAX(version) FROM _sqlx_migrations WHERE success = true"
    )
    .fetch_optional(pool)
    .await?;
    Ok(row.map(|r| r.0).unwrap_or(0))
}