use aurix_common::error::Result;
use aurix_db::models::AnalyticsSnapshotRow;
use aurix_db::DbPool;
use chrono::Utc;
use tracing::{error, info};
use uuid::Uuid;

pub struct AnalyticsCollector {
    pool: DbPool,
}

impl AnalyticsCollector {
    pub fn new(pool: DbPool) -> Self {
        Self { pool }
    }

    /// Start the periodic analytics collection task.
    pub fn start(self) {
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(300)); // every 5 min
            loop {
                interval.tick().await;
                if let Err(e) = self.collect_snapshots().await {
                    error!("Analytics collection failed: {}", e);
                }
            }
        });
    }

    async fn collect_snapshots(&self) -> Result<()> {
        // Get all active apps
        let apps = aurix_db::queries::list_apps(&self.pool, 1000, 0)
            .await
            .map_err(|e| aurix_common::error::AurixError::Database(e.to_string()))?;

        for app in &apps {
            let app_id = app.id;

            let active_users = aurix_db::queries::count_active_sessions(&self.pool, app_id)
                .await
                .unwrap_or(0);

            let active_channels = aurix_db::queries::count_active_channels(&self.pool, app_id)
                .await
                .unwrap_or(0);

            let snapshot = AnalyticsSnapshotRow {
                id: Uuid::now_v7(),
                app_id,
                timestamp: Utc::now(),
                active_users,
                active_channels,
                peak_concurrent: active_users, // simplified: current = peak for this window
                total_minutes: 0.0,
                bandwidth_gb: 0.0,
                avg_latency_ms: 0.0,
                avg_packet_loss: 0.0,
                error_count: 0,
            };

            if let Err(e) =
                aurix_db::queries::insert_analytics_snapshot(&self.pool, &snapshot).await
            {
                error!("Failed to insert snapshot for app {}: {}", app_id, e);
            }
        }

        info!("Analytics snapshots collected for {} apps", apps.len());
        Ok(())
    }
}
