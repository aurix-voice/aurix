// Migrations are handled by sqlx::migrate! macro pointing to the migrations directory.
// This module provides programmatic migration utilities.

use crate::DbPool;

pub static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("../../migrations");

/// True when every embedded migration has been applied successfully.
pub async fn check_migration_status(pool: &DbPool) -> Result<bool, sqlx::Error> {
    let applied = applied_migration_count(pool).await?;
    Ok(applied >= MIGRATOR.iter().count() as i64)
}

pub async fn applied_migration_count(pool: &DbPool) -> Result<i64, sqlx::Error> {
    let exists: (bool,) = sqlx::query_as("SELECT to_regclass('_sqlx_migrations') IS NOT NULL")
        .fetch_one(pool)
        .await?;
    if !exists.0 {
        return Ok(0);
    }
    let row: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM _sqlx_migrations WHERE success = true")
        .fetch_one(pool)
        .await?;
    Ok(row.0)
}

pub async fn get_current_version(pool: &DbPool) -> Result<i64, sqlx::Error> {
    if applied_migration_count(pool).await? == 0 {
        return Ok(0);
    }
    let row: (Option<i64>,) =
        sqlx::query_as("SELECT MAX(version) FROM _sqlx_migrations WHERE success = true")
            .fetch_one(pool)
            .await?;
    Ok(row.0.unwrap_or(0))
}
