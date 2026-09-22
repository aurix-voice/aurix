//! Redis fencing primitives behind cross-node failover, against a real Redis
//! (`AURIX_E2E_REDIS_URL`, e.g. `redis://127.0.0.1:6379/15`) or a real Redis Cluster
//! (`AURIX_E2E_REDIS_CLUSTER`, comma-separated seed URLs; takes precedence). Three "nodes"
//! share one server; every key carries a fresh session id so runs do not interfere. Against a
//! cluster the same tests prove that no multi-key script or command crosses a slot.

use aurix_common::config::{RateLimitConfig, RedisConfig};
use aurix_common::redis_pool::RedisSource;
use aurix_common::types::{AppId, ChannelId, MediaNodeId, SessionId, UserId};
use aurix_control::{
    EventBus, FleetLimiter, Limit, LimitBackend, LimitScope, MirroredPrefs, RedisStore,
    ServerEvent, SessionMirror, TakeoverRefused,
};
use std::sync::Arc;
use std::time::Duration;

fn redis_config() -> Option<RedisConfig> {
    let base = RedisConfig {
        pool_size: 2,
        ..RedisConfig::default()
    };
    if let Ok(seeds) = std::env::var("AURIX_E2E_REDIS_CLUSTER") {
        return Some(RedisConfig {
            cluster: seeds
                .split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_owned)
                .collect(),
            sharded_pubsub: std::env::var("AURIX_E2E_REDIS_SHARDED_PUBSUB")
                .map(|v| v != "0" && v != "false")
                .unwrap_or(true),
            ..base
        });
    }
    Some(RedisConfig {
        url: std::env::var("AURIX_E2E_REDIS_URL").ok()?,
        ..base
    })
}

async fn store(node: MediaNodeId) -> Option<RedisStore> {
    let cfg = redis_config()?;
    let source = RedisSource::open(&cfg).await.expect("redis");
    Some(
        RedisStore::connect(source, node)
            .await
            .expect("redis store"),
    )
}

/// Events published on one node's bus arrive on the other node's bus and never on the
/// publisher's own bus a second time; the subscriber's readiness flag follows the attach.
#[tokio::test]
#[ignore = "requires Redis (AURIX_E2E_REDIS_URL)"]
async fn events_replicate_between_nodes() {
    let (n1, n2) = (MediaNodeId::new(), MediaNodeId::new());
    let Some(node1) = store(n1).await else {
        eprintln!("AURIX_E2E_REDIS_URL not set; skipping");
        return;
    };
    let node1 = Arc::new(node1);
    let node2 = Arc::new(store(n2).await.unwrap());
    let bus1 = Arc::new(EventBus::new(64));
    let bus2 = Arc::new(EventBus::new(64));
    let mut local1 = bus1.subscribe();
    let mut local2 = bus2.subscribe();
    assert!(
        node1.event_subscriber_connected(),
        "never started: nothing to miss"
    );
    node1.start_event_replication(bus1.clone());
    node2.start_event_replication(bus2.clone());
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while !(node1.event_subscriber_connected() && node2.event_subscriber_connected()) {
        assert!(
            tokio::time::Instant::now() < deadline,
            "subscribers never attached"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    // A subscription confirmation can still be in flight on the server side; publish until the
    // peer hears us.
    let probe = AppId::new();
    let heard = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            bus1.publish(ServerEvent::WebhooksChanged { app_id: probe });
            match tokio::time::timeout(Duration::from_millis(300), local2.recv()).await {
                Ok(Ok(ServerEvent::WebhooksChanged { app_id })) if app_id == probe => break,
                Ok(Ok(_)) => continue,
                Ok(Err(e)) => panic!("bus2 closed: {e}"),
                Err(_) => continue,
            }
        }
    })
    .await;
    assert!(heard.is_ok(), "node 2 never received node 1's event");
    // A remote-only event shows up on node 1 exactly once, and node 2 sees its own publish once
    // (local delivery) with no echo through Redis. Only the probe is counted: other tests share
    // the event channel.
    let probe = AppId::new();
    bus2.publish(ServerEvent::WebhooksChanged { app_id: probe });
    let first = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match local1.recv().await.unwrap() {
                ServerEvent::WebhooksChanged { app_id } if app_id == probe => break,
                _ => continue,
            }
        }
    })
    .await;
    assert!(first.is_ok(), "node 1 receives node 2's event");
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(count_probe(&mut local1, probe), 0, "event delivered twice");
    assert_eq!(
        count_probe(&mut local2, probe),
        1,
        "node 2 gets its own event locally once, never echoed back through Redis"
    );
}

fn count_probe(rx: &mut tokio::sync::broadcast::Receiver<ServerEvent>, probe: AppId) -> usize {
    let mut n = 0;
    while let Ok(event) = rx.try_recv() {
        if matches!(event, ServerEvent::WebhooksChanged { app_id } if app_id == probe) {
            n += 1;
        }
    }
    n
}

/// Per-user keys are dropped in one multi-key `DEL`; on a cluster this only works because they
/// share the user's hash tag.
#[tokio::test]
#[ignore = "requires Redis (AURIX_E2E_REDIS_URL)"]
async fn user_keys_share_a_slot() {
    let Some(node1) = store(MediaNodeId::new()).await else {
        eprintln!("AURIX_E2E_REDIS_URL not set; skipping");
        return;
    };
    let user = UserId::new();
    let channel = ChannelId::new();
    node1.set_global_mute(user, true).await.unwrap();
    node1.add_user_channel(user, channel).await.unwrap();
    assert!(node1.is_globally_muted(user).await.unwrap());
    assert_eq!(node1.get_user_channels(user).await.unwrap(), vec![channel]);
    node1.forget_user(user).await.unwrap();
    assert!(!node1.is_globally_muted(user).await.unwrap());
    assert!(node1.get_user_channels(user).await.unwrap().is_empty());
}

/// Publishes `WebhooksChanged` events on `from` until one of them reaches `to`, or `budget`
/// runs out. Returns whether the peer heard us.
async fn events_flow(
    from: &EventBus,
    to: &mut tokio::sync::broadcast::Receiver<ServerEvent>,
    budget: Duration,
) -> bool {
    let probe = AppId::new();
    tokio::time::timeout(budget, async {
        loop {
            from.publish(ServerEvent::WebhooksChanged { app_id: probe });
            let wait = tokio::time::sleep(Duration::from_millis(300));
            tokio::pin!(wait);
            loop {
                tokio::select! {
                    _ = &mut wait => break,
                    r = to.recv() => match r {
                        Ok(ServerEvent::WebhooksChanged { app_id }) if app_id == probe => return,
                        Ok(_) | Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                        Err(e) => panic!("bus closed: {e}"),
                    },
                }
            }
        }
    })
    .await
    .is_ok()
}

/// Redis Cluster shard failover under the event bus. Needs the cluster and two hooks that
/// take the shard's `host:port`: `AURIX_E2E_REDIS_CLUSTER_KILL` (SIGKILL that node) and
/// `AURIX_E2E_REDIS_CLUSTER_START` (bring it back). After the master holding the event
/// channel's slot dies, both nodes' subscribers re-attach to the promoted replica, events flow
/// again, and the fencing scripts still work on the moved slot.
#[tokio::test]
#[ignore = "requires a Redis Cluster and AURIX_E2E_REDIS_CLUSTER_KILL/_START hooks"]
async fn cluster_shard_failover_reattaches_the_event_bus() {
    let (Ok(kill), Ok(start)) = (
        std::env::var("AURIX_E2E_REDIS_CLUSTER_KILL"),
        std::env::var("AURIX_E2E_REDIS_CLUSTER_START"),
    ) else {
        eprintln!("AURIX_E2E_REDIS_CLUSTER_KILL/_START not set; skipping");
        return;
    };
    let (n1, n2) = (MediaNodeId::new(), MediaNodeId::new());
    let node1 = Arc::new(store(n1).await.expect("AURIX_E2E_REDIS_CLUSTER"));
    let node2 = Arc::new(store(n2).await.unwrap());
    assert!(
        node1.is_cluster(),
        "this scenario needs AURIX_E2E_REDIS_CLUSTER"
    );
    let bus1 = Arc::new(EventBus::new(256));
    let bus2 = Arc::new(EventBus::new(256));
    let mut local2 = bus2.subscribe();
    node1.start_event_replication(bus1.clone());
    node2.start_event_replication(bus2.clone());
    assert!(
        events_flow(&bus1, &mut local2, Duration::from_secs(10)).await,
        "baseline: events never flowed"
    );
    let shard = node1
        .event_shard_master()
        .await
        .unwrap()
        .expect("event channel has a master");
    eprintln!("event channel lives on {shard}; killing it");
    let run = |hook: &str| {
        let status = std::process::Command::new("sh")
            .arg("-c")
            .arg(format!("{hook} {shard}"))
            .status()
            .expect("hook runs");
        assert!(status.success(), "hook {hook:?} failed: {status}");
    };
    run(&kill);

    // The cluster needs cluster-node-timeout (+ election) to promote the replica; the moved
    // slot must then serve the fencing scripts and both subscribers must have re-attached.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
    let promoted = loop {
        if let Ok(Some(owner)) = node1.event_shard_master().await {
            if owner != shard {
                break owner;
            }
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "cluster never promoted a replica for the event slot"
        );
        tokio::time::sleep(Duration::from_millis(250)).await;
    };
    eprintln!("slot promoted to {promoted}");
    assert!(
        events_flow(&bus1, &mut local2, Duration::from_secs(30)).await,
        "events never resumed after the shard failover"
    );
    // node1 → node2 only proves node2's subscriber; node1's re-attaches independently.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    while !(node1.event_subscriber_connected() && node2.event_subscriber_connected()) {
        assert!(
            tokio::time::Instant::now() < deadline,
            "a subscriber never re-attached after the shard failover"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let sid = SessionId::new();
    assert!(node1.write_session_mirror(&mirror(sid), 60).await.unwrap());
    assert_eq!(node2.claim_session(sid, n1, 60).await.unwrap(), Ok(()));
    assert_eq!(node1.get_session_node(sid).await.unwrap(), Some(n2));
    assert!(node2.delete_session_mirror(sid).await.unwrap());

    eprintln!("bringing {shard} back; it must rejoin as a replica");
    run(&start);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
    loop {
        let owner = node1.event_shard_master().await;
        if let Ok(Some(owner)) = owner {
            assert_eq!(owner, promoted, "old master took the slot back");
        }
        if events_flow(&bus2, &mut bus1.subscribe(), Duration::from_secs(3)).await {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "events do not flow after the old master rejoined"
        );
    }
}

fn mirror(session_id: SessionId) -> SessionMirror {
    SessionMirror {
        session_id,
        user_id: UserId::new(),
        app_id: AppId::new(),
        display_name: "Alice".into(),
        ssrc: 42,
        audio_seq: 0,
        resume_hash: SessionMirror::resume_hash_hex(&[7u8; 32]),
        ip: "127.0.0.1".into(),
        user_agent: None,
        channels: Vec::new(),
        prefs: MirroredPrefs::default(),
        updated_at: 0,
    }
}

/// Ownership is a compare-and-set: the previous owner's writes fail once another node has
/// claimed the session, a third node cannot claim with a stale expectation, and a released
/// claim hands the session back without losing the mirror.
#[tokio::test]
#[ignore = "requires Redis (AURIX_E2E_REDIS_URL)"]
async fn takeover_claims_are_fenced() {
    let (n1, n2, n3) = (MediaNodeId::new(), MediaNodeId::new(), MediaNodeId::new());
    let Some(node1) = store(n1).await else {
        eprintln!("AURIX_E2E_REDIS_URL not set; skipping");
        return;
    };
    let node2 = store(n2).await.unwrap();
    let node3 = store(n3).await.unwrap();
    let sid = SessionId::new();
    let m = mirror(sid);

    // Nothing to take over before the first write.
    assert_eq!(
        node2.claim_session(sid, n1, 60).await.unwrap(),
        Err(TakeoverRefused::NotMirrored)
    );
    assert!(node1.write_session_mirror(&m, 60).await.unwrap());
    assert_eq!(node1.get_session_node(sid).await.unwrap(), Some(n1));
    assert_eq!(
        node2.get_session_mirror(sid).await.unwrap().unwrap().ssrc,
        42
    );

    // Node 2 adopts; node 1's refresh is refused and its deletes are no-ops.
    assert_eq!(node2.claim_session(sid, n1, 60).await.unwrap(), Ok(()));
    assert_eq!(node1.get_session_node(sid).await.unwrap(), Some(n2));
    assert!(!node1.write_session_mirror(&m, 60).await.unwrap());
    assert!(!node1.delete_session_mirror(sid).await.unwrap());
    assert!(node2
        .get_session_mirror(sid)
        .await
        .is_ok_and(|m| m.is_some()));

    // A third node that still believes node 1 owns the session loses the race.
    assert_eq!(
        node3.claim_session(sid, n1, 60).await.unwrap(),
        Err(TakeoverRefused::Raced)
    );
    // Re-claiming what you own is idempotent.
    assert_eq!(node2.claim_session(sid, n2, 60).await.unwrap(), Ok(()));

    // Node 2 cannot honour the takeover after all and hands the session back to node 1.
    assert!(node2.release_session_claim(sid, Some(n1)).await.unwrap());
    assert_eq!(node1.get_session_node(sid).await.unwrap(), Some(n1));
    assert!(node1.write_session_mirror(&m, 60).await.unwrap());
    // Releasing a session you no longer own changes nothing.
    assert!(!node2.release_session_claim(sid, Some(n2)).await.unwrap());
    assert_eq!(node1.get_session_node(sid).await.unwrap(), Some(n1));

    // Node 3 takes over with the right expectation; then closes the session for good.
    assert_eq!(node3.claim_session(sid, n1, 60).await.unwrap(), Ok(()));
    assert!(node3.delete_session_mirror(sid).await.unwrap());
    assert!(node1.get_session_mirror(sid).await.unwrap().is_none());
    assert_eq!(node1.get_session_node(sid).await.unwrap(), None);
    assert_eq!(
        node1.claim_session(sid, n3, 60).await.unwrap(),
        Err(TakeoverRefused::NotMirrored)
    );
}

/// The liveness beacon and the single-reaper claim.
#[tokio::test]
#[ignore = "requires Redis (AURIX_E2E_REDIS_URL)"]
async fn reaper_runs_once_per_lost_node() {
    let (n1, n2, lost) = (MediaNodeId::new(), MediaNodeId::new(), MediaNodeId::new());
    let Some(node1) = store(n1).await else {
        eprintln!("AURIX_E2E_REDIS_URL not set; skipping");
        return;
    };
    let node2 = store(n2).await.unwrap();
    assert!(!node1.is_node_alive(lost).await.unwrap());
    node1.beacon_alive(60).await.unwrap();
    assert!(node2.is_node_alive(n1).await.unwrap());
    assert!(node1.claim_reaper(lost, 60).await.unwrap());
    assert!(
        !node2.claim_reaper(lost, 60).await.unwrap(),
        "only one node reaps a lost node"
    );
}

/// One token bucket per subject for the whole fleet: two nodes drain the same budget, the
/// refusal names the wait until the next token, and the bucket refills with the Redis clock.
#[tokio::test]
#[ignore = "requires Redis (AURIX_E2E_REDIS_URL)"]
async fn token_bucket_is_shared_by_every_node() {
    let Some(node1) = store(MediaNodeId::new()).await else {
        eprintln!("AURIX_E2E_REDIS_URL not set; skipping");
        return;
    };
    let node2 = store(MediaNodeId::new()).await.unwrap();
    let key = format!("test:{}", SessionId::new());

    // burst 3 at 10/s: three tokens across both nodes, then a wait of ~100 ms.
    assert_eq!(node1.take_tokens(&key, 10.0, 3.0, 1.0).await.unwrap(), None);
    assert_eq!(node2.take_tokens(&key, 10.0, 3.0, 1.0).await.unwrap(), None);
    assert_eq!(node1.take_tokens(&key, 10.0, 3.0, 1.0).await.unwrap(), None);
    let wait = node2
        .take_tokens(&key, 10.0, 3.0, 1.0)
        .await
        .unwrap()
        .expect("fourth token refused");
    assert!(
        wait > Duration::ZERO && wait <= Duration::from_millis(100),
        "{wait:?}"
    );
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert_eq!(
        node1.take_tokens(&key, 10.0, 3.0, 1.0).await.unwrap(),
        None,
        "refilled after the wait"
    );

    // A zero rate never refills.
    let frozen = format!("test:{}", SessionId::new());
    assert_eq!(
        node1.take_tokens(&frozen, 0.0, 1.0, 1.0).await.unwrap(),
        None
    );
    assert_eq!(
        node2.take_tokens(&frozen, 0.0, 1.0, 1.0).await.unwrap(),
        Some(Duration::MAX)
    );

    // A cost above the burst is refused with the time to fill the missing part.
    let big = format!("test:{}", SessionId::new());
    let wait = node1
        .take_tokens(&big, 1.0, 2.0, 4.0)
        .await
        .unwrap()
        .expect("cost above burst");
    assert_eq!(wait, Duration::from_secs(2));
}

/// The limiter on two nodes with the same config takes fleet decisions for one subject.
#[tokio::test]
#[ignore = "requires Redis (AURIX_E2E_REDIS_URL)"]
async fn fleet_limiter_throttles_across_nodes() {
    let Some(node1) = store(MediaNodeId::new()).await else {
        eprintln!("AURIX_E2E_REDIS_URL not set; skipping");
        return;
    };
    let node2 = store(MediaNodeId::new()).await.unwrap();
    let cfg = RateLimitConfig {
        enabled: true,
        reports_per_minute: 2,
        ..RateLimitConfig::default()
    };
    let l1 = FleetLimiter::new(cfg.clone(), Some(Arc::new(node1)));
    let l2 = FleetLimiter::new(cfg.clone(), Some(Arc::new(node2)));
    let user = UserId::new().to_string();

    assert!(l1.check(LimitScope::Report, &user).await.is_ok());
    assert!(l2.check(LimitScope::Report, &user).await.is_ok());
    let t = l1.check(LimitScope::Report, &user).await.unwrap_err();
    assert_eq!(t.backend, LimitBackend::Fleet);
    assert_eq!(t.scope, LimitScope::Report);
    assert_eq!(t.retry_after_secs(), 30);
    assert!(
        l2.check(LimitScope::Report, &user).await.is_err(),
        "the other node sees the same empty bucket"
    );
    // Other subjects and scopes are untouched.
    let someone_else = UserId::new().to_string();
    assert!(l2.check(LimitScope::Report, &someone_else).await.is_ok());
    assert!(l2.check(LimitScope::Block, &user).await.is_ok());

    // Per-key budgets: the caller passes the key's own limit; 0 tokens left refuses on any node.
    let api_key = SessionId::new().to_string();
    let per_key = Limit::per_minute(3);
    for _ in 0..3 {
        assert!(l2
            .check_with(LimitScope::ApiKey, &api_key, per_key, 1.0)
            .await
            .is_ok());
    }
    let t = l1
        .check_with(LimitScope::ApiKey, &api_key, per_key, 1.0)
        .await
        .unwrap_err();
    assert_eq!(t.backend, LimitBackend::Fleet);
    assert_eq!(t.retry_after_secs(), 20);

    // `fleet = false` keeps the node on its own buckets even with Redis at hand.
    let local_only = FleetLimiter::new(
        RateLimitConfig {
            fleet: false,
            ..cfg
        },
        None,
    );
    assert!(local_only.check(LimitScope::Report, &user).await.is_ok());
    assert!(local_only.check(LimitScope::Report, &user).await.is_ok());
    assert_eq!(
        local_only
            .check(LimitScope::Report, &user)
            .await
            .unwrap_err()
            .backend,
        LimitBackend::Local
    );
}
