//! Redis fencing primitives behind cross-node failover, against a real Redis
//! (`AURIX_E2E_REDIS_URL`, e.g. `redis://127.0.0.1:6379/15`). Three "nodes" share one server;
//! every key carries a fresh session id so runs do not interfere.

use aurix_common::config::RedisConfig;
use aurix_common::redis_pool::RedisSource;
use aurix_common::types::{AppId, MediaNodeId, SessionId, UserId};
use aurix_control::{MirroredPrefs, RedisStore, SessionMirror, TakeoverRefused};

async fn store(node: MediaNodeId) -> Option<RedisStore> {
    let url = std::env::var("AURIX_E2E_REDIS_URL").ok()?;
    let cfg = RedisConfig {
        url,
        pool_size: 2,
        sentinels: Vec::new(),
        sentinel_master: None,
    };
    let source = RedisSource::open(&cfg).await.expect("redis");
    Some(
        RedisStore::connect(source, node)
            .await
            .expect("redis store"),
    )
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
