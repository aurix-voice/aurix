//! Usage aggregation against a real PostgreSQL (`AURIX_E2E_DATABASE_URL`): interval overlap
//! split across buckets, exact peak concurrency with equal-timestamp ordering, open
//! intervals capped at `NOW()`, late-closure re-derivation, tenant separation, additive
//! metered counters, watermarks, retention and the concurrent-session admission lock.
//! Timestamps sit 500 days in the past so the test can share the database with running nodes.

use aurix_common::usage::{APP_BUCKET_SECS, CHANNEL_BUCKET_SECS};
use aurix_db::models::{AppRow, ChannelRow, SessionRow, UserRow};
use aurix_db::usage::{self as usage, CounterDelta};
use aurix_db::{queries, DbPool};
use chrono::{DateTime, Duration, Utc};
use uuid::Uuid;

async fn pool() -> Option<DbPool> {
    let url = std::env::var("AURIX_E2E_DATABASE_URL").ok()?;
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(8)
        .connect(&url)
        .await
        .expect("postgres");
    aurix_db::migrations::MIGRATOR
        .run(&pool)
        .await
        .expect("migrations");
    Some(pool)
}

async fn app(pool: &DbPool, name: &str, max_concurrent: i32) -> Uuid {
    let now = Utc::now();
    let row = AppRow {
        id: Uuid::new_v4(),
        name: format!("usage-live {name}"),
        description: None,
        owner_id: Uuid::new_v4(),
        api_key_hash: format!("test-{}", Uuid::new_v4()),
        api_secret_hash: format!("test-{}", Uuid::new_v4()),
        active: true,
        max_channels: 10,
        max_participants_per_channel: 10,
        max_concurrent_sessions: max_concurrent,
        monthly_participant_minutes: 0,
        created_at: now,
        updated_at: now,
    };
    queries::create_app(pool, &row).await.expect("app").id
}

async fn user(pool: &DbPool, app_id: Uuid) -> Uuid {
    let now = Utc::now();
    let row = UserRow {
        id: Uuid::new_v4(),
        app_id,
        external_id: format!("usage-live:{}", Uuid::new_v4()),
        display_name: "u".into(),
        metadata: None,
        is_banned: false,
        ban_reason: None,
        ban_expires_at: None,
        device_ids: Vec::new(),
        total_session_minutes: 0,
        last_seen_at: None,
        created_at: now,
        updated_at: now,
    };
    queries::upsert_user(pool, &row).await.expect("user").id
}

async fn channel(pool: &DbPool, app_id: Uuid) -> Uuid {
    let now = Utc::now();
    let row = ChannelRow {
        id: Uuid::new_v4(),
        app_id,
        name: format!("c-{}", Uuid::new_v4()),
        channel_type: "positional".into(),
        config: serde_json::json!({}),
        max_participants: 10,
        is_persistent: true,
        ad_hoc: false,
        active_participants: 0,
        created_at: now,
        updated_at: now,
        deleted_at: None,
    };
    queries::create_channel(pool, &row)
        .await
        .expect("channel")
        .id
}

fn session_row(app_id: Uuid, user_id: Uuid, at: DateTime<Utc>) -> SessionRow {
    SessionRow {
        id: Uuid::new_v4(),
        user_id,
        app_id,
        media_node_id: Uuid::nil(),
        ip_address: "127.0.0.1".into(),
        user_agent: None,
        connected_at: at,
        disconnected_at: None,
        disconnect_reason: None,
        quality_stats: None,
    }
}

/// Inserts a session `[from, to)` (`to = None` = still open) and returns its id.
async fn session(
    pool: &DbPool,
    app_id: Uuid,
    user_id: Uuid,
    from: DateTime<Utc>,
    to: Option<DateTime<Utc>>,
) -> Uuid {
    let row = queries::create_session(pool, &session_row(app_id, user_id, from))
        .await
        .expect("session");
    if let Some(to) = to {
        close_session(pool, row.id, to).await;
    }
    row.id
}

async fn close_session(pool: &DbPool, id: Uuid, at: DateTime<Utc>) {
    sqlx::query("UPDATE sessions SET disconnected_at = $2 WHERE id = $1")
        .bind(id)
        .bind(at)
        .execute(pool)
        .await
        .unwrap();
}

async fn membership(
    pool: &DbPool,
    channel_id: Uuid,
    user_id: Uuid,
    session_id: Uuid,
    from: DateTime<Utc>,
    to: Option<DateTime<Utc>>,
) {
    sqlx::query(
        r#"INSERT INTO channel_memberships (id, channel_id, user_id, session_id, ssrc, joined_at, left_at)
           VALUES ($1, $2, $3, $4, 1, $5, $6)"#,
    )
    .bind(Uuid::new_v4())
    .bind(channel_id)
    .bind(user_id)
    .bind(session_id)
    .bind(from)
    .bind(to)
    .execute(pool)
    .await
    .unwrap();
}

async fn cleanup(pool: &DbPool, apps: &[Uuid]) {
    for app_id in apps {
        for sql in [
            "DELETE FROM channel_memberships WHERE session_id IN (SELECT id FROM sessions WHERE app_id = $1)",
            "DELETE FROM sessions WHERE app_id = $1",
            "DELETE FROM channels WHERE app_id = $1",
            "DELETE FROM users WHERE app_id = $1",
            "DELETE FROM apps WHERE id = $1",
        ] {
            sqlx::query(sql).bind(app_id).execute(pool).await.unwrap();
        }
    }
}

fn minutes(m: i64) -> Duration {
    Duration::minutes(m)
}

fn approx(a: f64, b: f64) -> bool {
    (a - b).abs() < 1e-6
}

/// Base instant: a whole hour 500 days ago.
fn base() -> DateTime<Utc> {
    aurix_common::usage::bucket_start(Utc::now() - Duration::days(500), CHANNEL_BUCKET_SECS)
}

#[tokio::test]
#[ignore = "requires PostgreSQL (AURIX_E2E_DATABASE_URL)"]
async fn app_and_channel_buckets_are_derived_from_intervals() {
    let Some(pool) = pool().await else {
        eprintln!("AURIX_E2E_DATABASE_URL not set; skipping");
        return;
    };
    let t0 = base();
    let a = app(&pool, "A", 0).await;
    let b = app(&pool, "B", 0).await;
    let (u1, u2, u3) = (
        user(&pool, a).await,
        user(&pool, a).await,
        user(&pool, b).await,
    );
    let (c1, c2) = (channel(&pool, a).await, channel(&pool, b).await);

    // App A: s1 [1, 11), s2 [2, 4), s3 [4, 6) (starts the instant s2 ends), s4 [7, open).
    let s1 = session(&pool, a, u1, t0 + minutes(1), Some(t0 + minutes(11))).await;
    let s2 = session(&pool, a, u2, t0 + minutes(2), Some(t0 + minutes(4))).await;
    let s3 = session(&pool, a, u2, t0 + minutes(4), Some(t0 + minutes(6))).await;
    let s4 = session(&pool, a, u1, t0 + minutes(7), None).await;
    membership(&pool, c1, u1, s1, t0 + minutes(1), Some(t0 + minutes(11))).await;
    membership(&pool, c1, u2, s2, t0 + minutes(2), Some(t0 + minutes(4))).await;
    membership(&pool, c1, u2, s3, t0 + minutes(4), Some(t0 + minutes(6))).await;
    // App B: one short session in the same hour.
    let s5 = session(&pool, b, u3, t0 + minutes(1), Some(t0 + minutes(2))).await;
    membership(&pool, c2, u3, s5, t0 + minutes(1), Some(t0 + minutes(2))).await;

    let hour = Duration::seconds(CHANNEL_BUCKET_SECS);
    usage::aggregate_app_buckets(&pool, t0, t0 + hour, APP_BUCKET_SECS)
        .await
        .unwrap();
    usage::aggregate_channel_buckets(&pool, t0, t0 + hour, CHANNEL_BUCKET_SECS)
        .await
        .unwrap();

    let rows = usage::app_series(&pool, a, t0, t0 + hour, 1000)
        .await
        .unwrap();
    // Buckets 0..=11 hold s4 (open) — 12 buckets in the hour.
    assert_eq!(rows.len(), 12, "{rows:?}");
    let b0 = &rows[0];
    assert_eq!(b0.bucket, t0);
    assert_eq!(
        b0.peak_sessions, 2,
        "disconnect at :04 sorts before the connect at :04"
    );
    assert!(
        approx(b0.session_minutes, 4.0 + 2.0 + 1.0),
        "{}",
        b0.session_minutes
    );
    assert_eq!(b0.sessions_started, 3);
    assert_eq!(b0.unique_users, 2);
    assert_eq!(b0.peak_participants, 2);
    assert!(
        approx(b0.participant_minutes, 7.0),
        "{}",
        b0.participant_minutes
    );
    assert_eq!(b0.active_channels, 1);
    let b1 = &rows[1];
    assert_eq!(b1.peak_sessions, 2);
    assert!(
        approx(b1.session_minutes, 5.0 + 1.0 + 3.0),
        "{}",
        b1.session_minutes
    );
    assert_eq!(b1.sessions_started, 1);
    assert_eq!(b1.unique_users, 2, "u1 (s1) and u2 (s3); s4 is u1 again");
    let b2 = &rows[2];
    assert_eq!(b2.peak_sessions, 2);
    assert!(approx(b2.session_minutes, 1.0 + 5.0));
    assert_eq!(b2.sessions_started, 0);
    let b5 = &rows[5];
    assert_eq!(b5.peak_sessions, 1);
    assert!(
        approx(b5.session_minutes, 5.0),
        "open session fills the bucket"
    );
    assert_eq!(b5.peak_participants, 0);

    let ch = usage::channel_series(&pool, a, Some(c1), t0, t0 + hour, 10)
        .await
        .unwrap();
    assert_eq!(ch.len(), 1);
    assert_eq!(ch[0].bucket, t0);
    assert_eq!(ch[0].peak_participants, 2);
    assert!(approx(ch[0].participant_minutes, 10.0 + 2.0 + 2.0));
    assert_eq!(ch[0].joins, 3);
    assert_eq!(ch[0].unique_users, 2);

    // Tenant separation: B sees only its own minute.
    let rows_b = usage::app_series(&pool, b, t0, t0 + hour, 1000)
        .await
        .unwrap();
    assert_eq!(rows_b.len(), 1);
    assert_eq!(rows_b[0].peak_sessions, 1);
    assert!(approx(rows_b[0].session_minutes, 1.0));
    assert!(approx(rows_b[0].participant_minutes, 1.0));
    let ch_b = usage::channel_series(&pool, b, None, t0, t0 + hour, 10)
        .await
        .unwrap();
    assert_eq!(ch_b.len(), 1);
    assert_eq!(ch_b[0].channel_id, c2);
    assert!(usage::channel_series(&pool, a, Some(c2), t0, t0 + hour, 10)
        .await
        .unwrap()
        .is_empty());

    // Rollup to the hour: sums of minutes, maxima of peaks.
    let rolled = usage::app_series_rollup(&pool, a, t0, t0 + hour, CHANNEL_BUCKET_SECS, 10)
        .await
        .unwrap();
    assert_eq!(rolled.len(), 1);
    assert_eq!(rolled[0].bucket, t0);
    assert_eq!(rolled[0].peak_sessions, 2);
    let total_minutes: f64 = rows.iter().map(|r| r.session_minutes).sum();
    assert!(approx(rolled[0].session_minutes, total_minutes));
    assert_eq!(rolled[0].sessions_started, 4);
    let totals = usage::app_totals(&pool, a, t0, t0 + hour)
        .await
        .unwrap()
        .expect("totals");
    assert!(approx(totals.session_minutes, total_minutes));
    assert_eq!(totals.peak_sessions, 2);

    // Late closure: s4 actually ended at :12. Re-deriving the hour must shrink bucket 2 and
    // zero buckets 3..=11 (the open row had filled them).
    close_session(&pool, s4, t0 + minutes(12)).await;
    usage::aggregate_app_buckets(&pool, t0, t0 + hour, APP_BUCKET_SECS)
        .await
        .unwrap();
    let rows = usage::app_series(&pool, a, t0, t0 + hour, 1000)
        .await
        .unwrap();
    assert!(
        approx(rows[2].session_minutes, 1.0 + 2.0),
        "{}",
        rows[2].session_minutes
    );
    for r in &rows[3..] {
        assert_eq!(r.peak_sessions, 0, "{r:?}");
        assert!(approx(r.session_minutes, 0.0), "{r:?}");
    }

    // Additive counters: two flushes for the same bucket add up; channel-less bytes stay at
    // app scope; an unknown app is ignored instead of failing the batch.
    let deltas = [
        CounterDelta {
            app_id: a,
            channel_id: Some(c1),
            bucket: t0,
            metric: "chat_messages",
            value: 1,
        },
        CounterDelta {
            app_id: a,
            channel_id: Some(c1),
            bucket: t0 + minutes(5),
            metric: "chat_messages",
            value: 2,
        },
        CounterDelta {
            app_id: a,
            channel_id: None,
            bucket: t0,
            metric: "media_bytes_in",
            value: 1000,
        },
        CounterDelta {
            app_id: a,
            channel_id: Some(c1),
            bucket: t0,
            metric: "quality_samples",
            value: 3,
        },
        CounterDelta {
            app_id: a,
            channel_id: None,
            bucket: t0 + minutes(5),
            metric: "quality_samples",
            value: 1,
        },
        CounterDelta {
            app_id: a,
            channel_id: None,
            bucket: t0,
            metric: "mos_sum_milli",
            value: 12_300,
        },
        CounterDelta {
            app_id: a,
            channel_id: None,
            bucket: t0,
            metric: "poor_quality_samples",
            value: 2,
        },
        CounterDelta {
            app_id: Uuid::new_v4(),
            channel_id: None,
            bucket: t0,
            metric: "chat_messages",
            value: 7,
        },
    ];
    usage::add_counters(&pool, &deltas, CHANNEL_BUCKET_SECS)
        .await
        .unwrap();
    usage::add_counters(&pool, &deltas[..1], CHANNEL_BUCKET_SECS)
        .await
        .unwrap();
    let rows = usage::app_series(&pool, a, t0, t0 + hour, 1000)
        .await
        .unwrap();
    assert_eq!(rows[0].chat_messages, 2);
    assert_eq!(rows[1].chat_messages, 2);
    assert_eq!(rows[0].media_bytes_in, 1000);
    assert_eq!(
        rows[0].quality_samples, 3,
        "quality counters are additive too"
    );
    assert_eq!(rows[0].mos_sum_milli, 12_300);
    assert_eq!(rows[0].poor_quality_samples, 2);
    assert_eq!(rows[1].quality_samples, 1);
    let ch = usage::channel_series(&pool, a, Some(c1), t0, t0 + hour, 10)
        .await
        .unwrap();
    assert_eq!(
        ch[0].chat_messages, 4,
        "both 5-minute buckets land in the hour"
    );
    assert_eq!(
        ch[0].peak_participants, 2,
        "derived columns untouched by counters"
    );
    let rolled = usage::app_series_rollup(&pool, a, t0, t0 + hour, CHANNEL_BUCKET_SECS, 10)
        .await
        .unwrap();
    assert_eq!(rolled[0].quality_samples, 4, "rollups sum quality counters");
    assert_eq!(rolled[0].mos_sum_milli, 12_300);
    let totals = usage::app_totals(&pool, a, t0, t0 + hour)
        .await
        .unwrap()
        .expect("totals");
    assert_eq!(totals.quality_samples, 4);
    assert_eq!(totals.poor_quality_samples, 2);
    assert_eq!(
        usage::app_totals(&pool, b, t0, t0 + hour)
            .await
            .unwrap()
            .map(|t| t.quality_samples),
        Some(0),
        "tenant separation"
    );
    // …and a re-derivation keeps the counters.
    usage::aggregate_app_buckets(&pool, t0, t0 + hour, APP_BUCKET_SECS)
        .await
        .unwrap();
    let rows = usage::app_series(&pool, a, t0, t0 + hour, 1000)
        .await
        .unwrap();
    assert_eq!(rows[0].chat_messages, 2);
    assert_eq!(rows[0].peak_sessions, 2);
    assert_eq!(rows[0].quality_samples, 3);

    let busiest = usage::channel_totals(&pool, a, t0, t0 + hour, 10)
        .await
        .unwrap();
    assert_eq!(busiest.len(), 1);
    assert_eq!(busiest[0].channel_id, c1);
    assert_eq!(busiest[0].chat_messages, 4);

    // Retention: everything before :05 goes (bucket 0 of both apps at least).
    let removed = usage::delete_app_buckets_before(&pool, t0 + minutes(5))
        .await
        .unwrap();
    assert!(removed >= 2, "{removed}");
    let rows = usage::app_series(&pool, a, t0, t0 + hour, 1000)
        .await
        .unwrap();
    assert_eq!(rows[0].bucket, t0 + minutes(5));
    cleanup(&pool, &[a, b]).await;
}

#[tokio::test]
#[ignore = "requires PostgreSQL (AURIX_E2E_DATABASE_URL)"]
async fn open_intervals_are_capped_at_now_and_quota_sees_live_minutes() {
    let Some(pool) = pool().await else {
        eprintln!("AURIX_E2E_DATABASE_URL not set; skipping");
        return;
    };
    let a = app(&pool, "live", 0).await;
    let u = user(&pool, a).await;
    let c = channel(&pool, a).await;
    let started = Utc::now() - Duration::seconds(120);
    let s = session(&pool, a, u, started, None).await;
    membership(&pool, c, u, s, started, None).await;
    let from = aurix_common::usage::bucket_start(started, APP_BUCKET_SECS);
    let to = aurix_common::usage::bucket_start(Utc::now(), APP_BUCKET_SECS)
        + Duration::seconds(APP_BUCKET_SECS);
    usage::aggregate_app_buckets(&pool, from, to, APP_BUCKET_SECS)
        .await
        .unwrap();
    let rows = usage::app_series(&pool, a, from, to, 10).await.unwrap();
    let total: f64 = rows.iter().map(|r| r.session_minutes).sum();
    assert!(
        (1.9..=2.3).contains(&total),
        "open session counts up to NOW(), got {total} min over {rows:?}"
    );
    assert!(rows.iter().all(|r| r.peak_sessions == 1));

    // Quota view: with no watermark everything is live; with the watermark at the current
    // bucket, the finalized part plus the live overlap give the same total.
    let all_live = usage::participant_minutes_since(&pool, a, from, from)
        .await
        .unwrap();
    assert!((1.9..=2.4).contains(&all_live), "{all_live}");
    let current = aurix_common::usage::bucket_start(Utc::now(), APP_BUCKET_SECS);
    let split = usage::participant_minutes_since(&pool, a, from, current)
        .await
        .unwrap();
    assert!((all_live - split).abs() < 0.1, "{all_live} vs {split}");
    cleanup(&pool, &[a]).await;
}

#[tokio::test]
#[ignore = "requires PostgreSQL (AURIX_E2E_DATABASE_URL)"]
async fn session_quality_checkpoints_hand_over_and_rank_worst_first() {
    let Some(pool) = pool().await else {
        return;
    };
    let a = app(&pool, "Q", 0).await;
    let b = app(&pool, "Q-other", 0).await;
    let u = user(&pool, a).await;
    let ub = user(&pool, b).await;
    let t0 = base();
    let node1 = Uuid::new_v4();
    let node2 = Uuid::new_v4();
    let quality = |samples: i64, mos_avg: f64| serde_json::json!({ "samples": samples, "mos_avg": mos_avg, "bars": [0, 0, 0, 0, samples] });

    let mut rows = Vec::new();
    for i in 0..3 {
        let mut row = session_row(a, u, t0 + minutes(i));
        row.media_node_id = node1;
        rows.push(queries::create_session(&pool, &row).await.unwrap().id);
    }
    let mut other = session_row(b, ub, t0);
    other.media_node_id = node1;
    let other = queries::create_session(&pool, &other).await.unwrap().id;

    // Checkpoints are fenced by the owning node; a closed session is not rewritten.
    assert_eq!(
        queries::update_session_quality(&pool, rows[0], node1, quality(10, 2.4))
            .await
            .unwrap(),
        1
    );
    assert_eq!(
        queries::update_session_quality(&pool, rows[1], node2, quality(10, 1.0))
            .await
            .unwrap(),
        0,
        "another node cannot overwrite the owner's record"
    );
    queries::update_session_quality(&pool, rows[1], node1, quality(10, 4.1))
        .await
        .unwrap();
    queries::update_session_quality(&pool, rows[2], node1, quality(2, 1.5))
        .await
        .unwrap();
    queries::update_session_quality(&pool, other, node1, quality(10, 1.2))
        .await
        .unwrap();
    queries::close_session(&pool, rows[1], node1, "client", Some(quality(12, 4.2)))
        .await
        .unwrap();
    assert_eq!(
        queries::update_session_quality(&pool, rows[1], node1, quality(99, 1.0))
            .await
            .unwrap(),
        0
    );

    // Takeover hands the persisted record to the adopting node.
    let migrated = queries::migrate_session(&pool, rows[0], a, u, node2)
        .await
        .unwrap()
        .expect("adoptable");
    assert!(!migrated.reaped);
    assert_eq!(migrated.quality_stats, Some(quality(10, 2.4)));
    assert_eq!(
        queries::update_session_quality(&pool, rows[0], node2, quality(11, 2.3))
            .await
            .unwrap(),
        1,
        "the new owner continues the checkpoints"
    );
    assert_eq!(
        queries::update_session_quality(&pool, rows[0], node1, quality(1, 1.0))
            .await
            .unwrap(),
        0,
        "the old owner is fenced out"
    );

    // Worst first, tenant-scoped, thinly rated sessions excluded by `min_samples`.
    let worst = queries::worst_quality_sessions(&pool, a, t0, t0 + minutes(60), 3, 10)
        .await
        .unwrap();
    assert_eq!(
        worst.iter().map(|s| s.id).collect::<Vec<_>>(),
        vec![rows[0], rows[1]]
    );
    assert_eq!(worst[0].quality_stats, Some(quality(11, 2.3)));
    assert!(worst[1].disconnected_at.is_some());
    let all = queries::worst_quality_sessions(&pool, a, t0, t0 + minutes(60), 1, 10)
        .await
        .unwrap();
    assert_eq!(
        all.iter().map(|s| s.id).collect::<Vec<_>>(),
        vec![rows[2], rows[0], rows[1]]
    );
    let limited = queries::worst_quality_sessions(&pool, a, t0, t0 + minutes(60), 1, 1)
        .await
        .unwrap();
    assert_eq!(limited.len(), 1);
    assert!(
        queries::worst_quality_sessions(&pool, a, t0 + minutes(10), t0 + minutes(60), 1, 10)
            .await
            .unwrap()
            .is_empty(),
        "range applies to connected_at"
    );

    // A hand-edited or foreign-shaped record is skipped rather than failing the listing.
    for (i, junk) in [
        serde_json::json!({ "samples": "ten", "mos_avg": 1.0 }),
        serde_json::json!({ "samples": 10, "mos_avg": "bad" }),
        serde_json::json!(["not", "an", "object"]),
        serde_json::json!({}),
    ]
    .into_iter()
    .enumerate()
    {
        let mut row = session_row(a, u, t0 + minutes(20 + i as i64));
        row.media_node_id = node1;
        let id = queries::create_session(&pool, &row).await.unwrap().id;
        queries::update_session_quality(&pool, id, node1, junk)
            .await
            .unwrap();
    }
    let all = queries::worst_quality_sessions(&pool, a, t0, t0 + minutes(60), 1, 10)
        .await
        .unwrap();
    assert_eq!(
        all.iter().map(|s| s.id).collect::<Vec<_>>(),
        vec![rows[2], rows[0], rows[1]],
        "malformed quality_stats rows are ignored"
    );
    cleanup(&pool, &[a, b]).await;
}

#[tokio::test]
#[ignore = "requires PostgreSQL (AURIX_E2E_DATABASE_URL)"]
async fn watermarks_round_trip() {
    let Some(pool) = pool().await else {
        eprintln!("AURIX_E2E_DATABASE_URL not set; skipping");
        return;
    };
    let scope = format!("test-{}", Uuid::new_v4());
    assert!(usage::get_watermark(&pool, &scope).await.unwrap().is_none());
    let t = base();
    usage::set_watermark(&pool, &scope, t).await.unwrap();
    assert_eq!(usage::get_watermark(&pool, &scope).await.unwrap(), Some(t));
    usage::set_watermark(&pool, &scope, t + minutes(5))
        .await
        .unwrap();
    assert_eq!(
        usage::get_watermark(&pool, &scope).await.unwrap(),
        Some(t + minutes(5))
    );
    sqlx::query("DELETE FROM usage_watermarks WHERE scope = $1")
        .bind(&scope)
        .execute(&pool)
        .await
        .unwrap();
}

/// Twenty concurrent admissions against a cap of 5 admit exactly 5 sessions.
#[tokio::test]
#[ignore = "requires PostgreSQL (AURIX_E2E_DATABASE_URL)"]
async fn concurrent_session_limit_holds_under_contention() {
    let Some(pool) = pool().await else {
        eprintln!("AURIX_E2E_DATABASE_URL not set; skipping");
        return;
    };
    let a = app(&pool, "cap", 5).await;
    let mut users = Vec::new();
    for _ in 0..20 {
        users.push(user(&pool, a).await);
    }
    let mut tasks = Vec::new();
    for u in users.iter().copied() {
        let pool = pool.clone();
        let row = session_row(a, u, Utc::now());
        tasks.push(tokio::spawn(async move {
            queries::create_session_within_limit(&pool, &row, 5)
                .await
                .unwrap()
                .is_some()
        }));
    }
    let mut admitted = 0;
    for t in tasks {
        if t.await.unwrap() {
            admitted += 1;
        }
    }
    assert_eq!(admitted, 5);
    let (open,): (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM sessions WHERE app_id = $1 AND disconnected_at IS NULL",
    )
    .bind(a)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(open, 5);
    // A user already holding a slot on this node is replacing their session, not adding one.
    let (holder,): (Uuid,) = sqlx::query_as(
        "SELECT user_id FROM sessions WHERE app_id = $1 AND disconnected_at IS NULL LIMIT 1",
    )
    .bind(a)
    .fetch_one(&pool)
    .await
    .unwrap();
    let newcomer = user(&pool, a).await;
    assert!(
        queries::create_session_within_limit(&pool, &session_row(a, newcomer, Utc::now()), 5)
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        queries::create_session_within_limit(&pool, &session_row(a, holder, Utc::now()), 5)
            .await
            .unwrap()
            .is_some()
    );
    // Closing sessions frees slots for others.
    sqlx::query("UPDATE sessions SET disconnected_at = NOW() WHERE app_id = $1 AND user_id = $2")
        .bind(a)
        .bind(holder)
        .execute(&pool)
        .await
        .unwrap();
    assert!(
        queries::create_session_within_limit(&pool, &session_row(a, newcomer, Utc::now()), 5)
            .await
            .unwrap()
            .is_some()
    );
    assert!(queries::create_session_within_limit(
        &pool,
        &session_row(a, user(&pool, a).await, Utc::now()),
        5
    )
    .await
    .unwrap()
    .is_none());
    cleanup(&pool, &[a]).await;
}
