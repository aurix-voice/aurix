//! `migration_report` against a real PostgreSQL (`AURIX_E2E_DATABASE_URL`): a migrated
//! database is current, and every drift the report distinguishes — pending, checksum
//! mismatch, failed and unknown rows — is produced inside a transaction that is rolled back,
//! so the test can share the database with running nodes.

use aurix_db::migrations::{migration_report, MigrationReport, MIGRATOR};
use aurix_db::DbPool;
use sqlx::Connection;

async fn pool() -> Option<DbPool> {
    let url = std::env::var("AURIX_E2E_DATABASE_URL").ok()?;
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(2)
        .connect(&url)
        .await
        .expect("postgres");
    MIGRATOR.run(&pool).await.expect("migrations");
    Some(pool)
}

#[tokio::test]
#[ignore = "requires PostgreSQL (AURIX_E2E_DATABASE_URL)"]
async fn report_distinguishes_current_pending_drifted_failed_and_unknown() {
    let Some(pool) = pool().await else {
        eprintln!("AURIX_E2E_DATABASE_URL not set; skipping");
        return;
    };
    let latest = MigrationReport::embedded_latest();
    let embedded: Vec<i64> = MIGRATOR.iter().map(|m| m.version).collect();
    assert_eq!(latest, *embedded.iter().max().unwrap());

    let mut conn = pool.acquire().await.unwrap();
    let report = migration_report(&mut conn).await.unwrap();
    assert!(report.is_current(), "{report:?}");
    assert_eq!(report.applied, embedded);
    assert!(report.pending.is_empty());

    let mut tx = conn.begin().await.unwrap();
    let first = embedded[0];
    let last = latest;
    let middle = embedded[embedded.len() / 2];
    let ghost = latest + 1;
    sqlx::query("UPDATE _sqlx_migrations SET checksum = '\\xdeadbeef'::bytea WHERE version = $1")
        .bind(first)
        .execute(&mut *tx)
        .await
        .unwrap();
    sqlx::query("UPDATE _sqlx_migrations SET success = false WHERE version = $1")
        .bind(middle)
        .execute(&mut *tx)
        .await
        .unwrap();
    sqlx::query("DELETE FROM _sqlx_migrations WHERE version = $1")
        .bind(last)
        .execute(&mut *tx)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO _sqlx_migrations (version, description, installed_on, success, checksum, execution_time) \
         VALUES ($1, 'from a newer build', now(), true, '\\x00'::bytea, 0)",
    )
    .bind(ghost)
    .execute(&mut *tx)
    .await
    .unwrap();

    let drifted = migration_report(&mut tx).await.unwrap();
    assert!(!drifted.is_current());
    assert_eq!(drifted.checksum_mismatch, vec![first]);
    assert_eq!(drifted.failed, vec![middle]);
    assert_eq!(drifted.unknown, vec![ghost]);
    assert_eq!(
        drifted.pending.iter().map(|(v, _)| *v).collect::<Vec<_>>(),
        vec![last]
    );
    assert!(
        !drifted.pending[0].1.is_empty(),
        "pending carries the description"
    );
    assert!(
        drifted.applied.contains(&first),
        "a drifted row is still applied"
    );
    assert!(
        !drifted.applied.contains(&middle),
        "a failed row is not applied"
    );
    assert!(!drifted.applied.contains(&last));
    assert!(
        drifted.applied.contains(&ghost),
        "an unknown successful row counts as applied"
    );
    tx.rollback().await.unwrap();

    let restored = migration_report(&mut conn).await.unwrap();
    assert!(restored.is_current(), "{restored:?}");

    // Without the migrations table (a fresh database) everything embedded is pending.
    let mut tx = conn.begin().await.unwrap();
    sqlx::query("ALTER TABLE _sqlx_migrations RENAME TO _sqlx_migrations_hidden")
        .execute(&mut *tx)
        .await
        .unwrap();
    let fresh = migration_report(&mut tx).await.unwrap();
    assert!(fresh.applied.is_empty());
    assert_eq!(fresh.pending.len(), embedded.len());
    tx.rollback().await.unwrap();
}
