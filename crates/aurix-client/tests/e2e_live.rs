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
use aurix_client::{Client, ClientConfig, DspConfig, EncoderSettings, MediaPath, MediaPathPolicy};
use aurix_common::types::{
    AudioPolicy, ChannelId, ChannelRole, DownlinkMode, OpusBandwidth, OpusSignal, RecordingConsent,
    UserId,
};
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

/// Silently drops every UDP datagram one server media port sends back to this host
/// (`sudo iptables`), so a client of that node sees a UDP black hole — its packets leave, the
/// server even processes them, nothing returns — while other nodes stay reachable. Removes
/// the rule again on drop (also when the test panics).
struct UdpBlock {
    port: u16,
    active: bool,
}

impl UdpBlock {
    fn rule(port: u16) -> [String; 7] {
        [
            "INPUT".into(),
            "-p".into(),
            "udp".into(),
            "--sport".into(),
            port.to_string(),
            "-j".into(),
            "DROP".into(),
        ]
    }

    fn iptables(action: &str, port: u16) -> bool {
        std::process::Command::new("sudo")
            .args(["-n", "iptables", action])
            .args(Self::rule(port))
            .status()
            .is_ok_and(|s| s.success())
    }

    fn new(port: u16) -> Self {
        Self {
            port,
            active: false,
        }
    }

    fn block(&mut self) {
        if !self.active {
            assert!(Self::iptables("-A", self.port), "iptables -A failed");
            self.active = true;
        }
    }

    fn unblock(&mut self) {
        if self.active {
            assert!(Self::iptables("-D", self.port), "iptables -D failed");
            self.active = false;
        }
    }
}

impl Drop for UdpBlock {
    fn drop(&mut self) {
        if self.active {
            Self::iptables("-D", self.port);
        }
    }
}

async fn session_media_path(env: &Env, http: &reqwest::Client, session: &str) -> String {
    let stats: serde_json::Value = http
        .get(format!("{}/v1/sessions/{}/stats", env.api, session))
        .header("x-api-key", &env.api_key)
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    stats["media_path"].as_str().unwrap_or("").to_string()
}

async fn create_channel(env: &Env, http: &reqwest::Client) -> ChannelId {
    create_channel_with(env, http, serde_json::json!({})).await
}

async fn create_channel_with(
    env: &Env,
    http: &reqwest::Client,
    config: serde_json::Value,
) -> ChannelId {
    let ch: serde_json::Value = http
        .post(format!("{}/v1/channels", env.api))
        .header("x-api-key", &env.api_key)
        .json(&serde_json::json!({
            "name": format!("native-e2e-{}", uuid::Uuid::now_v7()),
            "config": config,
        }))
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
    issue_token_with(env, http, external_id, name, ch, true).await
}

async fn issue_token_with(
    env: &Env,
    http: &reqwest::Client,
    external_id: &str,
    name: &str,
    ch: ChannelId,
    speak: bool,
) -> (String, UserId) {
    let r: serde_json::Value = http
        .post(format!("{}/v1/tokens", env.api))
        .header("x-api-key", &env.api_key)
        .json(&serde_json::json!({
            "external_id": external_id,
            "display_name": name,
            "channels": [{"channel_id": ch, "join": true, "speak": speak, "receive": true, "moderate": false}],
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
    alice_cfg.dsp = DspConfig::BYPASS;
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

    // --- audio: Alice speaks (DSP bypassed), Bob hears the tone at its transmitted loudness.
    let (rms, active) = stream_tone(&alice, &bob, 1.6).await;
    eprintln!("bob heard rms={rms:.3} over {active} frames");
    assert!(active >= 50, "bob mixed only {active} active frames");
    assert!((0.15..0.35).contains(&rms), "unexpected rms {rms}");
    // --- with the default capture DSP the AGC lands the same tone on its -18 dBFS target.
    alice.set_dsp(DspConfig::default());
    assert!(alice.dsp().agc);
    let (rms_dsp, active_dsp) = stream_tone(&alice, &bob, 1.6).await;
    let agc_db = alice.dsp_stats().agc_gain_db;
    eprintln!("bob heard rms={rms_dsp:.3} over {active_dsp} frames with DSP (agc {agc_db:.1} dB)");
    assert!(
        active_dsp >= 50,
        "bob mixed only {active_dsp} active frames with DSP"
    );
    assert!(
        (0.09..0.16).contains(&rms_dsp),
        "AGC did not settle on -18 dBFS: rms {rms_dsp}"
    );
    assert!(
        agc_db < 0.0,
        "AGC should attenuate a -10 dBFS tone, gain {agc_db} dB"
    );
    alice.set_dsp(DspConfig::BYPASS);
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

    // --- quality: a clean local link scores 5 bars locally and in the server's merged view.
    let bob_stats = bob.stats();
    assert_eq!(bob_stats.bars, 5, "{bob_stats:?}");
    assert!(
        bob_stats.r_factor >= 80.0 && bob_stats.mos >= 4.0,
        "{bob_stats:?}"
    );
    assert!(
        bob_stats.media.rtt_min_ms <= bob_stats.media.rtt_avg_ms,
        "{bob_stats:?}"
    );
    assert!(
        bob_stats.media.rtt_avg_ms <= bob_stats.media.rtt_max_ms,
        "{bob_stats:?}"
    );
    assert_eq!(bob_stats.loss_percent, 0.0, "{bob_stats:?}");
    let Event::NetworkQuality(server_view) = wait_for(
        &alice,
        "server quality",
        Duration::from_secs(12),
        |e| matches!(e, Event::NetworkQuality(q) if q.uplink_packets_received > 0),
    )
    .await
    else {
        unreachable!()
    };
    assert_eq!(server_view.bars, 5, "{server_view:?}");
    assert_eq!(server_view.uplink_packets_lost, 0, "{server_view:?}");
    assert_eq!(alice.network_quality().map(|q| q.bars), Some(5));

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
    assert!(matches!(recovered, Event::Recovered { resumed: true, .. }));
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

/// The channel's Opus policy drives the native encoder: the join ack configures libopus
/// (bitrate/FEC/DTX/bandwidth/signal/complexity), a local complexity pin survives policy
/// changes, a live `PUT /v1/channels/:id/config` re-applies the encoder without interrupting
/// audio, and `follow_channel_policy = false` keeps the app's own settings.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn native_client_follows_channel_audio_policy() {
    let Some(env) = env() else {
        eprintln!("AURIX_E2E_API_KEY not set; skipping");
        return;
    };
    let http = reqwest::Client::new();
    let channel = create_channel_with(
        &env,
        &http,
        serde_json::json!({
            "bitrate": 40000, "min_bitrate": 24000,
            "enable_fec": false, "enable_dtx": false,
            "max_bandwidth": "wideband", "complexity": 6,
            "audio_profile": "music"
        }),
    )
    .await;
    let initial = AudioPolicy {
        bitrate_bps: 40_000,
        min_bitrate_bps: 24_000,
        fec: false,
        dtx: false,
        max_bandwidth: OpusBandwidth::Wideband,
        complexity: Some(6),
        signal: OpusSignal::Music,
        stereo: false,
        e2ee: false,
    };
    let (alice_token, _) = issue_token(&env, &http, "native-opus-alice", "Alice", channel).await;
    let (bob_token, _) = issue_token(&env, &http, "native-opus-bob", "Bob", channel).await;

    let alice = Client::new(ClientConfig::new(ws_with_path(&env.ws), alice_token)).unwrap();
    let mut bob_cfg = ClientConfig::new(ws_with_path(&env.ws), bob_token);
    bob_cfg.follow_channel_policy = false;
    bob_cfg.encoder = EncoderSettings {
        bitrate_bps: 20_000,
        complexity: 4,
        ..EncoderSettings::default()
    };
    let bob = Client::new(bob_cfg).unwrap();
    let baseline = alice.encoder_settings();
    assert_eq!(baseline, EncoderSettings::default());
    assert_eq!(alice.audio_policy(), None);

    alice.connect().unwrap();
    bob.connect().unwrap();
    for (c, who) in [(&alice, "alice"), (&bob, "bob")] {
        wait_for(c, &format!("{who} media"), Duration::from_secs(10), |e| {
            matches!(e, Event::MediaBound)
        })
        .await;
    }

    // --- join applies the channel policy to Alice's encoder; Bob (opted out) keeps his own.
    alice.join_channel(channel, None).unwrap();
    let ev = wait_for(&alice, "alice policy", Duration::from_secs(10), |e| {
        matches!(e, Event::AudioPolicyChanged(_))
    })
    .await;
    let Event::AudioPolicyChanged(p) = ev else {
        unreachable!()
    };
    assert_eq!(p, initial);
    assert_eq!(alice.audio_policy(), Some(initial));
    wait_for(
        &alice,
        "alice join",
        Duration::from_secs(10),
        |e| matches!(e, Event::ChannelJoined { channel_id, .. } if *channel_id == channel),
    )
    .await;
    let applied = alice.encoder_settings();
    assert_eq!(
        (
            applied.bitrate_bps,
            applied.fec,
            applied.dtx,
            applied.max_bandwidth,
            applied.signal,
            applied.complexity,
        ),
        (
            40_000,
            false,
            false,
            OpusBandwidth::Wideband,
            OpusSignal::Music,
            6
        ),
        "{applied:?}"
    );
    assert_eq!(
        (applied.vbr, applied.constrained_vbr),
        (baseline.vbr, baseline.constrained_vbr)
    );
    bob.join_channel(channel, None).unwrap();
    wait_for(
        &bob,
        "bob join",
        Duration::from_secs(10),
        |e| matches!(e, Event::ChannelJoined { channel_id, .. } if *channel_id == channel),
    )
    .await;
    wait_for(&alice, "bob joined", Duration::from_secs(10), |e| {
        matches!(e, Event::ParticipantJoined { .. })
    })
    .await;
    assert_eq!(
        bob.audio_policy(),
        Some(initial),
        "policy is tracked even when not followed"
    );
    let bob_enc = bob.encoder_settings();
    assert_eq!(
        (bob_enc.bitrate_bps, bob_enc.complexity),
        (20_000, 4),
        "{bob_enc:?}"
    );
    assert_eq!(bob_enc.max_bandwidth, OpusBandwidth::Fullband);

    // --- a local complexity pin overrides the channel hint.
    alice.set_complexity(Some(3)).unwrap();
    assert_eq!(alice.encoder_settings().complexity, 3);

    // --- audio flows under the wideband/music policy.
    let (rms, active) = stream_tone(&alice, &bob, 1.0).await;
    assert!(active >= 30 && rms > 0.1, "rms {rms} over {active} frames");

    // --- live policy update: the pin stays, everything else follows the new channel config.
    http.put(format!("{}/v1/channels/{channel}/config", env.api))
        .header("x-api-key", &env.api_key)
        .json(&serde_json::json!({
            "bitrate": 64000, "min_bitrate": 16000,
            "enable_fec": true, "enable_dtx": true,
            "max_bandwidth": "fullband",
            "audio_profile": "voice"
        }))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    let updated = AudioPolicy {
        bitrate_bps: 64_000,
        min_bitrate_bps: 16_000,
        fec: true,
        dtx: true,
        max_bandwidth: OpusBandwidth::Fullband,
        complexity: None,
        signal: OpusSignal::Voice,
        stereo: false,
        e2ee: false,
    };
    let ev = wait_for(
        &alice,
        "alice policy update",
        Duration::from_secs(10),
        |e| matches!(e, Event::AudioPolicyChanged(_)),
    )
    .await;
    let Event::AudioPolicyChanged(p) = ev else {
        unreachable!()
    };
    assert_eq!(p, updated);
    let ev = wait_for(&bob, "bob policy update", Duration::from_secs(10), |e| {
        matches!(e, Event::AudioPolicyChanged(_))
    })
    .await;
    let Event::AudioPolicyChanged(p) = ev else {
        unreachable!()
    };
    assert_eq!(p, updated);
    let applied = alice.encoder_settings();
    assert_eq!(
        (
            applied.bitrate_bps,
            applied.fec,
            applied.dtx,
            applied.max_bandwidth,
            applied.signal,
            applied.complexity,
        ),
        (
            64_000,
            true,
            true,
            OpusBandwidth::Fullband,
            OpusSignal::Voice,
            3
        ),
        "{applied:?}"
    );
    assert_eq!(
        bob.encoder_settings(),
        bob_enc,
        "opted-out client is untouched"
    );

    // --- unpin: policy has no complexity hint, so the app baseline (9) returns.
    alice.set_complexity(None).unwrap();
    assert_eq!(alice.encoder_settings().complexity, baseline.complexity);
    let (rms, active) = stream_tone(&alice, &bob, 1.0).await;
    assert!(
        active >= 30 && rms > 0.1,
        "rms {rms} over {active} frames after update"
    );

    // --- leaving the last channel keeps the last policy (no flip-flop between channels).
    alice.leave_channel(channel).unwrap();
    wait_for(
        &alice,
        "alice left",
        Duration::from_secs(5),
        |e| matches!(e, Event::ChannelLeft { channel_id } if *channel_id == channel),
    )
    .await;
    assert_eq!(alice.audio_policy(), Some(updated));
    assert_eq!(alice.encoder_settings().bitrate_bps, 64_000);
    alice.disconnect();
    bob.disconnect();
}

/// Media over the control WebSocket when UDP is unusable. Alice sits on the second node
/// (`AURIX_E2E_WS2`, default `ws://127.0.0.1:8091`, media UDP `AURIX_E2E_UDP2`, default 10002),
/// Bob on the first one over plain UDP, so the tunnelled uplink also crosses the cascade.
///
/// 1. `TunnelOnly`: bind, both directions of audio, heartbeats, REST `media_path = tunnel`.
/// 2. With `AURIX_E2E_SUDO_IPTABLES=1` the host drops UDP to Alice's node: `Auto` falls back
///    to the tunnel at bind time, moves back to UDP when the block lifts and the re-probe
///    answers, falls back again mid-session after unanswered heartbeats, and a control-plane
///    drop while tunnelled resumes the same session over the tunnel — audio flows throughout.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn native_client_tunnels_media_when_udp_is_blocked() {
    let Some(env) = env() else {
        eprintln!("AURIX_E2E_API_KEY not set; skipping");
        return;
    };
    let env2 = Env {
        api: std::env::var("AURIX_E2E_API2").unwrap_or_else(|_| "http://127.0.0.1:8090".into()),
        ws: std::env::var("AURIX_E2E_WS2").unwrap_or_else(|_| "ws://127.0.0.1:8091".into()),
        api_key: env.api_key.clone(),
    };
    let udp2: u16 = std::env::var("AURIX_E2E_UDP2")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(10002);
    let http = reqwest::Client::new();
    let channel = create_channel(&env, &http).await;
    let (alice_token, alice_id) = issue_token(&env, &http, "tunnel-alice", "Alice", channel).await;
    let (bob_token, bob_id) = issue_token(&env, &http, "tunnel-bob", "Bob", channel).await;

    let mut bob_cfg = ClientConfig::new(ws_with_path(&env.ws), bob_token);
    bob_cfg.heartbeat_interval = Duration::from_millis(500);
    bob_cfg.dsp = DspConfig::BYPASS;
    let bob = Client::new(bob_cfg).unwrap();
    bob.connect().unwrap();
    wait_for(&bob, "bob media", Duration::from_secs(10), |e| {
        matches!(e, Event::MediaBound)
    })
    .await;
    assert_eq!(bob.media_path(), Some(MediaPath::Udp));
    bob.join_channel(channel, None).unwrap();
    wait_for(&bob, "bob join", Duration::from_secs(10), |e| {
        matches!(e, Event::ChannelJoined { .. })
    })
    .await;

    async fn join_and_check_audio(
        alice: &Client,
        bob: &Client,
        channel: ChannelId,
        alice_id: UserId,
        bob_id: UserId,
        join: bool,
        label: &str,
    ) {
        if join {
            alice.join_channel(channel, None).unwrap();
            wait_for(alice, "alice join", Duration::from_secs(10), |e| {
                matches!(e, Event::ChannelJoined { participants, .. }
                    if participants.iter().any(|p| p.user_id == bob_id))
            })
            .await;
            wait_for(
                bob,
                "alice joined",
                Duration::from_secs(10),
                |e| matches!(e, Event::ParticipantJoined { participant, .. } if participant.user_id == alice_id),
            )
            .await;
        }
        let (rms, active) = stream_tone(alice, bob, 1.2).await;
        eprintln!("[{label}] bob heard rms={rms:.3} over {active} frames");
        assert!(
            active >= 30 && (0.15..0.35).contains(&rms),
            "[{label}] alice → bob: rms={rms} active={active}"
        );
        let (rms, active) = stream_tone(bob, alice, 1.2).await;
        eprintln!("[{label}] alice heard rms={rms:.3} over {active} frames");
        assert!(
            active >= 30 && (0.15..0.35).contains(&rms),
            "[{label}] bob → alice: rms={rms} active={active}"
        );
    }

    // --- 1. forced tunnel.
    let mut alice_cfg = ClientConfig::new(ws_with_path(&env2.ws), alice_token.clone());
    alice_cfg.media_path = MediaPathPolicy::TunnelOnly;
    alice_cfg.heartbeat_interval = Duration::from_millis(500);
    alice_cfg.dsp = DspConfig::BYPASS;
    let alice = Client::new(alice_cfg).unwrap();
    alice.connect().unwrap();
    let Event::SessionReady(session) =
        wait_for(&alice, "alice session", Duration::from_secs(10), |e| {
            matches!(e, Event::SessionReady(_))
        })
        .await
    else {
        unreachable!()
    };
    assert!(
        session.media_tunnel,
        "node does not offer the media tunnel: {session:?}"
    );
    wait_for(&alice, "alice tunnel", Duration::from_secs(10), |e| {
        matches!(
            e,
            Event::MediaPathChanged {
                path: MediaPath::Tunnel,
                ..
            }
        )
    })
    .await;
    assert_eq!(alice.state(), ConnectionState::MediaBound);
    assert_eq!(alice.media_path(), Some(MediaPath::Tunnel));
    assert_eq!(
        session_media_path(&env2, &http, &session.session_id.to_string()).await,
        "tunnel"
    );
    join_and_check_audio(&alice, &bob, channel, alice_id, bob_id, true, "tunnel-only").await;
    let stats = alice.stats();
    assert_eq!(stats.media_path, Some(MediaPath::Tunnel), "{stats:?}");
    assert!(stats.media.rtt_samples > 0, "{stats:?}");
    assert_eq!(stats.media.heartbeats_lost_consecutive, 0, "{stats:?}");
    assert_eq!(stats.media.uplink_dropped, 0, "{stats:?}");
    assert!(
        stats.media.bad_auth == 0 && stats.media.replayed == 0,
        "{stats:?}"
    );
    assert!(stats.media.rtt_ms > 0.0, "{stats:?}");
    alice.disconnect();
    wait_for(
        &bob,
        "alice gone",
        Duration::from_secs(10),
        |e| matches!(e, Event::ParticipantLeft { user_id, .. } if *user_id == alice_id),
    )
    .await;

    if std::env::var("AURIX_E2E_SUDO_IPTABLES").is_err() {
        eprintln!("AURIX_E2E_SUDO_IPTABLES not set; skipping the blocked-UDP part");
        bob.disconnect();
        return;
    }

    // --- 2. Auto with UDP to Alice's node dropped by the host firewall.
    let mut block = UdpBlock::new(udp2);
    block.block();
    let proxy = Proxy::start(ws_upstream(&env2.ws)).await;
    let mut alice_cfg = ClientConfig::new(ws_url_via(&proxy, &ws_with_path(&env2.ws)), alice_token);
    alice_cfg.heartbeat_interval = Duration::from_millis(500);
    alice_cfg.udp_fallback_lost_heartbeats = 3;
    alice_cfg.udp_reprobe_interval = Duration::from_secs(3);
    alice_cfg.reconnect.initial_delay = Duration::from_millis(200);
    alice_cfg.dsp = DspConfig::BYPASS;
    let alice = Client::new(alice_cfg).unwrap();
    let t0 = Instant::now();
    alice.connect().unwrap();
    let Event::MediaPathChanged { reason, .. } =
        wait_for(&alice, "fallback at bind", Duration::from_secs(20), |e| {
            matches!(
                e,
                Event::MediaPathChanged {
                    path: MediaPath::Tunnel,
                    ..
                }
            )
        })
        .await
    else {
        unreachable!()
    };
    eprintln!("fell back to the tunnel after {:?}: {reason}", t0.elapsed());
    assert!(reason.contains("UDP bind failed"), "{reason}");
    let session = alice.session().unwrap();
    assert_eq!(
        session_media_path(&env2, &http, &session.session_id.to_string()).await,
        "tunnel"
    );
    join_and_check_audio(&alice, &bob, channel, alice_id, bob_id, true, "auto→tunnel").await;

    // UDP comes back: the periodic re-probe moves the media off the tunnel.
    block.unblock();
    let t0 = Instant::now();
    wait_for(&alice, "back to UDP", Duration::from_secs(20), |e| {
        matches!(
            e,
            Event::MediaPathChanged {
                path: MediaPath::Udp,
                ..
            }
        )
    })
    .await;
    eprintln!("moved back to UDP after {:?}", t0.elapsed());
    assert_eq!(alice.media_path(), Some(MediaPath::Udp));
    assert_eq!(
        session_media_path(&env2, &http, &session.session_id.to_string()).await,
        "udp"
    );
    join_and_check_audio(
        &alice,
        &bob,
        channel,
        alice_id,
        bob_id,
        false,
        "reprobed-udp",
    )
    .await;

    // UDP dies mid-session: unanswered heartbeats push the media onto the tunnel.
    block.block();
    let t0 = Instant::now();
    let Event::MediaPathChanged { reason, .. } =
        wait_for(&alice, "heartbeat fallback", Duration::from_secs(20), |e| {
            matches!(
                e,
                Event::MediaPathChanged {
                    path: MediaPath::Tunnel,
                    ..
                }
            )
        })
        .await
    else {
        unreachable!()
    };
    eprintln!("heartbeat fallback after {:?}: {reason}", t0.elapsed());
    assert!(reason.contains("heartbeats unanswered"), "{reason}");
    assert_eq!(
        session_media_path(&env2, &http, &session.session_id.to_string()).await,
        "tunnel"
    );
    assert_eq!(alice.session().unwrap().session_id, session.session_id);
    join_and_check_audio(
        &alice,
        &bob,
        channel,
        alice_id,
        bob_id,
        false,
        "heartbeat→tunnel",
    )
    .await;

    // Control-plane drop while tunnelled: resume brings the same session back over the tunnel
    // without waiting for UDP first (the block is remembered).
    proxy.kill();
    wait_for(&alice, "recovering", Duration::from_secs(15), |e| {
        matches!(e, Event::Recovering { .. })
    })
    .await;
    let t0 = Instant::now();
    let recovered = wait_for(&alice, "recovered", Duration::from_secs(15), |e| {
        matches!(e, Event::Recovered { .. })
    })
    .await;
    assert!(
        matches!(recovered, Event::Recovered { resumed: true, .. }),
        "{recovered:?}"
    );
    eprintln!("resumed over the tunnel in {:?}", t0.elapsed());
    let after = alice.session().unwrap();
    assert_eq!(after.session_id, session.session_id);
    assert_eq!(after.ssrc, session.ssrc);
    assert_eq!(alice.media_path(), Some(MediaPath::Tunnel));
    assert_eq!(alice.joined_channels(), vec![channel]);
    while let Some(ev) = bob.poll_event() {
        assert!(
            !matches!(ev, Event::ParticipantLeft { user_id, .. } if user_id == alice_id),
            "bob saw alice leave during resume"
        );
    }
    join_and_check_audio(
        &alice,
        &bob,
        channel,
        alice_id,
        bob_id,
        false,
        "resumed-tunnel",
    )
    .await;
    let stats = alice.stats();
    assert_eq!(stats.media.bad_auth, 0, "{stats:?}");
    assert_eq!(stats.media.replayed, 0, "{stats:?}");
    eprintln!("final stats: {stats:?}");

    block.unblock();
    alice.disconnect();
    bob.disconnect();
}

/// Large-channel path end to end through the native core: Alice speaks, Bob asks for the
/// server mix and hears the tone on one synthetic stereo stream (Alice's SSRC never reaches
/// his mixer), Carol joins with `speak: false` and is a hidden listener — receive-only,
/// counted in `participant_count`, absent from Alice's roster.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn native_client_takes_a_server_mixed_downlink_and_listener_role() {
    let Some(env) = env() else {
        eprintln!("AURIX_E2E_API_KEY not set; skipping");
        return;
    };
    let http = reqwest::Client::new();
    let channel = create_channel_with(
        &env,
        &http,
        serde_json::json!({
            "audience": {"hide_listeners": true, "mix_for_listeners": true, "max_speakers": 0, "max_streams": 2}
        }),
    )
    .await;
    let (alice_token, alice_id) = issue_token(&env, &http, "mix-alice", "Alice", channel).await;
    let (bob_token, bob_id) = issue_token(&env, &http, "mix-bob", "Bob", channel).await;
    let (carol_token, carol_id) =
        issue_token_with(&env, &http, "mix-carol", "Carol", channel, false).await;

    let mut alice_cfg = ClientConfig::new(ws_with_path(&env.ws), alice_token);
    alice_cfg.dsp = DspConfig::BYPASS;
    let alice = Client::new(alice_cfg).unwrap();
    let bob = Client::new(ClientConfig::new(ws_with_path(&env.ws), bob_token)).unwrap();
    let carol = Client::new(ClientConfig::new(ws_with_path(&env.ws), carol_token)).unwrap();
    alice.connect().unwrap();
    bob.connect().unwrap();
    carol.connect().unwrap();
    for (c, who) in [(&alice, "alice"), (&bob, "bob"), (&carol, "carol")] {
        let Event::SessionReady(info) = wait_for(c, who, Duration::from_secs(10), |e| {
            matches!(e, Event::SessionReady(_))
        })
        .await
        else {
            unreachable!()
        };
        assert!(info.downlink_mix, "node must advertise the downlink mix");
        wait_for(c, who, Duration::from_secs(10), |e| {
            matches!(e, Event::MediaBound)
        })
        .await;
    }
    let alice_ssrc = alice.session().unwrap().ssrc;

    // --- Bob switches to the mix before anyone speaks; the ack is authoritative.
    assert_eq!(bob.downlink_mode(), DownlinkMode::Streams);
    bob.set_downlink_mode(DownlinkMode::Mixed).unwrap();
    wait_for(&bob, "bob mixed", Duration::from_secs(5), |e| {
        matches!(e, Event::DownlinkModeChanged(DownlinkMode::Mixed))
    })
    .await;
    assert_eq!(bob.downlink_mode(), DownlinkMode::Mixed);

    alice.join_channel(channel, None).unwrap();
    let Event::ChannelJoined {
        role,
        participant_count,
        hidden_listeners,
        ..
    } = wait_for(&alice, "alice join", Duration::from_secs(10), |e| {
        matches!(e, Event::ChannelJoined { .. })
    })
    .await
    else {
        unreachable!()
    };
    assert_eq!(role, ChannelRole::Speaker);
    assert_eq!(participant_count, 1);
    assert!(hidden_listeners);
    assert!(alice.can_speak_in(channel));
    bob.join_channel(channel, None).unwrap();
    wait_for(&bob, "bob join", Duration::from_secs(10), |e| {
        matches!(
            e,
            Event::ChannelJoined {
                participant_count: 2,
                ..
            }
        )
    })
    .await;
    wait_for(&alice, "bob joined", Duration::from_secs(10), |e| {
        matches!(e, Event::ParticipantJoined { participant, .. } if participant.user_id == bob_id)
    })
    .await;

    // --- Carol: listener, hidden from Alice, counted, cannot speak.
    carol.join_channel(channel, None).unwrap();
    let Event::ChannelJoined {
        role,
        participant_count,
        participants,
        ..
    } = wait_for(&carol, "carol join", Duration::from_secs(10), |e| {
        matches!(e, Event::ChannelJoined { .. })
    })
    .await
    else {
        unreachable!()
    };
    assert_eq!(role, ChannelRole::Listener);
    assert_eq!(participant_count, 3);
    assert_eq!(participants.len(), 2, "carol sees both speakers");
    assert!(participants.iter().any(|p| p.user_id == alice_id));
    assert!(!carol.can_speak_in(channel));
    assert_eq!(carol.channel_role(channel), Some(ChannelRole::Listener));
    assert_eq!(bob.participant_count(channel), Some(2));

    // --- Alice speaks: Bob hears the tone on the channel's synthetic mixed stream only.
    let (rms, active) = stream_tone(&alice, &bob, 1.6).await;
    eprintln!("bob heard mixed rms={rms:.3} over {active} frames");
    assert!(active >= 50, "bob mixed only {active} active frames");
    assert!((0.10..0.35).contains(&rms), "unexpected mixed rms {rms}");
    let bob_stats = bob.stats();
    assert!(bob_stats.media.audio_frames_received >= 50, "{bob_stats:?}");
    assert!(
        bob_stats.streams.iter().all(|s| s.ssrc != alice_ssrc),
        "per-speaker stream leaked into a mixed downlink: {bob_stats:?}"
    );
    let mix = bob_stats
        .streams
        .iter()
        .find(|s| s.ssrc & 0x8000_0000 != 0)
        .expect("synthetic mixed stream");
    assert_eq!(mix.lost, 0, "mixed stream must be gapless: {mix:?}");
    let quiet_until = Instant::now() + Duration::from_secs(2);
    while Instant::now() < quiet_until {
        while let Some(ev) = alice.poll_event() {
            assert!(
                !matches!(&ev, Event::ParticipantJoined { participant, .. } if participant.user_id == carol_id),
                "hidden listener announced to a speaker"
            );
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    // --- Carol hears Alice too (listeners in a mix_for_listeners channel are mixed), and
    //     her own capture goes nowhere: Alice's mixer sees no Carol stream.
    let (rms_c, active_c) = stream_tone(&alice, &carol, 1.0).await;
    eprintln!("carol heard rms={rms_c:.3} over {active_c} frames");
    assert!(active_c >= 30, "carol mixed only {active_c} active frames");
    let carol_ssrc = carol.session().unwrap().ssrc;
    let (_, _) = stream_tone(&carol, &alice, 0.6).await;
    assert!(
        alice.stats().streams.iter().all(|s| s.ssrc != carol_ssrc),
        "listener audio reached a speaker: {:?}",
        alice.stats()
    );

    // --- back to per-speaker streams: Alice's own SSRC shows up in Bob's mixer again.
    bob.set_downlink_mode(DownlinkMode::Streams).unwrap();
    wait_for(&bob, "bob streams", Duration::from_secs(5), |e| {
        matches!(e, Event::DownlinkModeChanged(DownlinkMode::Streams))
    })
    .await;
    let (rms2, active2) = stream_tone(&alice, &bob, 1.0).await;
    eprintln!("bob heard per-speaker rms={rms2:.3} over {active2} frames");
    assert!(active2 >= 30, "bob mixed only {active2} active frames");
    assert!(
        bob.stats().streams.iter().any(|s| s.ssrc == alice_ssrc),
        "per-speaker stream expected after switching back: {:?}",
        bob.stats()
    );

    alice.disconnect();
    bob.disconnect();
    carol.disconnect();
}

/// Alice pushes `secs` of a left-only 440 Hz tone as two-channel capture while Bob mixes
/// `channels`-wide output; returns Bob's per-channel RMS and the active frame count.
async fn stream_left_tone(
    alice: &Client,
    bob: &Client,
    secs: f32,
    channels: u8,
) -> (Vec<f32>, usize) {
    let frames = (secs * 50.0) as usize;
    let ch = channels as usize;
    let mut phase = 0.0f32;
    let mut pcm = vec![0f32; FRAME_SAMPLES * 2];
    let mut out = vec![0i16; FRAME_SAMPLES * ch];
    let mut active = 0usize;
    let mut sum_sq = vec![0f64; ch];
    let mut samples = 0usize;
    for _ in 0..frames {
        for s in pcm.as_chunks_mut::<2>().0 {
            s[0] = 0.3 * phase.sin();
            s[1] = 0.0;
            phase += 2.0 * std::f32::consts::PI * 440.0 / SAMPLE_RATE as f32;
        }
        alice.push_capture_f32(&pcm, SAMPLE_RATE, 2);
        if bob.mix_output_i16(&mut out, channels) > 0 {
            active += 1;
            for (i, s) in out.iter().enumerate() {
                let v = *s as f64 / 32768.0;
                sum_sq[i % ch] += v * v;
            }
            samples += FRAME_SAMPLES;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let rms = sum_sq
        .iter()
        .map(|s| {
            if samples > 0 {
                (s / samples as f64).sqrt() as f32
            } else {
                0.0
            }
        })
        .collect();
    (rms, active)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn native_stereo_uplink_keeps_the_image_and_stays_mono_elsewhere() {
    let Some(env) = env() else {
        eprintln!("AURIX_E2E_API_KEY not set; skipping");
        return;
    };
    let http = reqwest::Client::new();
    let music = create_channel_with(
        &env,
        &http,
        serde_json::json!({"stereo": true, "audio_profile": "music", "bitrate": 96000}),
    )
    .await;
    let voice = create_channel(&env, &http).await;
    let (alice_token, alice_id) = issue_token(&env, &http, "stereo-alice", "Alice", music).await;
    let (bob_token, _) = issue_token(&env, &http, "stereo-bob", "Bob", music).await;
    let (dave_token, _) = issue_token(&env, &http, "stereo-dave", "Dave", voice).await;

    // Alice and Dave both *ask* for stereo; only the music channel's policy grants it.
    let stereo_cfg = |token: String| {
        let mut cfg = ClientConfig::new(ws_with_path(&env.ws), token);
        cfg.dsp = DspConfig::BYPASS;
        cfg.encoder = EncoderSettings {
            channels: 2,
            ..EncoderSettings::default()
        };
        cfg
    };
    let alice = Client::new(stereo_cfg(alice_token)).unwrap();
    let dave = Client::new(stereo_cfg(dave_token)).unwrap();
    let bob = Client::new(ClientConfig::new(ws_with_path(&env.ws), bob_token)).unwrap();
    for c in [&alice, &bob, &dave] {
        c.connect().unwrap();
    }
    for (c, who) in [(&alice, "alice"), (&bob, "bob"), (&dave, "dave")] {
        wait_for(c, who, Duration::from_secs(10), |e| {
            matches!(e, Event::MediaBound)
        })
        .await;
    }

    // --- policy: music channel → stereo + music application; voice channel → forced mono.
    alice.join_channel(music, None).unwrap();
    let Event::AudioPolicyChanged(p) =
        wait_for(&alice, "alice policy", Duration::from_secs(10), |e| {
            matches!(e, Event::AudioPolicyChanged(_))
        })
        .await
    else {
        unreachable!()
    };
    assert!(p.stereo, "{p:?}");
    assert_eq!(p.signal, OpusSignal::Music);
    let s = alice.encoder_settings();
    assert_eq!(s.channels, 2, "{s:?}");
    assert_eq!(s.signal, OpusSignal::Music);
    assert_eq!(s.bitrate_bps, 96_000);

    dave.join_channel(voice, None).unwrap();
    let Event::AudioPolicyChanged(p) =
        wait_for(&dave, "dave policy", Duration::from_secs(10), |e| {
            matches!(e, Event::AudioPolicyChanged(_))
        })
        .await
    else {
        unreachable!()
    };
    assert!(!p.stereo, "{p:?}");
    assert_eq!(
        dave.encoder_settings().channels,
        1,
        "voice channels force mono"
    );

    bob.join_channel(music, None).unwrap();
    wait_for(&bob, "bob join", Duration::from_secs(10), |e| {
        matches!(e, Event::ChannelJoined { .. })
    })
    .await;
    wait_for(&alice, "bob joined", Duration::from_secs(10), |e| {
        matches!(e, Event::ParticipantJoined { participant, .. } if participant.user_id != alice_id)
    })
    .await;

    // --- Bob (stereo output) hears Alice's left-only tone on the left; the right stays quiet.
    let (rms, active) = stream_left_tone(&alice, &bob, 1.6, 2).await;
    eprintln!(
        "bob heard stereo L={:.3} R={:.3} over {active} frames",
        rms[0], rms[1]
    );
    assert!(active >= 50, "bob mixed only {active} active frames");
    assert!((0.15..0.30).contains(&rms[0]), "left rms {}", rms[0]);
    assert!(
        rms[1] < rms[0] * 0.1,
        "stereo image lost: L={} R={}",
        rms[0],
        rms[1]
    );

    // --- a mono output gets the downmix (half the left level), not silence and not full level.
    let (rms_m, active_m) = stream_left_tone(&alice, &bob, 1.0, 1).await;
    eprintln!("bob heard mono {:.3} over {active_m} frames", rms_m[0]);
    assert!(active_m >= 30);
    assert!(
        (0.07..0.16).contains(&rms_m[0]),
        "mono downmix rms {}",
        rms_m[0]
    );

    // --- recording of the stereo channel: 2-channel OpusHead, stereo packets inside.
    let rec: serde_json::Value = http
        .post(format!("{}/v1/recordings/start", env.api))
        .header("x-api-key", &env.api_key)
        .json(&serde_json::json!({"channel_id": music, "user_id": alice_id}))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    let recording_id: uuid::Uuid = rec["id"].as_str().unwrap().parse().unwrap();
    wait_for(&alice, "recording", Duration::from_secs(10), |e| {
        matches!(e, Event::Recording { recording_id: r, active: true, .. } if *r == recording_id)
    })
    .await;
    alice
        .respond_recording_consent(recording_id, RecordingConsent::Accepted)
        .unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    let _ = stream_left_tone(&alice, &bob, 0.6, 2).await;
    http.post(format!("{}/v1/recordings/{}/stop", env.api, recording_id))
        .header("x-api-key", &env.api_key)
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    let ogg = http
        .get(format!(
            "{}/v1/recordings/{}/download",
            env.api, recording_id
        ))
        .header("x-api-key", &env.api_key)
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .bytes()
        .await
        .unwrap();
    let head = ogg
        .windows(8)
        .position(|w| w == b"OpusHead")
        .expect("OpusHead");
    // OpusHead: magic(8) version(1) channel_count(1) ...
    assert_eq!(ogg[head + 9], 2, "recording OpusHead must be 2-channel");
    let pages = ogg.windows(4).filter(|w| *w == b"OggS").count();
    assert!(pages >= 3, "no audio pages recorded ({pages})");

    alice.disconnect();
    bob.disconnect();
    dave.disconnect();
}

/// One 20 ms round of the per-participant scenario: Alice and Dave push tones (amplitude 0 =
/// silent), Bob renders his aggregate mix (stereo i16) and, when `pull` names her, pulls Alice
/// alone (stereo f32).
/// Returns `(mix_rms, mix_active_frames, pull_rms_left, pull_rms_right, pull_active_frames)`.
async fn stream_split(
    alice: &Client,
    dave: &Client,
    bob: &Client,
    alice_amp: f32,
    dave_amp: f32,
    secs: f32,
    pull: Option<UserId>,
) -> (f32, usize, f32, f32, usize) {
    let frames = (secs * 50.0) as usize;
    let (mut pa, mut pd) = (0.0f32, 0.0f32);
    let mut pcm_a = vec![0f32; FRAME_SAMPLES];
    let mut pcm_d = vec![0f32; FRAME_SAMPLES];
    let mut mixed = vec![0i16; FRAME_SAMPLES * 2];
    let mut pulled = vec![0f32; FRAME_SAMPLES * 2];
    let (mut mix_active, mut pull_active) = (0usize, 0usize);
    let (mut mix_sq, mut mix_n) = (0f64, 0usize);
    let (mut l_sq, mut r_sq, mut pull_n) = (0f64, 0f64, 0usize);
    for _ in 0..frames {
        for (a, d) in pcm_a.iter_mut().zip(pcm_d.iter_mut()) {
            *a = alice_amp * pa.sin();
            *d = dave_amp * pd.sin();
            pa += 2.0 * std::f32::consts::PI * 440.0 / SAMPLE_RATE as f32;
            pd += 2.0 * std::f32::consts::PI * 660.0 / SAMPLE_RATE as f32;
        }
        if alice_amp > 0.0 {
            alice.push_capture_f32(&pcm_a, SAMPLE_RATE, 1);
        }
        if dave_amp > 0.0 {
            dave.push_capture_f32(&pcm_d, SAMPLE_RATE, 1);
        }
        if bob.mix_output_i16(&mut mixed, 2) > 0 {
            mix_active += 1;
            for s in &mixed {
                let v = *s as f64 / 32768.0;
                mix_sq += v * v;
            }
            mix_n += mixed.len();
        }
        let pulled_frames = pull.map_or(0, |user| bob.pull_participant_f32(user, &mut pulled, 2));
        if pulled_frames > 0 {
            pull_active += 1;
            for [l, r] in pulled.as_chunks::<2>().0 {
                l_sq += (*l as f64).powi(2);
                r_sq += (*r as f64).powi(2);
            }
            pull_n += pulled.len() / 2;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let rms = |sq: f64, n: usize| {
        if n > 0 {
            (sq / n as f64).sqrt() as f32
        } else {
            0.0
        }
    };
    (
        rms(mix_sq, mix_n),
        mix_active,
        rms(l_sq, pull_n),
        rms(r_sq, pull_n),
        pull_active,
    )
}

/// Bob claims Alice and renders her through `pull_participant_*` (an engine emitter) while
/// Dave stays in the aggregate mix: the mix carries one voice, not two, the pull carries only
/// Alice (unpanned, L == R), `participant_streams` names her; with Dave silent the mix is
/// completely quiet; releasing the claim puts Alice back into the mix.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn native_per_participant_pull_keeps_claimed_talkers_out_of_the_mix() {
    let Some(env) = env() else {
        eprintln!("AURIX_E2E_API_KEY not set; skipping");
        return;
    };
    let http = reqwest::Client::new();
    let channel = create_channel(&env, &http).await;
    let (alice_token, alice_id) = issue_token(&env, &http, "pull-alice", "Alice", channel).await;
    let (dave_token, dave_id) = issue_token(&env, &http, "pull-dave", "Dave", channel).await;
    let (bob_token, bob_id) = issue_token(&env, &http, "pull-bob", "Bob", channel).await;

    let mk = |token: String| {
        let mut cfg = ClientConfig::new(ws_with_path(&env.ws), token);
        cfg.dsp = DspConfig::BYPASS;
        Client::new(cfg).unwrap()
    };
    let alice = mk(alice_token);
    let dave = mk(dave_token);
    let bob = mk(bob_token);
    for c in [&alice, &dave, &bob] {
        c.connect().unwrap();
    }
    for (c, who) in [(&alice, "alice"), (&dave, "dave"), (&bob, "bob")] {
        wait_for(c, who, Duration::from_secs(10), |e| {
            matches!(e, Event::MediaBound)
        })
        .await;
    }

    // Bob claims Alice *before* the roster is known: the claim is by user id and resolves to
    // her SSRC when the join ack arrives.
    bob.set_participant_claimed(alice_id, true);
    assert_eq!(bob.claimed_participants(), vec![alice_id]);

    for (c, who) in [(&alice, "alice"), (&dave, "dave"), (&bob, "bob")] {
        c.join_channel(channel, None).unwrap();
        wait_for(c, &format!("{who} join"), Duration::from_secs(10), |e| {
            matches!(e, Event::ChannelJoined { .. })
        })
        .await;
    }
    // Bob joined last, so Alice and Dave arrived with his join ack (roster), not as events.
    let seen: std::collections::HashSet<UserId> = bob
        .participants(channel)
        .into_iter()
        .map(|p| p.user_id)
        .filter(|u| *u != bob_id)
        .collect();
    assert!(
        seen.contains(&alice_id) && seen.contains(&dave_id),
        "{seen:?}"
    );

    // --- both talk: the mix carries Dave only (one voice ≈ 0.21, two would be ≈ 0.30), the
    // pull carries Alice, centred.
    let (mix, mix_active, l, r, pull_active) =
        stream_split(&alice, &dave, &bob, 0.3, 0.3, 1.6, Some(alice_id)).await;
    eprintln!("both talking: mix {mix:.3} ({mix_active} frames), pull L={l:.3} R={r:.3} ({pull_active} frames)");
    assert!(mix_active >= 50, "mix active {mix_active}");
    assert!(pull_active >= 50, "pull active {pull_active}");
    assert!(
        (0.15..0.26).contains(&mix),
        "mix rms {mix} (Alice leaked into the mix?)"
    );
    assert!((0.15..0.26).contains(&l), "pull L {l}");
    assert!((l - r).abs() < 0.01, "pull must be unpanned: L={l} R={r}");

    let streams = bob.participant_streams();
    eprintln!("streams: {streams:?}");
    let alice_stream = streams
        .iter()
        .find(|s| s.user_id == Some(alice_id))
        .expect("alice's stream is listed");
    assert!(!alice_stream.synthesized && !alice_stream.mixed && !alice_stream.stereo);
    assert!(
        streams.iter().any(|s| s.user_id == Some(dave_id)),
        "dave's stream is listed"
    );

    // --- Dave silent, Alice talking: the aggregate mix goes quiet once Dave's last buffered
    // frames (jitter depth + concealment) have drained.
    let (mix, mix_active, l, _r, pull_active) =
        stream_split(&alice, &dave, &bob, 0.3, 0.0, 1.0, Some(alice_id)).await;
    eprintln!(
        "alice only: mix {mix:.3} ({mix_active} frames), pull L={l:.3} ({pull_active} frames)"
    );
    assert!(pull_active >= 30, "pull active {pull_active}");
    assert!((0.15..0.26).contains(&l), "pull L {l}");
    assert!(
        mix_active <= 6,
        "claimed Alice must not reach the mix: rms {mix} over {mix_active} frames"
    );

    // --- release: Alice is mixed again.
    bob.set_participant_claimed(alice_id, false);
    assert!(bob.claimed_participants().is_empty());
    let (mix, mix_active, _l, _r, _p) =
        stream_split(&alice, &dave, &bob, 0.3, 0.0, 1.0, None).await;
    eprintln!("released: mix {mix:.3} ({mix_active} frames)");
    assert!(mix_active >= 30, "mix active {mix_active}");
    assert!((0.15..0.26).contains(&mix), "mix rms {mix}");

    alice.disconnect();
    dave.disconnect();
    bob.disconnect();
}

/// Group E2EE between native clients: sender keys travel wrapped through the node, plaintext
/// and unkeyed sessions are refused, join/leave rotate the generation, a resume keeps the keys.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn native_group_e2ee_rotates_on_join_and_leave_and_refuses_plaintext() {
    let Some(env) = env() else {
        eprintln!("AURIX_E2E_API_KEY not set; skipping");
        return;
    };
    let http = reqwest::Client::new();
    let channel = create_channel_with(&env, &http, serde_json::json!({"e2ee": true})).await;
    let (alice_token, alice_id) =
        issue_token(&env, &http, "native-e2ee-alice", "Alice", channel).await;
    let (bob_token, bob_id) = issue_token(&env, &http, "native-e2ee-bob", "Bob", channel).await;
    let (carol_token, _) = issue_token(&env, &http, "native-e2ee-carol", "Carol", channel).await;
    let (dave_token, dave_id) = issue_token(&env, &http, "native-e2ee-dave", "Dave", channel).await;

    // Alice uses a pinned identity so her fingerprint is reproducible; the others are ephemeral.
    let proxy = Proxy::start(ws_upstream(&env.ws)).await;
    let mut alice_cfg = ClientConfig::new(ws_url_via(&proxy, &ws_with_path(&env.ws)), alice_token);
    alice_cfg.dsp = DspConfig::BYPASS;
    alice_cfg.heartbeat_interval = Duration::from_millis(500);
    alice_cfg.reconnect.initial_delay = Duration::from_millis(200);
    alice_cfg.e2ee_identity = Some([7u8; 32]);
    let expected_fp = Client::new(alice_cfg.clone()).unwrap().e2ee_fingerprint();
    let alice = Client::new(alice_cfg).unwrap();
    assert_eq!(alice.e2ee_fingerprint(), expected_fp);
    let bob = Client::new(ClientConfig::new(ws_with_path(&env.ws), bob_token)).unwrap();
    let mut carol_cfg = ClientConfig::new(ws_with_path(&env.ws), carol_token);
    carol_cfg.e2ee = false;
    let carol = Client::new(carol_cfg).unwrap();
    let mut dave_cfg = ClientConfig::new(ws_with_path(&env.ws), dave_token);
    dave_cfg.dsp = DspConfig::BYPASS;
    let dave = Client::new(dave_cfg).unwrap();

    for (c, who) in [
        (&alice, "alice"),
        (&bob, "bob"),
        (&carol, "carol"),
        (&dave, "dave"),
    ] {
        c.connect().unwrap();
        wait_for(c, &format!("{who} media"), Duration::from_secs(10), |e| {
            matches!(e, Event::MediaBound)
        })
        .await;
    }

    // --- a session without E2EE support cannot enter the channel at all.
    let rid = carol.join_channel(channel, None).unwrap();
    wait_for(&carol, "carol refused", Duration::from_secs(10), |e| {
        matches!(e, Event::RequestFailed { request_id, code, .. }
            if *request_id == rid && code == "E2EE_REQUIRED")
    })
    .await;
    assert!(carol.joined_channels().is_empty());

    // --- the operator cannot record the channel either.
    let rec = http
        .post(format!("{}/v1/recordings/start", env.api))
        .header("x-api-key", &env.api_key)
        .json(&serde_json::json!({"channel_id": channel, "user_id": alice_id}))
        .send()
        .await
        .unwrap();
    assert_eq!(
        rec.status(),
        400,
        "recording an e2ee channel: {:?}",
        rec.text().await
    );

    // --- Alice and Bob join: each learns the other's identity and receives a sender key.
    alice.join_channel(channel, None).unwrap();
    wait_for(
        &alice,
        "alice join",
        Duration::from_secs(10),
        |e| matches!(e, Event::ChannelJoined { channel_id, .. } if *channel_id == channel),
    )
    .await;
    bob.join_channel(channel, None).unwrap();
    wait_for(&bob, "bob join", Duration::from_secs(10), |e| {
        matches!(e, Event::ChannelJoined { .. })
    })
    .await;
    let bob_fp_seen = wait_for(&alice, "alice learns bob", Duration::from_secs(10), |e| {
        matches!(e, Event::E2eePeerKey { user_id, previous_fingerprint: None, .. } if *user_id == bob_id)
    })
    .await;
    let alice_fp_seen = wait_for(
        &bob,
        "bob learns alice",
        Duration::from_secs(10),
        |e| matches!(e, Event::E2eePeerKey { user_id, .. } if *user_id == alice_id),
    )
    .await;
    if let Event::E2eePeerKey { fingerprint, .. } = &alice_fp_seen {
        assert_eq!(fingerprint, &expected_fp, "bob sees alice's real identity");
    }
    if let Event::E2eePeerKey { fingerprint, .. } = &bob_fp_seen {
        assert_eq!(
            Some(fingerprint.clone()),
            alice.e2ee_peer_fingerprint(bob_id)
        );
        assert_eq!(fingerprint, &bob.e2ee_fingerprint());
    }
    // A new member costs each side one rotation; the key Bob receives is the rotated one.
    let mut gen_two = None;
    wait_for(&alice, "alice keys bob", Duration::from_secs(10), |e| {
        if let Event::E2eeKeyRotated { generation } = e {
            gen_two = Some(*generation);
        }
        matches!(e, Event::E2eePeerDecryptable { user_id, decryptable: true } if *user_id == bob_id)
    })
    .await;
    let gen_two = gen_two.expect("alice rotated before bob could decrypt her");
    wait_for(&bob, "bob keys alice", Duration::from_secs(10), |e| {
        matches!(e, Event::E2eePeerDecryptable { user_id, decryptable: true } if *user_id == alice_id)
    })
    .await;
    assert!(alice.e2ee_peer_decryptable(bob_id) && bob.e2ee_peer_decryptable(alice_id));

    // --- audio flows encrypted end to end and decrypts on Bob's side.
    let (rms, active) = stream_tone(&alice, &bob, 1.6).await;
    eprintln!("bob heard e2ee rms={rms:.3} over {active} frames");
    assert!(active >= 50, "bob mixed only {active} active frames");
    assert!((0.15..0.35).contains(&rms), "unexpected rms {rms}");
    let a = alice.stats().transmit;
    assert!(a.frames_e2ee >= 60, "alice sealed {} frames", a.frames_e2ee);
    assert_eq!(
        a.frames_e2ee, a.frames_sent,
        "every frame to the channel was sealed"
    );
    let b = bob.stats().transmit;
    assert_eq!(
        b.e2ee_undecryptable, 0,
        "bob dropped {} frames",
        b.e2ee_undecryptable
    );

    // --- Dave joins: Alice rotates to a newer generation, both peers decrypt him and he them.
    dave.join_channel(channel, None).unwrap();
    wait_for(&dave, "dave join", Duration::from_secs(10), |e| {
        matches!(e, Event::ChannelJoined { .. })
    })
    .await;
    let mut gen_three = None;
    wait_for(&alice, "alice decrypts dave", Duration::from_secs(10), |e| {
        if let Event::E2eeKeyRotated { generation } = e {
            gen_three = Some(*generation);
        }
        matches!(e, Event::E2eePeerDecryptable { user_id, decryptable: true } if *user_id == dave_id)
    })
    .await;
    let gen_three = gen_three.expect("alice rotated for dave");
    assert_eq!(gen_three, gen_two.wrapping_add(1));
    wait_for(&bob, "bob decrypts dave", Duration::from_secs(10), |e| {
        matches!(e, Event::E2eePeerDecryptable { user_id, decryptable: true } if *user_id == dave_id)
    })
    .await;
    for peer in [alice_id, bob_id] {
        wait_for(&dave, "dave decrypts", Duration::from_secs(10), |e| {
            matches!(e, Event::E2eePeerDecryptable { user_id, decryptable: true } if *user_id == peer)
        })
        .await;
    }
    let (rms, active) = stream_tone(&dave, &alice, 1.0).await;
    eprintln!("alice heard dave rms={rms:.3} over {active} frames");
    assert!(
        active >= 30 && (0.15..0.35).contains(&rms),
        "dave → alice: {rms} / {active}"
    );
    let (rms, active) = stream_tone(&alice, &dave, 1.0).await;
    eprintln!("dave heard alice rms={rms:.3} over {active} frames");
    assert!(
        active >= 30 && (0.15..0.35).contains(&rms),
        "alice → dave: {rms} / {active}"
    );

    // --- Alice's control socket dies; the resumed session keeps every key and generation.
    proxy.kill();
    let recovered = wait_for(&alice, "alice recovered", Duration::from_secs(20), |e| {
        matches!(e, Event::Recovered { .. })
    })
    .await;
    assert!(matches!(recovered, Event::Recovered { resumed: true, .. }));
    assert!(alice.e2ee_peer_decryptable(bob_id) && alice.e2ee_peer_decryptable(dave_id));
    let (rms, active) = stream_tone(&alice, &bob, 1.0).await;
    eprintln!("bob heard alice after resume rms={rms:.3} over {active} frames");
    assert!(
        active >= 30 && (0.15..0.35).contains(&rms),
        "after resume: {rms} / {active}"
    );

    // --- Dave leaves: he can no longer be decrypted and the remaining members rotate away from
    // the key he holds.
    dave.leave_channel(channel).unwrap();
    let mut dropped = false;
    let ev = wait_for(&alice, "alice rotates away from dave", Duration::from_secs(10), |e| {
        if matches!(e, Event::E2eePeerDecryptable { user_id, decryptable: false } if *user_id == dave_id)
        {
            dropped = true;
        }
        matches!(e, Event::E2eeKeyRotated { .. })
    })
    .await;
    assert!(dropped, "dave was dropped before the rotation");
    let Event::E2eeKeyRotated {
        generation: gen_four,
    } = ev
    else {
        unreachable!()
    };
    assert_eq!(gen_four, gen_three.wrapping_add(1));
    assert!(!alice.e2ee_peer_decryptable(dave_id));
    wait_for(
        &bob,
        "bob rotated after leave",
        Duration::from_secs(10),
        |e| matches!(e, Event::E2eeKeyRotated { .. }),
    )
    .await;
    let (rms, active) = stream_tone(&alice, &bob, 1.0).await;
    eprintln!("bob heard alice after rotation rms={rms:.3} over {active} frames");
    assert!(
        active >= 30 && (0.15..0.35).contains(&rms),
        "after leave: {rms} / {active}"
    );
    assert_eq!(bob.stats().transmit.e2ee_undecryptable, 0);

    // --- Bob leaves the channel entirely: nothing of Alice is decryptable to him any more.
    bob.leave_channel(channel).unwrap();
    wait_for(&bob, "bob left", Duration::from_secs(10), |e| {
        matches!(e, Event::ChannelLeft { .. })
    })
    .await;
    assert!(!bob.e2ee_peer_decryptable(alice_id));

    alice.disconnect();
    bob.disconnect();
    carol.disconnect();
    dave.disconnect();
}
