// Migrations are handled by sqlx::migrate! macro pointing to the migrations directory.
// This module provides programmatic migration utilities.

use crate::DbPool;

pub static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("../../migrations");

/// Applied-vs-embedded comparison for operators (`aurix doctor`): what the node will do at
/// start-up and what would make `MIGRATOR.run` refuse.
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct MigrationReport {
    /// Versions applied successfully, ascending.
    pub applied: Vec<i64>,
    /// Embedded migrations without an applied row: `(version, description)`, ascending.
    pub pending: Vec<(i64, String)>,
    /// Applied versions whose stored checksum differs from the embedded file (the file was
    /// edited after it ran; sqlx refuses to continue).
    pub checksum_mismatch: Vec<i64>,
    /// Rows recorded with `success = false` (an interrupted migration; needs manual repair).
    pub failed: Vec<i64>,
    /// Applied versions unknown to this binary (the database was migrated by a newer node).
    pub unknown: Vec<i64>,
}

impl MigrationReport {
    /// Highest embedded version.
    pub fn embedded_latest() -> i64 {
        MIGRATOR.iter().map(|m| m.version).max().unwrap_or(0)
    }

    pub fn is_current(&self) -> bool {
        self.pending.is_empty()
            && self.checksum_mismatch.is_empty()
            && self.failed.is_empty()
            && self.unknown.is_empty()
    }
}

pub async fn migration_report(
    conn: &mut sqlx::PgConnection,
) -> Result<MigrationReport, sqlx::Error> {
    let exists: (bool,) = sqlx::query_as("SELECT to_regclass('_sqlx_migrations') IS NOT NULL")
        .fetch_one(&mut *conn)
        .await?;
    let rows: Vec<(i64, bool, Vec<u8>)> = if exists.0 {
        sqlx::query_as("SELECT version, success, checksum FROM _sqlx_migrations ORDER BY version")
            .fetch_all(&mut *conn)
            .await?
    } else {
        Vec::new()
    };
    let mut report = MigrationReport::default();
    for m in MIGRATOR.iter() {
        match rows.iter().find(|(v, _, _)| *v == m.version) {
            None => report.pending.push((m.version, m.description.to_string())),
            Some((v, false, _)) => report.failed.push(*v),
            Some((v, true, checksum)) => {
                if checksum.as_slice() != m.checksum.as_ref() {
                    report.checksum_mismatch.push(*v);
                }
                report.applied.push(*v);
            }
        }
    }
    for (v, success, _) in &rows {
        if !MIGRATOR.iter().any(|m| m.version == *v) {
            report.unknown.push(*v);
            if *success {
                report.applied.push(*v);
            }
        }
    }
    report.applied.sort_unstable();
    Ok(report)
}

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
