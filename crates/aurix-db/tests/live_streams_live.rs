//! Live stream directory against a real PostgreSQL (`AURIX_E2E_DATABASE_URL`): rows are
//! published only for registered nodes, refreshed in place, read per tenant, listed fleet-wide
//! only while the owner is healthy, pruned to what the owner still holds, dropped with the
//! owner and counted as channel hosts by the cascade planner. Uses throwaway nodes and apps so
//! it can share the database with running nodes.

use aurix_db::models::{AppRow, LiveStreamRow, MediaNodeRow};
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

async fn fake_app(pool: &DbPool) -> Uuid {
    let now = Utc::now();
    let id = Uuid::new_v4();
    let row = AppRow {
        id,
        name: format!("live-streams-test-{id}"),
        description: None,
        owner_id: Uuid::new_v4(),
        api_key_hash: format!("test-{id}"),
        api_secret_hash: format!("test-{id}"),
        active: true,
        max_channels: 0,
        max_participants_per_channel: 0,
        max_concurrent_sessions: 0,
        monthly_participant_minutes: 0,
        created_at: now,
        updated_at: now,
    };
    queries::create_app(pool, &row).await.expect("app").id
}

async fn set_healthy(pool: &DbPool, node: Uuid, healthy: bool) {
    sqlx::query("UPDATE media_nodes SET healthy = $2 WHERE id = $1")
        .bind(node)
        .bind(healthy)
        .execute(pool)
        .await
        .expect("healthy flag");
}

fn row(node: Uuid, app: Uuid, channel: Uuid) -> LiveStreamRow {
    LiveStreamRow {
        id: Uuid::new_v4(),
        node_id: node,
        app_id: app,
        channel_id: channel,
        mode: "pull".into(),
        format: "opus".into(),
        mix: true,
        state: "streaming".into(),
        users: Some(serde_json::json!([Uuid::new_v4()])),
        label: Some("tap".into()),
        push_url: None,
        started_at: Utc::now(),
        updated_at: Utc::now(),
        frames_sent: 1,
        frames_dropped: 0,
        reconnects: 0,
        consent: serde_json::json!({}),
    }
}

fn ids(rows: &[LiveStreamRow]) -> Vec<Uuid> {
    let mut v: Vec<Uuid> = rows.iter().map(|r| r.id).collect();
    v.sort();
    v
}

#[tokio::test]
#[ignore = "requires PostgreSQL (AURIX_E2E_DATABASE_URL)"]
async fn directory_rows_are_scoped_refreshed_pruned_and_follow_their_node() {
    let Some(pool) = pool().await else {
        eprintln!("AURIX_E2E_DATABASE_URL not set; skipping");
        return;
    };
    let owner = fake_node(&pool).await;
    let other = fake_node(&pool).await;
    let app = fake_app(&pool).await;
    let tenant2 = fake_app(&pool).await;
    let channel = Uuid::new_v4();

    // A node that never registered publishes nothing (a stale process after a fleet reset).
    let ghost = row(Uuid::new_v4(), app, channel);
    queries::upsert_live_stream(&pool, &ghost)
        .await
        .expect("ghost upsert");
    assert!(queries::get_live_stream(&pool, app, ghost.id)
        .await
        .expect("get ghost")
        .is_none());

    // Publish, then refresh: status/counters change, identity stays.
    let mut a = row(owner, app, channel);
    queries::upsert_live_stream(&pool, &a).await.expect("a");
    a.state = "reconnecting".into();
    a.frames_sent = 40;
    a.frames_dropped = 3;
    a.reconnects = 1;
    a.consent = serde_json::json!({ Uuid::new_v4().to_string(): "accepted" });
    a.label = Some("renamed-is-ignored".into());
    queries::upsert_live_stream(&pool, &a)
        .await
        .expect("a refresh");
    let got = queries::get_live_stream(&pool, app, a.id)
        .await
        .expect("get a")
        .expect("a exists");
    assert_eq!(got.state, "reconnecting");
    assert_eq!(
        (got.frames_sent, got.frames_dropped, got.reconnects),
        (40, 3, 1)
    );
    assert_eq!(got.consent, a.consent);
    assert_eq!(
        got.label.as_deref(),
        Some("tap"),
        "descriptors are immutable"
    );
    assert!(got.mix && got.node_id == owner && got.channel_id == channel);

    // Tenant scoping: another app neither sees nor lists the stream.
    assert!(queries::get_live_stream(&pool, tenant2, a.id)
        .await
        .expect("get as tenant2")
        .is_none());
    set_healthy(&pool, owner, true).await;
    assert!(
        queries::list_remote_live_streams(&pool, tenant2, None, other)
            .await
            .expect("list tenant2")
            .is_empty()
    );

    // Fleet listing: from another node, per app and optionally per channel, never the caller's
    // own rows.
    let b = row(owner, app, Uuid::new_v4());
    queries::upsert_live_stream(&pool, &b).await.expect("b");
    let mine = row(other, app, channel);
    queries::upsert_live_stream(&pool, &mine)
        .await
        .expect("mine");
    let all = queries::list_remote_live_streams(&pool, app, None, other)
        .await
        .expect("list all");
    let mut want = vec![a.id, b.id];
    want.sort();
    assert_eq!(ids(&all), want);
    let one = queries::list_remote_live_streams(&pool, app, Some(channel), other)
        .await
        .expect("list channel");
    assert_eq!(ids(&one), vec![a.id]);

    // The planner counts the owner as a host of the channel, so the audio is relayed to it.
    let hosts = queries::remote_nodes_for_channels(&pool, &[channel], other)
        .await
        .expect("hosts");
    assert!(hosts.contains(&(channel, owner)));
    assert!(!hosts.iter().any(|(_, n)| *n == other));

    // Rows of an unhealthy owner are not offered to callers (its streams are gone with it).
    set_healthy(&pool, owner, false).await;
    assert!(queries::list_remote_live_streams(&pool, app, None, other)
        .await
        .expect("list unhealthy")
        .is_empty());
    assert!(
        !queries::remote_nodes_for_channels(&pool, &[channel], other)
            .await
            .expect("hosts unhealthy")
            .contains(&(channel, owner))
    );
    set_healthy(&pool, owner, true).await;

    // Prune to what the owner still holds; delete one explicitly.
    let pruned = queries::prune_live_streams(&pool, owner, &[a.id])
        .await
        .expect("prune");
    assert_eq!(pruned, 1);
    assert!(queries::get_live_stream(&pool, app, b.id)
        .await
        .expect("get b")
        .is_none());
    assert!(queries::delete_live_stream(&pool, a.id)
        .await
        .expect("delete a"));
    assert!(!queries::delete_live_stream(&pool, a.id)
        .await
        .expect("delete a again"));

    // A node that left the fleet takes its rows along, explicitly or through the FK cascade.
    let c = row(owner, app, channel);
    queries::upsert_live_stream(&pool, &c).await.expect("c");
    assert_eq!(
        queries::delete_node_live_streams(&pool, owner)
            .await
            .expect("node rows"),
        1
    );
    let d = row(owner, app, channel);
    queries::upsert_live_stream(&pool, &d).await.expect("d");
    queries::delete_media_node(&pool, owner)
        .await
        .expect("delete owner");
    assert!(queries::get_live_stream(&pool, app, d.id)
        .await
        .expect("get d")
        .is_none());

    queries::delete_media_node(&pool, other)
        .await
        .expect("cleanup other");
    for app in [app, tenant2] {
        sqlx::query("DELETE FROM apps WHERE id = $1")
            .bind(app)
            .execute(&pool)
            .await
            .expect("cleanup app");
    }
}
