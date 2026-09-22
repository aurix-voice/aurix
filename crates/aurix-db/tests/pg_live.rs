//! Chat history storage against a real PostgreSQL (`AURIX_E2E_DATABASE_URL`): keyset
//! pagination with equal timestamps, unread counts, read-marker monotonicity, the offline
//! inbox, tenant scoping and the deletion cascades. Every run uses fresh app ids and removes
//! them afterwards, so it can share the database with running nodes.

use aurix_db::models::{
    AppRow, ChatConversation, ChatMessageRow, ChatSearchScope, MessageCursor, ReactionAdd, UserRow,
};
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

#[derive(Clone)]
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
        edited_at: None,
        deleted_at: None,
        deleted_by: None,
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

/// Edits keep the row's cursor and stamp `edited_at`; a deletion is a tombstone (empty text, no
/// metadata, reactions gone, `deleted_by` recorded) that stays in history at its position but
/// leaves search, unread counts and the offline inbox; reactions are idempotent per
/// (user, reaction), capped per message in distinct kinds, counted exactly and listed with a
/// bounded reader-first user list; search is tenant-, conversation- and author-scoped, pages
/// backwards by keyset and treats a term-less query as "nothing to search".
#[tokio::test]
#[ignore = "requires PostgreSQL (AURIX_E2E_DATABASE_URL)"]
async fn edits_tombstones_reactions_and_search() {
    let Some(pool) = pool().await else {
        eprintln!("AURIX_E2E_DATABASE_URL not set; skipping");
        return;
    };
    let app_id = app(&pool, "edits").await;
    let other_app = app(&pool, "edits-other").await;
    let alice = user(&pool, app_id, "alice").await;
    let bob = user(&pool, app_id, "bob").await;
    let carol = user(&pool, app_id, "carol").await;
    let stranger = user(&pool, other_app, "stranger").await;
    let channel = Uuid::new_v4();
    // Whole seconds: PostgreSQL stores microseconds, and the test compares round-tripped stamps.
    let base = DateTime::<Utc>::from_timestamp(Utc::now().timestamp() - 600, 0).unwrap();

    // Channel: three "loot" messages (two by Alice, one by Bob) plus one unrelated.
    let loot1 = insert(
        &pool,
        app_id,
        Some(channel),
        alice,
        None,
        "loot drop at the bridge",
        base,
        false,
    )
    .await;
    let loot2 = insert(
        &pool,
        app_id,
        Some(channel),
        bob,
        None,
        "who took the loot?",
        base + Duration::seconds(1),
        false,
    )
    .await;
    let loot3 = insert(
        &pool,
        app_id,
        Some(channel),
        alice,
        None,
        "LOOT is mine, GG",
        base + Duration::seconds(2),
        false,
    )
    .await;
    let other = insert(
        &pool,
        app_id,
        Some(channel),
        bob,
        None,
        "regroup at spawn",
        base + Duration::seconds(3),
        false,
    )
    .await;
    // Direct: Alice ↔ Bob and Carol → Alice (offline), all mentioning loot; a foreign tenant too.
    let dm_ab = insert(
        &pool,
        app_id,
        None,
        alice,
        Some(bob),
        "keep the loot",
        base,
        false,
    )
    .await;
    let dm_ba = insert(
        &pool,
        app_id,
        None,
        bob,
        Some(alice),
        "loot split 50/50",
        base + Duration::seconds(1),
        false,
    )
    .await;
    let dm_ca = insert(
        &pool,
        app_id,
        None,
        carol,
        Some(alice),
        "need loot too",
        base + Duration::seconds(2),
        true,
    )
    .await;
    insert(
        &pool,
        other_app,
        Some(channel),
        stranger,
        None,
        "loot loot loot",
        base,
        false,
    )
    .await;

    // ── edit ──
    let edited = queries::update_chat_message_text(
        &pool,
        app_id,
        loot2.id,
        "who took the epic loot?",
        &Some(serde_json::json!({"k": 1})),
    )
    .await
    .unwrap()
    .expect("edited row");
    assert_eq!(edited.text, "who took the epic loot?");
    assert_eq!(edited.metadata, Some(serde_json::json!({"k": 1})));
    assert!(edited.edited_at.is_some() && edited.deleted_at.is_none());
    assert_eq!(
        edited.sent_at, loot2.sent_at,
        "edit keeps the original position"
    );
    assert!(
        queries::update_chat_message_text(&pool, other_app, loot2.id, "x", &None)
            .await
            .unwrap()
            .is_none(),
        "another tenant cannot edit by id"
    );
    assert_eq!(
        queries::get_chat_message(&pool, app_id, loot2.id)
            .await
            .unwrap()
            .unwrap()
            .text,
        "who took the epic loot?"
    );

    // ── reactions ──
    let add = |msg: Uuid, who: Uuid, r: &'static str| {
        let pool = pool.clone();
        async move {
            queries::add_chat_reaction(&pool, app_id, msg, who, r, 2)
                .await
                .unwrap()
        }
    };
    assert_eq!(add(loot1.id, bob, "+1").await, ReactionAdd::Added);
    assert_eq!(
        add(loot1.id, bob, "+1").await,
        ReactionAdd::AlreadySet,
        "duplicate add is a no-op"
    );
    assert_eq!(add(loot1.id, carol, "+1").await, ReactionAdd::Added);
    assert_eq!(add(loot1.id, alice, "+1").await, ReactionAdd::Added);
    assert_eq!(add(loot1.id, alice, "fire").await, ReactionAdd::Added);
    assert_eq!(
        add(loot1.id, bob, "skull").await,
        ReactionAdd::TooManyDistinct,
        "third distinct reaction exceeds max_distinct = 2"
    );
    assert_eq!(
        add(loot1.id, bob, "fire").await,
        ReactionAdd::Added,
        "an existing kind is still open"
    );
    assert_eq!(
        queries::count_chat_reaction(&pool, app_id, loot1.id, "+1")
            .await
            .unwrap(),
        3
    );
    assert_eq!(
        queries::count_chat_reaction(&pool, app_id, loot1.id, "fire")
            .await
            .unwrap(),
        2
    );
    assert_eq!(
        queries::count_chat_reaction(&pool, app_id, loot1.id, "skull")
            .await
            .unwrap(),
        0
    );
    assert_eq!(
        queries::count_chat_reaction(&pool, other_app, loot1.id, "+1")
            .await
            .unwrap(),
        0,
        "counts are tenant-scoped"
    );
    assert!(
        queries::remove_chat_reaction(&pool, app_id, loot1.id, carol, "+1")
            .await
            .unwrap()
    );
    assert!(
        !queries::remove_chat_reaction(&pool, app_id, loot1.id, carol, "+1")
            .await
            .unwrap(),
        "duplicate remove is a no-op"
    );
    assert!(
        !queries::remove_chat_reaction(&pool, other_app, loot1.id, bob, "+1")
            .await
            .unwrap(),
        "another tenant cannot remove"
    );
    assert_eq!(
        queries::count_chat_reaction(&pool, app_id, loot1.id, "+1")
            .await
            .unwrap(),
        2
    );
    // Bounded, reader-first user list: shown = 1 lists only the reader when they reacted.
    let tallies =
        queries::list_chat_reactions(&pool, app_id, &[loot1.id, loot2.id], Some(alice), 1)
            .await
            .unwrap();
    let plus = tallies
        .iter()
        .find(|t| t.message_id == loot1.id && t.reaction == "+1")
        .expect("+1 tally");
    assert_eq!(plus.count, 2);
    assert_eq!(
        plus.user_ids,
        vec![alice],
        "reader first, list bounded to `shown`"
    );
    let fire = tallies
        .iter()
        .find(|t| t.message_id == loot1.id && t.reaction == "fire")
        .expect("fire tally");
    assert_eq!(fire.count, 2);
    assert_eq!(fire.user_ids.len(), 1);
    assert!(
        !tallies.iter().any(|t| t.message_id == loot2.id),
        "messages without reactions have no tally"
    );
    let full = queries::list_chat_reactions(&pool, app_id, &[loot1.id], Some(bob), 20)
        .await
        .unwrap();
    let plus_full = full.iter().find(|t| t.reaction == "+1").unwrap();
    assert_eq!(plus_full.user_ids.len(), 2);
    assert_eq!(plus_full.user_ids[0], bob, "the reader comes first");
    assert!(
        queries::list_chat_reactions(&pool, other_app, &[loot1.id], None, 20)
            .await
            .unwrap()
            .is_empty(),
        "tallies are tenant-scoped"
    );

    // ── search ──
    let search = |scope: ChatSearchScope,
                  q: &'static str,
                  from: Option<Uuid>,
                  before: Option<MessageCursor>,
                  limit: i64| {
        let pool = pool.clone();
        async move {
            queries::search_chat_messages(&pool, app_id, scope, q, from, before, limit)
                .await
                .unwrap()
        }
    };
    let hits = search(ChatSearchScope::Channel(channel), "loot", None, None, 10)
        .await
        .expect("query has terms");
    assert_eq!(
        hits.iter().map(|m| m.id).collect::<Vec<_>>(),
        vec![loot3.id, loot2.id, loot1.id],
        "newest first, case-insensitive, the edited text still matches"
    );
    assert!(
        !hits.iter().any(|m| m.id == other.id),
        "non-matching messages are not returned"
    );
    let alice_only = search(
        ChatSearchScope::Channel(channel),
        "loot",
        Some(alice),
        None,
        10,
    )
    .await
    .unwrap();
    assert_eq!(
        alice_only.iter().map(|m| m.id).collect::<Vec<_>>(),
        vec![loot3.id, loot1.id]
    );
    let page1 = search(ChatSearchScope::Channel(channel), "loot", None, None, 2)
        .await
        .unwrap();
    assert_eq!(page1.len(), 2);
    let page2 = search(
        ChatSearchScope::Channel(channel),
        "loot",
        None,
        Some(cursor(&page1[1])),
        2,
    )
    .await
    .unwrap();
    assert_eq!(
        page2.iter().map(|m| m.id).collect::<Vec<_>>(),
        vec![loot1.id],
        "keyset paging continues past the last hit"
    );
    let phrase = search(
        ChatSearchScope::Channel(channel),
        "\"epic loot\" -mine",
        None,
        None,
        10,
    )
    .await
    .unwrap();
    assert_eq!(
        phrase.iter().map(|m| m.id).collect::<Vec<_>>(),
        vec![loot2.id],
        "web-search syntax: phrase + exclusion"
    );
    assert!(
        search(ChatSearchScope::Channel(channel), "-", None, None, 10)
            .await
            .is_none(),
        "a query without searchable terms is None"
    );
    assert!(
        search(
            ChatSearchScope::Channel(Uuid::new_v4()),
            "loot",
            None,
            None,
            10
        )
        .await
        .unwrap()
        .is_empty(),
        "unknown channel finds nothing"
    );
    assert!(
        queries::search_chat_messages(
            &pool,
            other_app,
            ChatSearchScope::Channel(channel),
            "loot",
            None,
            None,
            10
        )
        .await
        .unwrap()
        .unwrap()
        .iter()
        .all(|m| m.from_user_id == stranger),
        "the other tenant only ever sees its own rows"
    );
    let direct = search(ChatSearchScope::Direct(alice, bob), "loot", None, None, 10)
        .await
        .unwrap();
    assert_eq!(
        direct.iter().map(|m| m.id).collect::<Vec<_>>(),
        vec![dm_ba.id, dm_ab.id],
        "both directions of one conversation"
    );
    let all_of_alice = search(ChatSearchScope::User(alice), "loot", None, None, 10)
        .await
        .unwrap();
    assert_eq!(
        all_of_alice.iter().map(|m| m.id).collect::<Vec<_>>(),
        vec![dm_ca.id, dm_ba.id, dm_ab.id],
        "every direct conversation of the user, no channel messages"
    );
    let all_of_carol = search(ChatSearchScope::User(carol), "loot", None, None, 10)
        .await
        .unwrap();
    assert_eq!(
        all_of_carol.iter().map(|m| m.id).collect::<Vec<_>>(),
        vec![dm_ca.id]
    );

    // ── delete → tombstone ──
    let unread_before = queries::count_unread_messages(
        &pool,
        app_id,
        alice,
        ChatConversation::Direct(carol),
        None,
        100,
    )
    .await
    .unwrap();
    assert_eq!(unread_before, 1);
    assert_eq!(
        queries::list_unread_offline_messages(&pool, app_id, alice, None, 100)
            .await
            .unwrap()
            .len(),
        1
    );
    let tomb = queries::delete_chat_message(&pool, app_id, dm_ca.id, carol)
        .await
        .unwrap()
        .expect("deleted row");
    assert_eq!(tomb.text, "");
    assert!(tomb.metadata.is_none());
    assert!(tomb.deleted_at.is_some());
    assert_eq!(tomb.deleted_by, Some(carol));
    assert_eq!(
        tomb.sent_at, dm_ca.sent_at,
        "the tombstone keeps its position"
    );
    assert!(
        queries::delete_chat_message(&pool, app_id, dm_ca.id, alice)
            .await
            .unwrap()
            .is_none(),
        "a second delete changes nothing"
    );
    assert_eq!(
        queries::count_unread_messages(
            &pool,
            app_id,
            alice,
            ChatConversation::Direct(carol),
            None,
            100
        )
        .await
        .unwrap(),
        0,
        "deleted messages do not count as unread"
    );
    assert!(
        queries::list_unread_offline_messages(&pool, app_id, alice, None, 100)
            .await
            .unwrap()
            .is_empty(),
        "deleted messages are not replayed"
    );
    assert!(
        search(ChatSearchScope::User(carol), "loot", None, None, 10)
            .await
            .unwrap()
            .is_empty(),
        "deleted messages leave search"
    );
    // Reactions vanish with the message; the tombstone stays in history at its place.
    assert_eq!(add(loot1.id, bob, "+1").await, ReactionAdd::AlreadySet);
    let tomb_ch = queries::delete_chat_message(&pool, app_id, loot1.id, Uuid::nil())
        .await
        .unwrap()
        .expect("channel tombstone");
    assert_eq!(
        tomb_ch.deleted_by,
        Some(Uuid::nil()),
        "operator deletion records the nil user"
    );
    assert_eq!(
        queries::count_chat_reaction(&pool, app_id, loot1.id, "+1")
            .await
            .unwrap(),
        0
    );
    assert!(
        queries::list_chat_reactions(&pool, app_id, &[loot1.id], None, 20)
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        add(loot1.id, bob, "+1").await,
        ReactionAdd::Added,
        "the storage layer does not refuse reactions on tombstones — ChatService::react does (live_message)"
    );
    let history = queries::list_channel_messages(&pool, app_id, channel, None, None, false, 50)
        .await
        .unwrap();
    assert_eq!(
        history.iter().map(|m| m.id).collect::<Vec<_>>(),
        vec![other.id, loot3.id, loot2.id, loot1.id],
        "history keeps the tombstone in place"
    );
    let first = history.iter().find(|m| m.id == loot1.id).unwrap();
    assert!(first.deleted_at.is_some() && first.text.is_empty());
    let after_tomb =
        queries::list_channel_messages(&pool, app_id, channel, None, Some(cursor(first)), true, 50)
            .await
            .unwrap();
    assert_eq!(after_tomb.len(), 3, "the tombstone still works as a cursor");
    assert_eq!(
        search(ChatSearchScope::Channel(channel), "loot", None, None, 10)
            .await
            .unwrap()
            .iter()
            .map(|m| m.id)
            .collect::<Vec<_>>(),
        vec![loot3.id, loot2.id]
    );

    cleanup(&pool, &[app_id, other_app]).await;
    assert_eq!(count(&pool, "chat_messages", app_id).await, 0);
    assert_eq!(count(&pool, "chat_reactions", app_id).await, 0);
}

/// Per-device delivery cursors: created by the first acknowledgement of a directed message
/// addressed to the user (channel messages, other users' messages, unknown ids and other
/// tenants never create or move one), ordered by `(sent_at, id)` so equal timestamps resolve
/// by id, forward-only (older acks only refresh `updated_at`), tombstones still count, the
/// replay after a cursor is exactly the newer directed messages, the sweep drops idle devices
/// only, and erasing the user takes the cursors along.
#[tokio::test]
#[ignore = "requires PostgreSQL (AURIX_E2E_DATABASE_URL)"]
async fn per_device_cursors() {
    let Some(pool) = pool().await else {
        eprintln!("AURIX_E2E_DATABASE_URL not set; skipping");
        return;
    };
    let app_id = app(&pool, "devices").await;
    let other_app = app(&pool, "devices-other").await;
    let alice = user(&pool, app_id, "alice").await;
    let bob = user(&pool, app_id, "bob").await;
    let carol = user(&pool, app_id, "carol").await;
    let stranger = user(&pool, other_app, "stranger").await;
    let channel = Uuid::new_v4();
    let base = DateTime::<Utc>::from_timestamp(Utc::now().timestamp() - 600, 0).unwrap();

    // Bob → Alice: d0 < d1 == d2 (same second, ordered by id) < d3; Bob → Carol; a channel message.
    let d0 = insert(&pool, app_id, None, bob, Some(alice), "d0", base, true).await;
    let mut same = [
        insert(
            &pool,
            app_id,
            None,
            bob,
            Some(alice),
            "d1",
            base + Duration::seconds(1),
            true,
        )
        .await,
        insert(
            &pool,
            app_id,
            None,
            bob,
            Some(alice),
            "d2",
            base + Duration::seconds(1),
            true,
        )
        .await,
    ];
    same.sort_by_key(|m| m.id);
    let (d1, d2) = (same[0].clone(), same[1].clone());
    let d3 = insert(
        &pool,
        app_id,
        None,
        bob,
        Some(alice),
        "d3",
        base + Duration::seconds(2),
        false,
    )
    .await;
    let to_carol = insert(&pool, app_id, None, bob, Some(carol), "c0", base, true).await;
    let in_channel = insert(&pool, app_id, Some(channel), bob, None, "hi", base, false).await;
    let foreign = insert(
        &pool,
        other_app,
        None,
        stranger,
        Some(stranger),
        "x",
        base,
        true,
    )
    .await;
    let advance = |user: Uuid, device: &'static str, message: Uuid| {
        let pool = pool.clone();
        async move {
            queries::advance_chat_device_cursor(&pool, app_id, user, device, message)
                .await
                .unwrap()
        }
    };

    // Nothing that is not a directed message to Alice creates a cursor.
    for (why, id) in [
        ("unknown", Uuid::new_v4()),
        ("channel message", in_channel.id),
        ("carol's message", to_carol.id),
        ("other tenant", foreign.id),
    ] {
        assert!(advance(alice, "phone", id).await.is_none(), "{why}");
    }
    assert!(
        queries::get_chat_device_cursor(&pool, app_id, alice, "phone")
            .await
            .unwrap()
            .is_none()
    );

    // First ack creates the cursor at that message with the message's own timestamp.
    let c = advance(alice, "phone", d1.id).await.expect("d1 is alice's");
    assert_eq!((c.message_id, c.message_sent_at), (d1.id, d1.sent_at));
    let created = c.created_at;
    // Same timestamp, larger id: moves forward.
    let c = advance(alice, "phone", d2.id).await.unwrap();
    assert_eq!((c.message_id, c.message_sent_at), (d2.id, d2.sent_at));
    // Older acknowledgements keep the cursor but refresh `updated_at`.
    let before = queries::get_chat_device_cursor(&pool, app_id, alice, "phone")
        .await
        .unwrap()
        .unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    for older in [d1.id, d0.id] {
        let c = advance(alice, "phone", older).await.unwrap();
        assert_eq!((c.message_id, c.message_sent_at), (d2.id, d2.sent_at));
        assert!(c.updated_at > before.updated_at, "activity is recorded");
        assert_eq!(c.created_at, created);
    }
    // A tombstone is still acknowledgeable (the client saw the deletion).
    queries::delete_chat_message(&pool, app_id, d3.id, bob)
        .await
        .unwrap()
        .expect("d3 exists");
    let c = advance(alice, "phone", d3.id).await.unwrap();
    assert_eq!(c.message_id, d3.id);

    // Devices are independent; the replay after a cursor is exactly what is newer.
    let c = advance(alice, "pc", d0.id).await.unwrap();
    assert_eq!(c.message_id, d0.id);
    let after_pc = queries::list_direct_messages_after(
        &pool,
        app_id,
        alice,
        Some(MessageCursor {
            sent_at: c.message_sent_at,
            id: c.message_id,
        }),
        None,
        50,
    )
    .await
    .unwrap();
    assert_eq!(
        after_pc.iter().map(|m| m.id).collect::<Vec<_>>(),
        vec![d1.id, d2.id],
        "d0 acked, d3 is a tombstone"
    );
    let phone = queries::get_chat_device_cursor(&pool, app_id, alice, "phone")
        .await
        .unwrap()
        .unwrap();
    assert!(queries::list_direct_messages_after(
        &pool,
        app_id,
        alice,
        Some(MessageCursor {
            sent_at: phone.message_sent_at,
            id: phone.message_id,
        }),
        None,
        50,
    )
    .await
    .unwrap()
    .is_empty());
    // Same device id under another user or tenant is a different cursor.
    assert!(
        queries::get_chat_device_cursor(&pool, app_id, carol, "phone")
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        queries::get_chat_device_cursor(&pool, other_app, alice, "phone")
            .await
            .unwrap()
            .is_none()
    );
    let devices = queries::list_chat_device_cursors(&pool, app_id, alice)
        .await
        .unwrap();
    assert_eq!(
        devices
            .iter()
            .map(|d| d.device_id.as_str())
            .collect::<Vec<_>>(),
        vec!["pc", "phone"],
        "most recently active first"
    );

    // Retention: only idle devices are swept.
    sqlx::query(
        "UPDATE chat_device_cursors SET updated_at = NOW() - INTERVAL '100 days' \
         WHERE app_id = $1 AND device_id = 'phone'",
    )
    .bind(app_id)
    .execute(&pool)
    .await
    .unwrap();
    let swept = queries::delete_chat_device_cursors_before(&pool, Utc::now() - Duration::days(90))
        .await
        .unwrap();
    assert!(swept >= 1);
    assert!(
        queries::get_chat_device_cursor(&pool, app_id, alice, "phone")
            .await
            .unwrap()
            .is_none()
    );
    assert!(queries::get_chat_device_cursor(&pool, app_id, alice, "pc")
        .await
        .unwrap()
        .is_some());
    // Forgetting a device is idempotent.
    assert!(
        queries::delete_chat_device_cursor(&pool, app_id, alice, "pc")
            .await
            .unwrap()
    );
    assert!(
        !queries::delete_chat_device_cursor(&pool, app_id, alice, "pc")
            .await
            .unwrap()
    );

    // Erasure removes the cursors with the user.
    advance(alice, "pc", d3.id).await.unwrap();
    advance(alice, "tablet", d0.id).await.unwrap();
    let counts = queries::erase_user(&pool, app_id, alice, false)
        .await
        .unwrap()
        .expect("alice exists");
    assert_eq!(counts.chat_device_cursors, 2);
    assert!(queries::list_chat_device_cursors(&pool, app_id, alice)
        .await
        .unwrap()
        .is_empty());

    cleanup(&pool, &[app_id, other_app]).await;
    assert_eq!(count(&pool, "chat_device_cursors", app_id).await, 0);
}
