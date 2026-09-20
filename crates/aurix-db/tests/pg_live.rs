//! Chat history storage against a real PostgreSQL (`AURIX_E2E_DATABASE_URL`): keyset
//! pagination with equal timestamps, unread counts, read-marker monotonicity, the offline
//! inbox, tenant scoping and the deletion cascades. Every run uses fresh app ids and removes
//! them afterwards, so it can share the database with running nodes.

use aurix_db::models::{AppRow, ChatConversation, ChatMessageRow, MessageCursor, UserRow};
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
        name: format!("pg-live {name}"),
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
        external_id: format!("pg-live:{name}:{}", Uuid::new_v4()),
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

struct Msg {
    id: Uuid,
    sent_at: DateTime<Utc>,
}

#[allow(clippy::too_many_arguments)]
async fn insert(
    pool: &DbPool,
    app_id: Uuid,
    channel_id: Option<Uuid>,
    from: Uuid,
    to: Option<Uuid>,
    text: &str,
    sent_at: DateTime<Utc>,
    offline: bool,
) -> Msg {
    let row = ChatMessageRow {
        id: Uuid::now_v7(),
        app_id,
        channel_id,
        from_user_id: from,
        display_name: "pg-live".into(),
        to_user_id: to,
        text: text.into(),
        metadata: None,
        sent_at,
        offline,
    };
    queries::insert_chat_message(pool, &row)
        .await
        .expect("insert message");
    Msg {
        id: row.id,
        sent_at: row.sent_at,
    }
}

fn cursor(m: &ChatMessageRow) -> MessageCursor {
    MessageCursor::of(m)
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

/// Twelve channel messages where groups of four share one `sent_at`: paging backwards by 5
/// and forwards by 5 visits every message exactly once, cursors line up at group boundaries
/// and a foreign tenant sees nothing.
#[tokio::test]
#[ignore = "requires PostgreSQL (AURIX_E2E_DATABASE_URL)"]
async fn keyset_pagination_with_equal_timestamps() {
    let Some(pool) = pool().await else {
        eprintln!("AURIX_E2E_DATABASE_URL not set; skipping");
        return;
    };
    let app_id = app(&pool, "pagination").await;
    let other_app = app(&pool, "pagination-other").await;
    let alice = user(&pool, app_id, "alice").await;
    let channel = Uuid::new_v4();
    let base = Utc::now() - Duration::hours(1);
    let mut all = Vec::new();
    for i in 0..12 {
        let at = base + Duration::seconds(i / 4);
        all.push(
            insert(
                &pool,
                app_id,
                Some(channel),
                alice,
                None,
                &format!("m{i}"),
                at,
                false,
            )
            .await,
        );
    }
    // Same channel uuid in another tenant: must never leak into the app's pages.
    let stranger = user(&pool, other_app, "stranger").await;
    insert(
        &pool,
        other_app,
        Some(channel),
        stranger,
        None,
        "foreign",
        base,
        false,
    )
    .await;

    // Backwards: newest first, 5 + 5 + 2.
    let mut seen: Vec<Uuid> = Vec::new();
    let mut before: Option<MessageCursor> = None;
    let mut pages = 0;
    loop {
        let rows = queries::list_channel_messages(&pool, app_id, channel, before, None, false, 5)
            .await
            .unwrap();
        pages += 1;
        if rows.is_empty() {
            break;
        }
        for w in rows.windows(2) {
            assert!(
                (w[0].sent_at, w[0].id) > (w[1].sent_at, w[1].id),
                "strictly descending (sent_at, id)"
            );
        }
        seen.extend(rows.iter().map(|r| r.id));
        before = rows.last().map(cursor);
        if rows.len() < 5 {
            break;
        }
    }
    assert_eq!(pages, 3);
    let mut expected: Vec<Uuid> = all.iter().rev().map(|m| m.id).collect();
    assert_eq!(
        seen, expected,
        "no duplicates, no gaps across equal timestamps"
    );

    // Forwards from the oldest: ascending pages cover the same set once.
    seen.clear();
    let mut after: Option<MessageCursor> = Some(MessageCursor {
        sent_at: all[0].sent_at,
        id: all[0].id,
    });
    loop {
        let rows = queries::list_channel_messages(&pool, app_id, channel, None, after, true, 5)
            .await
            .unwrap();
        if rows.is_empty() {
            break;
        }
        for w in rows.windows(2) {
            assert!((w[0].sent_at, w[0].id) < (w[1].sent_at, w[1].id));
        }
        seen.extend(rows.iter().map(|r| r.id));
        after = rows.last().map(cursor);
    }
    expected.reverse();
    assert_eq!(
        seen,
        expected[1..].to_vec(),
        "forward paging resumes right after the cursor"
    );

    // A window bounded on both sides inside one equal-timestamp group.
    let rows = queries::list_channel_messages(
        &pool,
        app_id,
        channel,
        Some(MessageCursor {
            sent_at: all[7].sent_at,
            id: all[7].id,
        }),
        Some(MessageCursor {
            sent_at: all[4].sent_at,
            id: all[4].id,
        }),
        false,
        10,
    )
    .await
    .unwrap();
    let ids: Vec<Uuid> = rows.iter().map(|r| r.id).collect();
    assert_eq!(
        ids,
        vec![all[6].id, all[5].id],
        "exclusive on both ends: {ids:?}"
    );

    let foreign = queries::list_channel_messages(&pool, other_app, channel, None, None, false, 50)
        .await
        .unwrap();
    assert_eq!(foreign.len(), 1, "the other tenant only sees its own row");
    assert!(
        !queries::list_channel_messages(&pool, app_id, channel, None, None, false, 50)
            .await
            .unwrap()
            .iter()
            .any(|r| r.text == "foreign")
    );

    cleanup(&pool, &[app_id, other_app]).await;
    assert_eq!(
        count(&pool, "chat_messages", app_id).await,
        0,
        "app deletion cascades"
    );
}

/// Direct conversations: history is symmetric, unread counts follow the reader's marker,
/// markers never move backwards and re-marking is a no-op, the offline inbox drains as the
/// marker advances, and erasing a user removes their messages and markers (both sides).
#[tokio::test]
#[ignore = "requires PostgreSQL (AURIX_E2E_DATABASE_URL)"]
async fn read_markers_unread_counts_and_offline_inbox() {
    let Some(pool) = pool().await else {
        eprintln!("AURIX_E2E_DATABASE_URL not set; skipping");
        return;
    };
    let app_id = app(&pool, "markers").await;
    let alice = user(&pool, app_id, "alice").await;
    let bob = user(&pool, app_id, "bob").await;
    let carol = user(&pool, app_id, "carol").await;
    let base = Utc::now() - Duration::minutes(30);
    // Bob → Alice while she is offline: three at the same instant, then one later.
    let mut to_alice = Vec::new();
    for i in 0..3 {
        to_alice.push(
            insert(
                &pool,
                app_id,
                None,
                bob,
                Some(alice),
                &format!("b{i}"),
                base,
                true,
            )
            .await,
        );
    }
    to_alice.push(
        insert(
            &pool,
            app_id,
            None,
            bob,
            Some(alice),
            "b3",
            base + Duration::seconds(5),
            true,
        )
        .await,
    );
    // Alice → Bob (live), Carol → Alice offline, and a channel message from Bob.
    insert(
        &pool,
        app_id,
        None,
        alice,
        Some(bob),
        "a0",
        base + Duration::seconds(1),
        false,
    )
    .await;
    let from_carol = insert(
        &pool,
        app_id,
        None,
        carol,
        Some(alice),
        "c0",
        base + Duration::seconds(2),
        true,
    )
    .await;
    let channel = Uuid::new_v4();
    let ch_msg = insert(
        &pool,
        app_id,
        Some(channel),
        bob,
        None,
        "hello all",
        base,
        false,
    )
    .await;
    insert(
        &pool,
        app_id,
        Some(channel),
        alice,
        None,
        "hi",
        base + Duration::seconds(1),
        false,
    )
    .await;

    // Direct history is the same set from either side.
    let ab = queries::list_direct_messages(&pool, app_id, alice, bob, None, None, false, 50)
        .await
        .unwrap();
    let ba = queries::list_direct_messages(&pool, app_id, bob, alice, None, None, false, 50)
        .await
        .unwrap();
    assert_eq!(ab.len(), 5);
    assert_eq!(
        ab.iter().map(|r| r.id).collect::<Vec<_>>(),
        ba.iter().map(|r| r.id).collect::<Vec<_>>()
    );
    assert!(
        ab.iter().all(|r| r.text != "c0"),
        "carol's message is another conversation"
    );

    // Unread: Alice has 4 from Bob (her own a0 does not count), Bob has 1 from Alice.
    let unread = |user_id, conv, after| {
        let pool = pool.clone();
        async move {
            queries::count_unread_messages(&pool, app_id, user_id, conv, after, 1000)
                .await
                .unwrap()
        }
    };
    assert_eq!(unread(alice, ChatConversation::Direct(bob), None).await, 4);
    assert_eq!(unread(bob, ChatConversation::Direct(alice), None).await, 1);
    assert_eq!(
        unread(alice, ChatConversation::Channel(channel), None).await,
        1
    );
    assert_eq!(
        queries::count_unread_messages(
            &pool,
            app_id,
            alice,
            ChatConversation::Direct(bob),
            None,
            2
        )
        .await
        .unwrap(),
        2,
        "the cap bounds the scan"
    );

    // Offline inbox: everything queued for Alice, oldest first, across senders.
    let inbox = queries::list_unread_offline_messages(&pool, app_id, alice, None, 100)
        .await
        .unwrap();
    let texts: Vec<&str> = inbox.iter().map(|r| r.text.as_str()).collect();
    assert_eq!(texts, vec!["b0", "b1", "b2", "c0", "b3"]);
    for w in inbox.windows(2) {
        assert!((w[0].sent_at, w[0].id) < (w[1].sent_at, w[1].id));
    }
    let newest_two = queries::list_unread_offline_messages(&pool, app_id, alice, None, 2)
        .await
        .unwrap();
    assert_eq!(
        newest_two
            .iter()
            .map(|r| r.text.as_str())
            .collect::<Vec<_>>(),
        vec!["c0", "b3"],
        "a limit keeps the newest ones"
    );
    let recent = queries::list_unread_offline_messages(
        &pool,
        app_id,
        alice,
        Some(base + Duration::seconds(2)),
        100,
    )
    .await
    .unwrap();
    assert_eq!(
        recent.iter().map(|r| r.text.as_str()).collect::<Vec<_>>(),
        vec!["c0", "b3"]
    );
    assert!(
        queries::list_unread_offline_messages(&pool, app_id, bob, None, 100)
            .await
            .unwrap()
            .is_empty(),
        "live messages are not an inbox"
    );

    // Alice reads Bob's second message of the equal-timestamp group.
    let mark = |user_id, conv, m: &Msg| {
        let pool = pool.clone();
        let c = MessageCursor {
            sent_at: m.sent_at,
            id: m.id,
        };
        async move {
            queries::advance_read_marker(&pool, app_id, user_id, conv, c)
                .await
                .unwrap()
        }
    };
    let moved = mark(alice, ChatConversation::Direct(bob), &to_alice[1]).await;
    let marker = moved.expect("first mark moves");
    assert_eq!(marker.message_id, to_alice[1].id);
    assert_eq!(marker.kind, "direct");
    assert_eq!(marker.conversation_id, bob);
    assert_eq!(
        unread(
            alice,
            ChatConversation::Direct(bob),
            Some(cursor_of(&marker))
        )
        .await,
        2
    );
    let inbox = queries::list_unread_offline_messages(&pool, app_id, alice, None, 100)
        .await
        .unwrap();
    assert_eq!(
        inbox.iter().map(|r| r.text.as_str()).collect::<Vec<_>>(),
        vec!["b2", "c0", "b3"],
        "the inbox drops what the marker covers, per sender"
    );

    // Idempotent and monotonic: same message → None, older message → None, newer → moves.
    assert!(mark(alice, ChatConversation::Direct(bob), &to_alice[1])
        .await
        .is_none());
    assert!(mark(alice, ChatConversation::Direct(bob), &to_alice[0])
        .await
        .is_none());
    let again = queries::get_read_marker(&pool, app_id, alice, ChatConversation::Direct(bob))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(again.message_id, to_alice[1].id, "unchanged");
    let newer = mark(alice, ChatConversation::Direct(bob), &to_alice[3])
        .await
        .unwrap();
    assert_eq!(newer.message_id, to_alice[3].id);
    assert_eq!(
        unread(
            alice,
            ChatConversation::Direct(bob),
            Some(cursor_of(&newer))
        )
        .await,
        0
    );
    assert!(newer.read_at >= again.read_at);

    // Channel marker of Alice; Bob's and Carol's absence is reported as no row.
    mark(alice, ChatConversation::Channel(channel), &ch_msg)
        .await
        .unwrap();
    let markers = queries::list_channel_read_markers(&pool, app_id, channel, None, 100)
        .await
        .unwrap();
    assert_eq!(markers.len(), 1);
    assert_eq!(markers[0].user_id, alice);
    let only_bob = queries::list_channel_read_markers(&pool, app_id, channel, Some(&[bob]), 100)
        .await
        .unwrap();
    assert!(only_bob.is_empty());
    let mine = queries::list_user_read_markers(&pool, app_id, alice, 100)
        .await
        .unwrap();
    assert_eq!(mine.len(), 2, "direct + channel");

    // Carol's marker on her conversation with Alice; erasing Alice removes Alice's rows and
    // the markers that point at her.
    mark(carol, ChatConversation::Direct(alice), &from_carol).await;
    let counts = queries::erase_user(&pool, app_id, alice, false)
        .await
        .unwrap()
        .expect("alice exists");
    assert_eq!(
        counts.chat_messages, 7,
        "4 from bob + a0 + c0 + her channel message"
    );
    assert!(queries::list_user_read_markers(&pool, app_id, alice, 100)
        .await
        .unwrap()
        .is_empty());
    assert!(
        queries::get_read_marker(&pool, app_id, carol, ChatConversation::Direct(alice))
            .await
            .unwrap()
            .is_none(),
        "peers' markers about the erased user go too"
    );
    let left = queries::list_channel_messages(&pool, app_id, channel, None, None, false, 50)
        .await
        .unwrap();
    assert_eq!(left.len(), 1, "bob's channel message survives");

    cleanup(&pool, &[app_id]).await;
    assert_eq!(count(&pool, "chat_messages", app_id).await, 0);
    assert_eq!(count(&pool, "chat_read_markers", app_id).await, 0);
}

fn cursor_of(m: &aurix_db::models::ChatReadMarkerRow) -> MessageCursor {
    MessageCursor {
        sent_at: m.message_sent_at,
        id: m.message_id,
    }
}
