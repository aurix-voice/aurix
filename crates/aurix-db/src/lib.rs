pub mod models;
pub mod queries;
pub mod migrations;

use sqlx::postgres::{PgPool, PgPoolOptions};
use aurix_common::config::DatabaseConfig;
use std::time::Duration;

pub type DbPool = PgPool;

pub async fn create_pool(config: &DatabaseConfig) -> Result<DbPool, sqlx::Error> {
    let pool = PgPoolOptions::new()
        .max_connections(config.max_connections)
        .min_connections(config.min_connections)
        .acquire_timeout(Duration::from_secs(config.connect_timeout_secs))
        .idle_timeout(Duration::from_secs(config.idle_timeout_secs))
        .connect(&config.url)
        .await?;

    if config.run_migrations {
        tracing::info!("Running database migrations...");
        migrations::MIGRATOR
            .run(&pool)
            .await?;
        tracing::info!("Database migrations complete");
    }

    Ok(pool)
}