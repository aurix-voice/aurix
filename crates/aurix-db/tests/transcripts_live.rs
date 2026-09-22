//! Stored transcripts (`stt.persist`) against a real PostgreSQL (`AURIX_E2E_DATABASE_URL`):
//! keyset pagination with equal timestamps, translations keyed by the live event id (first
//! writer wins, never for a missing source), tenant scoping, retention, deletion cascades and
//! user erasure. Fresh app ids per run, removed afterwards, so it can share the database with
//! running nodes.

use aurix_db::models::{AppRow, TranscriptCursor, TranscriptRow, UserRow};
use aurix_db::queries;
use aurix_db::DbPool;
use chrono::{DateTime, Duration, Utc};
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

async fn app(pool: &DbPool, name: &str) -> Uuid {
    let now = Utc::now();
    let row = AppRow {
        id: Uuid::new_v4(),
        name: format!("transcripts-live {name}"),
        description: None,
        owner_id: Uuid::new_v4(),
        api_key_hash: format!("test-{}", Uuid::new_v4()),
        api_secret_hash: format!("test-{}", Uuid::new_v4()),
        active: true,
        max_channels: 10,
        max_participants_per_channel: 10,
        max_concurrent_sessions: 0,
        monthly_participant_minutes: 0,
        created_at: now,
        updated_at: now,
    };
    queries::create_app(pool, &row)
        .await
        .expect("create app")
        .id
}

async fn user(pool: &DbPool, app_id: Uuid, name: &str) -> Uuid {
    let now = Utc::now();
    let row = UserRow {
        id: Uuid::new_v4(),
        app_id,
        external_id: format!("transcripts-live:{name}:{}", Uuid::new_v4()),
        display_name: name.into(),
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
    queries::upsert_user(pool, &row)
        .await
        .expect("upsert user")
        .id
}

async fn insert(
    pool: &DbPool,
    app_id: Uuid,
    channel_id: Uuid,
    user_id: Uuid,
    text: &str,
    started_at: DateTime<Utc>,
) -> TranscriptRow {
    let row = TranscriptRow {
        id: Uuid::new_v4(),
        app_id,
        channel_id,
        user_id,
        text: text.into(),
        language: Some("en".into()),
        started_at,
        duration_ms: 1_200,
        words: Some(serde_json::json!([{"word": text, "start_ms": 0, "end_ms": 1200}])),
        node_id: Some(Uuid::new_v4()),
        created_at: Utc::now(),
    };
    queries::insert_transcript(pool, &row)
        .await
        .expect("insert transcript");
    row
}

fn cursor(t: &TranscriptRow) -> TranscriptCursor {
    TranscriptCursor {
        started_at: t.started_at,
        id: t.id,
    }
}

async fn cleanup(pool: &DbPool, apps: &[Uuid]) {
    for app_id in apps {
        sqlx::query("DELETE FROM users WHERE app_id = $1")
            .bind(app_id)
            .execute(pool)
            .await
            .unwrap();
        sqlx::query("DELETE FROM apps WHERE id = $1")
            .bind(app_id)
            .execute(pool)
            .await
            .unwrap();
    }
}

async fn count(pool: &DbPool, table: &str, app_id: Uuid) -> i64 {
    let (n,): (i64,) = sqlx::query_as(&format!("SELECT COUNT(*) FROM {table} WHERE app_id = $1"))
        .bind(app_id)
        .fetch_one(pool)
        .await
        .unwrap();
    n
}

async fn translation_count(pool: &DbPool, app_id: Uuid) -> i64 {
    let (n,): (i64,) = sqlx::query_as(
        r#"SELECT COUNT(*) FROM transcript_translations tt
           JOIN transcripts t ON t.id = tt.transcript_id WHERE t.app_id = $1"#,
    )
    .bind(app_id)
    .fetch_one(pool)
    .await
    .unwrap();
    n
}

/// Twelve transcripts of two speakers where groups of four share one `started_at`: paging
/// backwards by 5 and forwards by 5 visits each exactly once, the speaker filter and the user
/// listing agree, and a foreign tenant sees nothing.
#[tokio::test]
#[ignore = "requires PostgreSQL (AURIX_E2E_DATABASE_URL)"]
async fn keyset_pagination_with_equal_timestamps() {
    let Some(pool) = pool().await else {
        eprintln!("AURIX_E2E_DATABASE_URL not set; skipping");
        return;
    };
    let app_id = app(&pool, "pagination").await;
    let other = app(&pool, "pagination-other").await;
    let channel = Uuid::new_v4();
    let alice = user(&pool, app_id, "alice").await;
    let bob = user(&pool, app_id, "bob").await;
    let base = Utc::now() - Duration::minutes(10);
    let mut all = Vec::new();
    for i in 0..12u32 {
        let speaker = if i % 3 == 0 { bob } else { alice };
        let t = insert(
            &pool,
            app_id,
            channel,
            speaker,
            &format!("segment {i}"),
            base + Duration::seconds(i64::from(i / 4)),
        )
        .await;
        all.push(t);
    }
    // Same channel id, foreign tenant: must never surface below.
    insert(&pool, other, channel, alice, "foreign", base).await;
    let rows =
        |mut rows: Vec<TranscriptRow>| -> Vec<Uuid> { rows.drain(..).map(|r| r.id).collect() };

    // Backwards from the present, 5 at a time.
    let mut seen = Vec::new();
    let mut before: Option<TranscriptCursor> = None;
    loop {
        let page =
            queries::list_channel_transcripts(&pool, app_id, channel, None, before, None, false, 6)
                .await
                .unwrap();
        let more = page.len() > 5;
        let page: Vec<TranscriptRow> = page.into_iter().take(5).collect();
        for w in page.windows(2) {
            assert!(
                (w[0].started_at, w[0].id) > (w[1].started_at, w[1].id),
                "newest first, ties broken by id"
            );
        }
        before = page.last().map(cursor);
        seen.extend(rows(page));
        if !more {
            break;
        }
    }
    let mut expected: Vec<Uuid> = all.iter().map(|t| t.id).collect();
    let mut got = seen.clone();
    expected.sort();
    got.sort();
    assert_eq!(got, expected, "every row exactly once");
    assert_eq!(seen.len(), 12);

    // Forwards from the oldest, 5 at a time, lands on the same set in reverse.
    let oldest = seen.last().unwrap();
    let oldest_row = all.iter().find(|t| t.id == *oldest).unwrap();
    let mut forward = vec![*oldest];
    let mut after = Some(cursor(oldest_row));
    loop {
        let page =
            queries::list_channel_transcripts(&pool, app_id, channel, None, None, after, true, 6)
                .await
                .unwrap();
        let more = page.len() > 5;
        let page: Vec<TranscriptRow> = page.into_iter().take(5).collect();
        for w in page.windows(2) {
            assert!((w[0].started_at, w[0].id) < (w[1].started_at, w[1].id));
        }
        after = page.last().map(cursor);
        forward.extend(rows(page));
        if !more {
            break;
        }
    }
    let mut back = seen.clone();
    back.reverse();
    assert_eq!(forward, back);

    // Speaker filter and the per-user listing agree.
    let bobs =
        queries::list_channel_transcripts(&pool, app_id, channel, Some(bob), None, None, false, 50)
            .await
            .unwrap();
    assert_eq!(bobs.len(), 4);
    assert!(bobs.iter().all(|t| t.user_id == bob));
    let bobs_user = queries::list_user_transcripts(&pool, app_id, bob, None, None, false, 50)
        .await
        .unwrap();
    assert_eq!(rows(bobs), rows(bobs_user));
    let alices = queries::export_user_transcripts(&pool, app_id, alice, 100)
        .await
        .unwrap();
    assert_eq!(alices.len(), 8);

    // Words round-trip as JSON; nothing leaks across tenants.
    let stored = alices
        .iter()
        .find(|t| t.id == all[1].id)
        .expect("alice's row");
    assert_eq!(stored.words, all[1].words);
    assert_eq!(stored.language.as_deref(), Some("en"));
    assert_eq!(stored.duration_ms, 1_200);
    let foreign =
        queries::list_channel_transcripts(&pool, other, channel, None, None, None, false, 50)
            .await
            .unwrap();
    assert_eq!(foreign.len(), 1);
    assert_eq!(foreign[0].text, "foreign");
    assert!(
        queries::list_user_transcripts(&pool, other, alice, None, None, false, 50)
            .await
            .unwrap()
            .iter()
            .all(|t| t.text == "foreign"),
        "the user listing is tenant-scoped even for a shared user id"
    );

    cleanup(&pool, &[app_id, other]).await;
    assert_eq!(count(&pool, "transcripts", app_id).await, 0, "apps cascade");
}

/// Translations attach to the live event id: first writer wins per language, a missing or
/// foreign source stores nothing, and they go with the source row (single delete, channel
/// delete, retention sweep, user erasure).
#[tokio::test]
#[ignore = "requires PostgreSQL (AURIX_E2E_DATABASE_URL)"]
async fn translations_retention_and_erasure() {
    let Some(pool) = pool().await else {
        eprintln!("AURIX_E2E_DATABASE_URL not set; skipping");
        return;
    };
    let app_id = app(&pool, "translations").await;
    let other = app(&pool, "translations-other").await;
    let channel = Uuid::new_v4();
    let channel2 = Uuid::new_v4();
    let alice = user(&pool, app_id, "alice").await;
    let bob = user(&pool, app_id, "bob").await;
    let now = Utc::now();

    let old = insert(
        &pool,
        app_id,
        channel,
        alice,
        "old",
        now - Duration::days(40),
    )
    .await;
    let fresh = insert(
        &pool,
        app_id,
        channel,
        alice,
        "fresh",
        now - Duration::minutes(1),
    )
    .await;
    let bobs = insert(&pool, app_id, channel, bob, "bob", now).await;
    let elsewhere = insert(&pool, app_id, channel2, alice, "elsewhere", now).await;

    // Idempotent insert (the same event replayed by a retry).
    queries::insert_transcript(&pool, &fresh).await.unwrap();
    assert_eq!(count(&pool, "transcripts", app_id).await, 4);

    // Translations: first writer wins, per language, tenant-checked, never for a missing row.
    assert!(
        queries::insert_transcript_translation(&pool, app_id, fresh.id, "de", "frisch")
            .await
            .unwrap()
    );
    assert!(
        !queries::insert_transcript_translation(&pool, app_id, fresh.id, "de", "later node")
            .await
            .unwrap(),
        "second writer for the same language is ignored"
    );
    assert!(
        queries::insert_transcript_translation(&pool, app_id, fresh.id, "fr", "frais")
            .await
            .unwrap()
    );
    assert!(
        !queries::insert_transcript_translation(&pool, other, fresh.id, "it", "fresco")
            .await
            .unwrap(),
        "a foreign tenant cannot attach translations"
    );
    assert!(
        !queries::insert_transcript_translation(&pool, app_id, Uuid::new_v4(), "de", "x")
            .await
            .unwrap(),
        "no source row, no translation"
    );
    queries::insert_transcript_translation(&pool, app_id, old.id, "de", "alt")
        .await
        .unwrap();
    queries::insert_transcript_translation(&pool, app_id, bobs.id, "de", "bob")
        .await
        .unwrap();
    let joined = queries::list_transcript_translations(&pool, &[fresh.id, bobs.id, Uuid::new_v4()])
        .await
        .unwrap();
    let mut fresh_langs: Vec<&str> = joined
        .iter()
        .filter(|t| t.transcript_id == fresh.id)
        .map(|t| t.language.as_str())
        .collect();
    fresh_langs.sort_unstable();
    assert_eq!(fresh_langs, ["de", "fr"]);
    assert_eq!(
        joined
            .iter()
            .find(|t| t.transcript_id == fresh.id && t.language == "de")
            .map(|t| t.text.as_str()),
        Some("frisch")
    );
    assert_eq!(
        joined.iter().filter(|t| t.transcript_id == bobs.id).count(),
        1
    );
    assert_eq!(translation_count(&pool, app_id).await, 4);
    assert!(queries::list_transcript_translations(&pool, &[])
        .await
        .unwrap()
        .is_empty());

    // Retention: only rows that started before the cutoff, translations cascade, batched.
    let cutoff = now - Duration::days(30);
    assert_eq!(
        queries::delete_transcripts_before(&pool, cutoff, 1)
            .await
            .unwrap(),
        1
    );
    assert_eq!(
        queries::delete_transcripts_before(&pool, cutoff, 1)
            .await
            .unwrap(),
        0
    );
    assert_eq!(count(&pool, "transcripts", app_id).await, 3);
    assert_eq!(translation_count(&pool, app_id).await, 3);

    // Single delete is tenant-scoped and reports whether it hit.
    assert!(!queries::delete_transcript(&pool, other, fresh.id)
        .await
        .unwrap());
    assert!(queries::delete_transcript(&pool, app_id, fresh.id)
        .await
        .unwrap());
    assert!(!queries::delete_transcript(&pool, app_id, fresh.id)
        .await
        .unwrap());
    assert_eq!(translation_count(&pool, app_id).await, 1);

    // Channel delete leaves other channels alone.
    assert_eq!(
        queries::delete_channel_transcripts(&pool, other, channel)
            .await
            .unwrap(),
        0
    );
    assert_eq!(
        queries::delete_channel_transcripts(&pool, app_id, channel)
            .await
            .unwrap(),
        1
    );
    assert_eq!(count(&pool, "transcripts", app_id).await, 1);
    assert_eq!(translation_count(&pool, app_id).await, 0);

    // User erasure removes what the user said (and counts it), nothing of anyone else.
    insert(&pool, app_id, channel2, bob, "bob again", now).await;
    let counts = queries::erase_user(&pool, app_id, alice, false)
        .await
        .unwrap()
        .expect("alice exists");
    assert_eq!(counts.transcripts, 1, "{counts:?}");
    let left =
        queries::list_channel_transcripts(&pool, app_id, channel2, None, None, None, false, 10)
            .await
            .unwrap();
    assert_eq!(left.len(), 1);
    assert_eq!((left[0].user_id, left[0].text.as_str()), (bob, "bob again"));
    assert_ne!(left[0].id, elsewhere.id);

    cleanup(&pool, &[app_id, other]).await;
    assert_eq!(count(&pool, "transcripts", app_id).await, 0);
}
