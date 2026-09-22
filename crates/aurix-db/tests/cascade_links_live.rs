//! Cascade link table against a real PostgreSQL (`AURIX_E2E_DATABASE_URL`): a node replaces
//! its own rows, unknown peers are skipped, an empty report still counts as a report, stale
//! rows and reporters fall out of the fresh view but stay listed, and deleting a node removes
//! its links. Uses throwaway unhealthy nodes so it can share the database with running nodes.

use aurix_db::models::MediaNodeRow;
use aurix_db::{queries, DbPool};
use chrono::Utc;
use uuid::Uuid;

async fn pool() -> Option<DbPool> {
    let url = std::env::var("AURIX_E2E_DATABASE_URL").ok()?;
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(4)
        .connect(&url)
        .await
        .expect("postgres");
    aurix_db::migrations::MIGRATOR
        .run(&pool)
        .await
        .expect("migrations");
    Some(pool)
}

async fn fake_node(pool: &DbPool) -> Uuid {
    let now = Utc::now();
    let row = MediaNodeRow {
        id: Uuid::new_v4(),
        region: "test".into(),
        address: "127.0.0.1".into(),
        address_ipv6: None,
        media_port: 1,
        api_port: 1,
        cascade_port: Some(2),
        ws_url: None,
        api_url: None,
        latitude: None,
        longitude: None,
        capacity: 0,
        active_channels: 0,
        active_participants: 0,
        cpu_usage: 0.0,
        memory_usage: 0.0,
        bandwidth_in_mbps: 0.0,
        bandwidth_out_mbps: 0.0,
        healthy: false,
        relay_only: false,
        draining: false,
        drain_reason: None,
        draining_since: None,
        drained_by: None,
        version: "test".into(),
        last_heartbeat: now,
        registered_at: now,
    };
    queries::upsert_media_node(pool, &row)
        .await
        .expect("node")
        .id
}

async fn age(pool: &DbPool, node: Uuid, secs: i64) {
    sqlx::query(
        "UPDATE media_node_links SET measured_at = NOW() - make_interval(secs => $2) WHERE node_id = $1",
    )
    .bind(node)
    .bind(secs as f64)
    .execute(pool)
    .await
    .expect("age links");
    sqlx::query(
        "UPDATE media_nodes SET links_reported_at = NOW() - make_interval(secs => $2) WHERE id = $1",
    )
    .bind(node)
    .bind(secs as f64)
    .execute(pool)
    .await
    .expect("age report");
}

fn of(rows: &[aurix_db::models::MediaNodeLinkRow], node: Uuid) -> Vec<(Uuid, String, i32)> {
    rows.iter()
        .filter(|r| r.node_id == node)
        .map(|r| (r.peer_id, r.transport.clone(), r.rtt_ms))
        .collect()
}

#[tokio::test]
#[ignore = "requires PostgreSQL (AURIX_E2E_DATABASE_URL)"]
async fn link_tables_are_replaced_aged_out_and_cascade_deleted() {
    let Some(pool) = pool().await else {
        eprintln!("AURIX_E2E_DATABASE_URL not set; skipping");
        return;
    };
    let a = fake_node(&pool).await;
    let b = fake_node(&pool).await;
    let c = fake_node(&pool).await;
    let ghost = Uuid::new_v4();

    queries::replace_media_node_links(
        &pool,
        a,
        &[(b, "udp", 12), (c, "tcp", 80), (ghost, "udp", 1)],
    )
    .await
    .expect("report a");
    let (fresh, reporters) = queries::fresh_media_node_links(&pool, 60)
        .await
        .expect("fresh");
    let mut got = of(&fresh, a);
    got.sort();
    let mut want = vec![(b, "udp".to_string(), 12), (c, "tcp".to_string(), 80)];
    want.sort();
    assert_eq!(got, want, "unknown peers are skipped, known ones stored");
    assert!(reporters.contains(&a) && !reporters.contains(&b));

    // An empty table is still a report: b becomes a reporter without rows.
    queries::replace_media_node_links(&pool, b, &[])
        .await
        .expect("report b");
    let (fresh, reporters) = queries::fresh_media_node_links(&pool, 60)
        .await
        .expect("fresh");
    assert!(of(&fresh, b).is_empty());
    assert!(reporters.contains(&b));

    // A new report replaces the old table: c disappears, b's RTT is updated.
    queries::replace_media_node_links(&pool, a, &[(b, "udp", 15)])
        .await
        .expect("report a again");
    let (fresh, _) = queries::fresh_media_node_links(&pool, 60)
        .await
        .expect("fresh");
    assert_eq!(of(&fresh, a), vec![(b, "udp".to_string(), 15)]);

    // Stale rows and reporters drop out of the fresh view but stay in the operator listing.
    age(&pool, a, 120).await;
    let (fresh, reporters) = queries::fresh_media_node_links(&pool, 60)
        .await
        .expect("fresh");
    assert!(of(&fresh, a).is_empty(), "aged rows are not fresh");
    assert!(!reporters.contains(&a) && reporters.contains(&b));
    let all = queries::list_media_node_links(&pool).await.expect("list");
    assert_eq!(of(&all, a), vec![(b, "udp".to_string(), 15)]);

    // Deleting a node removes the links it reported and the links pointing at it.
    queries::replace_media_node_links(&pool, c, &[(a, "udp", 3)])
        .await
        .expect("report c");
    queries::delete_media_node(&pool, a)
        .await
        .expect("delete a");
    let all = queries::list_media_node_links(&pool).await.expect("list");
    assert!(of(&all, a).is_empty() && of(&all, c).is_empty());

    for n in [b, c] {
        queries::delete_media_node(&pool, n).await.expect("cleanup");
    }
}
