//! Live end-to-end test of the native client against a running Aurix node.
//! Skipped unless `AURIX_E2E_API_KEY` is set (same environment as `aurix-server`'s `e2e_live`):
//!
//! ```text
//! AURIX_E2E_API=http://127.0.0.1:8080 AURIX_E2E_WS=ws://127.0.0.1:8081 \
//! AURIX_E2E_API_KEY=aurx_... cargo test -p aurix-client --test e2e_live
//! ```
//!
//! Two native clients talk through the real WS + UDP stack: roster events, encrypted audio
//! (tone → Opus → server → Opus → mixer), speaking state, chat, a forced control-plane drop
//! with session resume, and clean leave/disconnect.

use aurix_client::audio::{FRAME_SAMPLES, SAMPLE_RATE};
use aurix_client::events::{ConnectionState, Event};
use aurix_client::{Client, ClientConfig};
use aurix_common::types::{ChannelId, UserId};
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

struct Env {
    api: String,
    ws: String,
    api_key: String,
}

fn env() -> Option<Env> {
    let api_key = std::env::var("AURIX_E2E_API_KEY").ok()?;
    Some(Env {
        api: std::env::var("AURIX_E2E_API").unwrap_or_else(|_| "http://127.0.0.1:8080".into()),
        ws: std::env::var("AURIX_E2E_WS").unwrap_or_else(|_| "ws://127.0.0.1:8081".into()),
        api_key,
    })
}

async fn create_channel(env: &Env, http: &reqwest::Client) -> ChannelId {
    let ch: serde_json::Value = http
        .post(format!("{}/v1/channels", env.api))
        .header("x-api-key", &env.api_key)
        .json(&serde_json::json!({"name": format!("native-e2e-{}", uuid::Uuid::now_v7())}))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    ChannelId::from_uuid(ch["id"].as_str().unwrap().parse().unwrap())
}

async fn issue_token(
    env: &Env,
    http: &reqwest::Client,
    external_id: &str,
    name: &str,
    ch: ChannelId,
) -> (String, UserId) {
    let r: serde_json::Value = http
        .post(format!("{}/v1/tokens", env.api))
        .header("x-api-key", &env.api_key)
        .json(&serde_json::json!({
            "external_id": external_id,
            "display_name": name,
            "channels": [{"channel_id": ch, "join": true, "speak": true, "receive": true, "moderate": false}],
        }))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    (
        r["token"].as_str().unwrap().to_string(),
        UserId::from_uuid(r["user_id"].as_str().unwrap().parse().unwrap()),
    )
}

/// TCP proxy in front of the WS port so a test can sever the control plane without touching
/// the server. `kill()` drops every live connection; new connections keep working.
struct Proxy {
    addr: SocketAddr,
    generation: Arc<tokio::sync::Notify>,
    _accept: tokio::task::JoinHandle<()>,
}

impl Proxy {
    async fn start(upstream: SocketAddr) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let generation = Arc::new(tokio::sync::Notify::new());
        let notify = Arc::clone(&generation);
        let accept = tokio::spawn(async move {
            loop {
                let Ok((client, _)) = listener.accept().await else {
                    return;
                };
                let notify = Arc::clone(&notify);
                tokio::spawn(async move {
                    let Ok(server) = TcpStream::connect(upstream).await else {
                        return;
                    };
                    let (mut cr, mut cw) = client.into_split();
                    let (mut sr, mut sw) = server.into_split();
                    let up = async {
                        let mut buf = [0u8; 16 * 1024];
                        loop {
                            let n = cr.read(&mut buf).await.unwrap_or(0);
                            if n == 0 || sw.write_all(&buf[..n]).await.is_err() {
                                break;
                            }
                        }
                    };
                    let down = async {
                        let mut buf = [0u8; 16 * 1024];
                        loop {
                            let n = sr.read(&mut buf).await.unwrap_or(0);
                            if n == 0 || cw.write_all(&buf[..n]).await.is_err() {
                                break;
                            }
                        }
                    };
                    tokio::select! {
                        _ = up => {}
                        _ = down => {}
                        _ = notify.notified() => {}
                    }
                });
            }
        });
        Self {
            addr,
            generation,
            _accept: accept,
        }
    }

    fn kill(&self) {
        self.generation.notify_waiters();
    }
}

fn ws_url_via(proxy: &Proxy, ws: &str) -> String {
    let path = ws
        .split("://")
        .nth(1)
        .and_then(|rest| rest.find('/').map(|i| &rest[i..]))
        .unwrap_or("/ws");
    let path = if path.is_empty() { "/ws" } else { path };
    format!("ws://{}{}", proxy.addr, path)
}

fn ws_upstream(ws: &str) -> SocketAddr {
    let authority = ws.split("://").nth(1).unwrap().split('/').next().unwrap();
    let (host, port) = authority.rsplit_once(':').unwrap_or((authority, "8081"));
    format!("{host}:{port}").parse().unwrap()
}

fn ws_with_path(ws: &str) -> String {
    if ws.split("://").nth(1).is_some_and(|r| r.contains('/')) {
        ws.to_string()
    } else {
        format!("{ws}/ws")
    }
}

async fn wait_for<F: FnMut(&Event) -> bool>(
    client: &Client,
    what: &str,
    timeout: Duration,
    mut pred: F,
) -> Event {
    let deadline = Instant::now() + timeout;
    loop {
        while let Some(ev) = client.poll_event() {
            eprintln!("  [{what}] {ev:?}");
            if pred(&ev) {
                return ev;
            }
            if let Event::Disconnected { reason } | Event::FailedToRecover { reason } = &ev {
                panic!("connection ended while waiting for {what}: {reason}");
            }
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for {what} (state {:?})",
            client.state()
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// Alice pushes `secs` of a 440 Hz tone in 20 ms frames while Bob mixes his output; returns
/// Bob's RMS over the active frames and how many frames carried audio.
async fn stream_tone(alice: &Client, bob: &Client, secs: f32) -> (f32, usize) {
    let frames = (secs * 50.0) as usize;
    let mut phase = 0.0f32;
    let mut pcm = vec![0f32; FRAME_SAMPLES];
    let mut out = vec![0i16; FRAME_SAMPLES * 2];
    let mut active = 0usize;
    let mut sum_sq = 0f64;
    let mut samples = 0usize;
    for _ in 0..frames {
        for s in pcm.iter_mut() {
            *s = 0.3 * phase.sin();
            phase += 2.0 * std::f32::consts::PI * 440.0 / SAMPLE_RATE as f32;
        }
        alice.push_capture_f32(&pcm, SAMPLE_RATE, 1);
        if bob.mix_output_i16(&mut out, 2) > 0 {
            active += 1;
            for s in &out {
                let v = *s as f64 / 32768.0;
                sum_sq += v * v;
            }
            samples += out.len();
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let rms = if samples > 0 {
        (sum_sq / samples as f64).sqrt() as f32
    } else {
        0.0
    };
    (rms, active)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn native_clients_talk_chat_resume_and_leave() {
    let Some(env) = env() else {
        eprintln!("AURIX_E2E_API_KEY not set; skipping");
        return;
    };
    let http = reqwest::Client::new();
    let channel = create_channel(&env, &http).await;
    let (alice_token, alice_id) = issue_token(&env, &http, "native-alice", "Alice", channel).await;
    let (bob_token, bob_id) = issue_token(&env, &http, "native-bob", "Bob", channel).await;

    let proxy = Proxy::start(ws_upstream(&env.ws)).await;
    let mut alice_cfg = ClientConfig::new(ws_url_via(&proxy, &ws_with_path(&env.ws)), alice_token);
    alice_cfg.heartbeat_interval = Duration::from_millis(500);
    alice_cfg.reconnect.initial_delay = Duration::from_millis(200);
    let mut bob_cfg = ClientConfig::new(ws_with_path(&env.ws), bob_token);
    bob_cfg.heartbeat_interval = Duration::from_millis(500);

    let alice = Client::new(alice_cfg).unwrap();
    let bob = Client::new(bob_cfg).unwrap();
    let woke = Arc::new(AtomicBool::new(false));
    let hook_flag = Arc::clone(&woke);
    alice.set_wake_hook(Some(Arc::new(move || {
        hook_flag.store(true, Ordering::Release)
    })));

    alice.connect().unwrap();
    bob.connect().unwrap();
    let ready = wait_for(&alice, "alice session", Duration::from_secs(10), |e| {
        matches!(e, Event::SessionReady(_))
    })
    .await;
    let Event::SessionReady(alice_session) = ready else {
        unreachable!()
    };
    assert!(!alice_session.resumed);
    assert!(woke.load(Ordering::Acquire), "wake hook did not fire");
    wait_for(&alice, "alice media", Duration::from_secs(10), |e| {
        matches!(e, Event::MediaBound)
    })
    .await;
    wait_for(&bob, "bob media", Duration::from_secs(10), |e| {
        matches!(e, Event::MediaBound)
    })
    .await;
    assert_eq!(alice.state(), ConnectionState::MediaBound);
    assert_eq!(alice.user_id(), Some(alice_id));

    // --- join: Alice first, then Bob; Alice sees Bob arrive.
    alice.join_channel(channel, None).unwrap();
    let joined = wait_for(
        &alice,
        "alice join",
        Duration::from_secs(10),
        |e| matches!(e, Event::ChannelJoined { channel_id, .. } if *channel_id == channel),
    )
    .await;
    if let Event::ChannelJoined { participants, .. } = joined {
        assert!(
            participants.is_empty(),
            "roster excludes the joiner: {participants:?}"
        );
    }
    bob.join_channel(channel, None).unwrap();
    wait_for(&bob, "bob join", Duration::from_secs(10), |e| {
        matches!(e, Event::ChannelJoined { participants, .. }
            if participants.len() == 1 && participants[0].user_id == alice_id)
    })
    .await;
    wait_for(&alice, "bob joined", Duration::from_secs(10), |e| {
        matches!(e, Event::ParticipantJoined { participant, .. } if participant.user_id == bob_id)
    })
    .await;
    assert_eq!(alice.joined_channels(), vec![channel]);
    assert_eq!(bob.participants(channel).len(), 1);

    // --- audio: Alice speaks, Bob hears a tone of the expected loudness and sees speaking.
    let (rms, active) = stream_tone(&alice, &bob, 1.6).await;
    eprintln!("bob heard rms={rms:.3} over {active} frames");
    assert!(active >= 50, "bob mixed only {active} active frames");
    assert!((0.15..0.35).contains(&rms), "unexpected rms {rms}");
    wait_for(&bob, "alice speaking", Duration::from_secs(5), |e| {
        matches!(e, Event::ParticipantSpeaking { user_id, speaking: true, .. } if *user_id == alice_id)
    })
    .await;
    assert!(alice.is_speaking());
    let alice_ssrc = alice.session().unwrap().ssrc;
    assert_eq!(bob.user_for_ssrc(alice_ssrc), Some(alice_id));
    let bob_stats = bob.stats();
    assert!(bob_stats.media.audio_frames_received >= 50, "{bob_stats:?}");
    assert_eq!(bob_stats.media.bad_auth, 0);
    let stream = bob_stats
        .streams
        .iter()
        .find(|s| s.ssrc == alice_ssrc)
        .expect("alice stream stats");
    assert_eq!(
        stream.lost, 0,
        "gapless audio sequence expected: {stream:?}"
    );
    let deadline = Instant::now() + Duration::from_secs(3);
    while bob.stats().media.rtt_ms == 0.0 {
        assert!(
            Instant::now() < deadline,
            "no heartbeat RTT: {:?}",
            bob.stats()
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let alice_stats = alice.stats();
    assert!(alice_stats.transmit.frames_sent >= 50, "{alice_stats:?}");

    // --- chat: channel message with a client reference, echoed to Alice with her request id.
    let req = alice
        .send_chat(channel, "gg wp", Some(serde_json::json!({"kind": "ping"})))
        .unwrap();
    wait_for(&bob, "chat", Duration::from_secs(5), |e| {
        matches!(e, Event::ChatMessage { message, .. }
            if message.text == "gg wp" && message.from_user_id == alice_id
                && message.metadata.as_ref().and_then(|m| m["kind"].as_str()) == Some("ping"))
    })
    .await;
    wait_for(&alice, "chat echo", Duration::from_secs(5), |e| {
        matches!(e, Event::ChatMessage { request_id: Some(r), message } if *r == req && message.text == "gg wp")
    })
    .await;

    // --- resume: sever Alice's control plane; the session, SSRC and membership survive.
    proxy.kill();
    wait_for(&alice, "recovering", Duration::from_secs(15), |e| {
        matches!(e, Event::Recovering { .. })
    })
    .await;
    let recovered = wait_for(&alice, "recovered", Duration::from_secs(15), |e| {
        matches!(e, Event::Recovered { .. })
    })
    .await;
    assert!(matches!(recovered, Event::Recovered { resumed: true }));
    let after = alice.session().unwrap();
    assert_eq!(after.session_id, alice_session.session_id);
    assert_eq!(after.ssrc, alice_session.ssrc);
    assert!(after.resumed);
    assert_eq!(alice.joined_channels(), vec![channel]);
    assert_eq!(alice.state(), ConnectionState::MediaBound);
    // Bob never saw Alice leave.
    while let Some(ev) = bob.poll_event() {
        assert!(
            !matches!(ev, Event::ParticipantLeft { user_id, .. } if user_id == alice_id),
            "bob saw alice leave during resume"
        );
    }
    let (rms, active) = stream_tone(&alice, &bob, 1.2).await;
    eprintln!("after resume bob heard rms={rms:.3} over {active} frames");
    assert!(
        active >= 30 && rms > 0.15,
        "audio did not resume: rms={rms} active={active}"
    );

    // --- leave & disconnect.
    alice.leave_channel(channel).unwrap();
    wait_for(
        &alice,
        "alice left",
        Duration::from_secs(5),
        |e| matches!(e, Event::ChannelLeft { channel_id } if *channel_id == channel),
    )
    .await;
    wait_for(
        &bob,
        "alice gone",
        Duration::from_secs(5),
        |e| matches!(e, Event::ParticipantLeft { user_id, .. } if *user_id == alice_id),
    )
    .await;
    assert!(alice.joined_channels().is_empty());
    assert!(bob.participants(channel).is_empty());

    alice.disconnect();
    assert_eq!(alice.state(), ConnectionState::Disconnected);
    let mut saw_disconnect = false;
    while let Some(ev) = alice.poll_event() {
        if matches!(ev, Event::Disconnected { .. }) {
            saw_disconnect = true;
        }
    }
    assert!(saw_disconnect, "no Disconnected event after disconnect()");
    bob.disconnect();
    assert_eq!(bob.state(), ConnectionState::Disconnected);
}
