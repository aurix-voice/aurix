//! Lossy-WAN and network-migration E2E of the native client against a running Aurix node,
//! with the impairment produced by Linux netem on loopback (`tools/netem/shape.sh`).
//! Skipped unless `AURIX_E2E_API_KEY` **and** `AURIX_E2E_SUDO_TC=1` are set (the shaper needs
//! `sudo tc`); the node must listen on loopback with QUIC enabled:
//!
//! ```text
//! AURIX_E2E_API=http://127.0.0.1:8080 AURIX_E2E_WS=ws://127.0.0.1:8081 AURIX_E2E_UDP=10000 \
//! AURIX_E2E_API_KEY=aurx_... AURIX_E2E_SUDO_TC=1 cargo test -p aurix-client --test netem_live
//! ```
//!
//! What is asserted is quantitative — measured loss, MOS, profile tiers, the share of lost
//! frames rebuilt from FEC/DRED, RTT under added delay, audio continuity across a migration —
//! not just "it did not crash". The shaping is host-global, so the tests here serialize.

use aurix_client::audio::{FRAME_SAMPLES, SAMPLE_RATE};
use aurix_client::events::Event;
use aurix_client::{
    Client, ClientConfig, ClientStats, DspConfig, LossProfile, MediaPath, MediaPathPolicy,
};
use aurix_common::types::{ChannelId, UserId};
use std::path::PathBuf;
use std::process::Command;
use std::time::{Duration, Instant};
use tokio::sync::Mutex;

struct Env {
    api: String,
    ws: String,
    api_key: String,
    udp: u16,
}

fn env() -> Option<Env> {
    let api_key = std::env::var("AURIX_E2E_API_KEY").ok()?;
    if std::env::var("AURIX_E2E_SUDO_TC").is_err() {
        eprintln!("AURIX_E2E_SUDO_TC not set; skipping (netem needs sudo tc)");
        return None;
    }
    Some(Env {
        api: std::env::var("AURIX_E2E_API").unwrap_or_else(|_| "http://127.0.0.1:8080".into()),
        ws: std::env::var("AURIX_E2E_WS").unwrap_or_else(|_| "ws://127.0.0.1:8081".into()),
        api_key,
        udp: std::env::var("AURIX_E2E_UDP")
            .ok()
            .and_then(|p| p.parse().ok())
            .unwrap_or(10000),
    })
}

/// One shaper at a time: the qdisc sits on the shared loopback device.
static SHAPER: Mutex<()> = Mutex::const_new(());

/// netem on the node's media port through `tools/netem/shape.sh` (sudo). Cleared on drop,
/// also when the test panics.
struct Shaper {
    script: PathBuf,
    port: u16,
}

impl Shaper {
    fn new(port: u16) -> Self {
        let script = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tools/netem/shape.sh");
        let s = Self { script, port };
        s.clear();
        s
    }

    fn run(&self, args: &[&str]) {
        let out = Command::new("sudo")
            .arg("-n")
            .arg(&self.script)
            .args(args)
            .output()
            .expect("run sudo tools/netem/shape.sh");
        assert!(
            out.status.success(),
            "shape.sh {args:?} failed: {}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
    }

    /// `down`: what the node sends to its clients; `up`: what they send to it (netem syntax).
    fn apply(&self, down: Option<&str>, up: Option<&str>) {
        let port = self.port.to_string();
        let mut args = vec!["apply", port.as_str()];
        if let Some(d) = down {
            args.extend(["--down", d]);
        }
        if let Some(u) = up {
            args.extend(["--up", u]);
        }
        eprintln!("[netem] {}", args.join(" "));
        self.run(&args);
    }

    fn clear(&self) {
        self.run(&["clear"]);
    }
}

impl Drop for Shaper {
    fn drop(&mut self) {
        self.clear();
    }
}

async fn create_channel(env: &Env, http: &reqwest::Client) -> ChannelId {
    let ch: serde_json::Value = http
        .post(format!("{}/v1/channels", env.api))
        .header("x-api-key", &env.api_key)
        .json(&serde_json::json!({
            "name": format!("netem-e2e-{}", uuid::Uuid::now_v7()),
            "config": {},
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
        }
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// A talker and a listener in one channel on the node, media over `path`, DSP bypassed, VAD
/// off (a continuous uplink so loss is measured over every frame), the UDP → tunnel fallback
/// disabled (heartbeat acks are lossy too; this test is about the lossy link, not leaving it),
/// one-second quality reports.
async fn pair(env: &Env, path: MediaPathPolicy) -> (Client, Client, ChannelId) {
    let http = reqwest::Client::new();
    let channel = create_channel(env, &http).await;
    let (alice_token, alice_id) = issue_token(env, &http, "netem-alice", "Alice", channel).await;
    let (bob_token, _) = issue_token(env, &http, "netem-bob", "Bob", channel).await;
    let mk = |token: String| {
        let mut cfg = ClientConfig::new(ws_with_path(&env.ws), token);
        cfg.heartbeat_interval = Duration::from_secs(1);
        cfg.udp_fallback_lost_heartbeats = 0;
        cfg.media_path = path;
        cfg.dsp = DspConfig::BYPASS;
        cfg.vad_gate = false;
        Client::new(cfg).unwrap()
    };
    let alice = mk(alice_token);
    let bob = mk(bob_token);
    alice.connect().unwrap();
    bob.connect().unwrap();
    for (c, who) in [(&alice, "alice"), (&bob, "bob")] {
        wait_for(c, &format!("{who} media"), Duration::from_secs(15), |e| {
            matches!(e, Event::MediaBound)
        })
        .await;
    }
    alice.join_channel(channel, None).unwrap();
    wait_for(&alice, "alice join", Duration::from_secs(10), |e| {
        matches!(e, Event::ChannelJoined { .. })
    })
    .await;
    bob.join_channel(channel, None).unwrap();
    wait_for(&bob, "bob join", Duration::from_secs(10), |e| {
        matches!(e, Event::ChannelJoined { participants, .. }
            if participants.iter().any(|p| p.user_id == alice_id))
    })
    .await;
    (alice, bob, channel)
}

/// Syllable-like bursts (glottal-pulse harmonics with moving formant emphasis) separated by
/// short pauses; SILK keeps classifying it as speech, so LBRR (FEC) and DRED are produced —
/// a pure tone would be coded as music without either.
fn speech_like(frame: usize, pcm: &mut [f32]) {
    let mut seed = 0x1234_5678u32 ^ (frame as u32).wrapping_mul(0x9E37_79B9);
    for (i, out) in pcm.iter_mut().enumerate() {
        let t = (frame * FRAME_SAMPLES + i) as f32 / SAMPLE_RATE as f32;
        let syllable = (t / 0.18).floor();
        let phase = (t % 0.18) / 0.18;
        let voiced = phase < 0.7;
        let f0 = 110.0 + 25.0 * (syllable * 0.9).sin() + 15.0 * (t * 4.0).sin();
        let formant = 500.0 + 400.0 * ((syllable * 1.7).sin() + 1.0);
        let env = if voiced {
            (phase / 0.1).min(1.0) * ((0.7 - phase) / 0.1).min(1.0)
        } else {
            0.0
        };
        let mut s = 0.0;
        for h in 1..=12 {
            let f = f0 * h as f32;
            let weight = 1.0 / (1.0 + ((f - formant) / 300.0).powi(2));
            s += (2.0 * std::f32::consts::PI * f * t).sin() * weight;
        }
        seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        let noise = ((seed >> 9) as f32 / (1u32 << 23) as f32 - 1.0) * 0.02;
        *out = (s * 0.4 * env + noise) * 0.27;
    }
}

/// What one streaming stretch produced.
#[derive(Debug)]
struct Stretch {
    frames: usize,
    /// Output frames in which Bob's mixer had audio.
    active: usize,
    rms: f32,
    alice: Vec<Event>,
    bob: Vec<Event>,
    /// Bob's stats at the start and at the end of the stretch.
    bob_before: ClientStats,
    bob_after: ClientStats,
}

impl Stretch {
    fn lost(&self) -> u64 {
        self.bob_after.frames_lost - self.bob_before.frames_lost
    }
    fn recovered(&self) -> u64 {
        (self.bob_after.frames_fec_recovered + self.bob_after.frames_dred_recovered)
            - (self.bob_before.frames_fec_recovered + self.bob_before.frames_dred_recovered)
    }
    /// Share of the lost frames rebuilt from FEC/DRED rather than concealed.
    fn recovery_ratio(&self) -> f32 {
        if self.lost() == 0 {
            return 1.0;
        }
        self.recovered() as f32 / self.lost() as f32
    }
    fn alice_profiles(&self) -> Vec<(LossProfile, f32)> {
        self.alice
            .iter()
            .filter_map(|e| match e {
                Event::LossProfileChanged {
                    profile,
                    uplink_loss_percent,
                } => Some((*profile, *uplink_loss_percent)),
                _ => None,
            })
            .collect()
    }
}

/// Alice talks speech-like audio for `secs` while Bob renders it; `clock` keeps the signal
/// continuous across stretches. Events of both are drained (and printed) as they come.
async fn stream(alice: &Client, bob: &Client, secs: f32, clock: &mut usize) -> Stretch {
    let frames = (secs * 50.0) as usize;
    let mut pcm = vec![0f32; FRAME_SAMPLES];
    let mut out = vec![0i16; FRAME_SAMPLES * 2];
    let mut st = Stretch {
        frames,
        active: 0,
        rms: 0.0,
        alice: Vec::new(),
        bob: Vec::new(),
        bob_before: bob.stats(),
        bob_after: ClientStats::default(),
    };
    let mut sum_sq = 0f64;
    let mut samples = 0usize;
    let mut next = Instant::now();
    for _ in 0..frames {
        speech_like(*clock, &mut pcm);
        *clock += 1;
        alice.push_capture_f32(&pcm, SAMPLE_RATE, 1);
        if bob.mix_output_i16(&mut out, 2) > 0 {
            st.active += 1;
            for s in &out {
                let v = *s as f64 / 32768.0;
                sum_sq += v * v;
            }
            samples += out.len();
        }
        while let Some(ev) = alice.poll_event() {
            eprintln!("  [alice] {ev:?}");
            st.alice.push(ev);
        }
        while let Some(ev) = bob.poll_event() {
            if !matches!(
                ev,
                Event::ChannelEnergy { .. } | Event::ParticipantSpeaking { .. }
            ) {
                eprintln!("  [bob] {ev:?}");
            }
            st.bob.push(ev);
        }
        next += Duration::from_millis(20);
        tokio::time::sleep_until(next.into()).await;
    }
    st.rms = if samples > 0 {
        (sum_sq / samples as f64).sqrt() as f32
    } else {
        0.0
    };
    st.bob_after = bob.stats();
    let s = &st.bob_after;
    eprintln!(
        "[stretch {secs}s] active={}/{} rms={:.3} lost={} recovered={} ratio={:.2} bob loss={:.1}% mos={:.2} jitter={:.1}ms | alice profile={:?} server={:?}",
        st.active,
        st.frames,
        st.rms,
        st.lost(),
        st.recovered(),
        st.recovery_ratio(),
        s.loss_percent,
        s.mos,
        s.jitter_ms,
        alice.stats().loss_profile,
        alice.stats().server,
    );
    st
}

fn assert_heard(st: &Stretch, label: &str, min_active_share: f32) {
    let share = st.active as f32 / st.frames.max(1) as f32;
    assert!(
        share >= min_active_share && st.rms >= 0.03,
        "[{label}] bob did not hear alice: active {}/{} ({share:.2}), rms {:.3}",
        st.active,
        st.frames,
        st.rms
    );
}

/// Downlink and uplink loss on the node's UDP port, one direction at a time, with the
/// sender's redundancy following whichever direction is lossy:
///
/// 1. Clean loopback: no loss, `Low` profile, MOS ≥ 4.
/// 2. 20 % loss on the **downlink** only (Bob's side; Alice's packets reach the node intact):
///    the node reports Bob's downlink loss back to Alice as `receivers_loss_percent`, Alice
///    moves to `High` on it, and from then on Bob rebuilds most of what he loses from Alice's
///    FEC/DRED instead of concealing it; his MOS drops below 3.5.
/// 3. Cleared: Alice leaves `High` after the dwell, Bob's MOS recovers.
/// 4. 12 % loss + 20 ± 10 ms delay with reordering on the **uplink** only: the node measures
///    the loss on Alice's packets (`uplink_loss_percent`), Alice goes `High`, Bob still hears
///    her (reordered packets are put back, the gaps rebuilt).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn lossy_link_is_measured_in_both_directions_and_protected_by_the_sender() {
    let Some(env) = env() else {
        eprintln!("skipping");
        return;
    };
    let _lock = SHAPER.lock().await;
    let shaper = Shaper::new(env.udp);
    let (alice, bob, _channel) = pair(&env, MediaPathPolicy::UdpOnly).await;
    assert_eq!(alice.media_path(), Some(MediaPath::Udp));
    let mut clock = 0usize;

    // 1. clean
    let clean = stream(&alice, &bob, 4.0, &mut clock).await;
    assert_heard(&clean, "clean", 0.8);
    assert!(
        clean.lost() <= 2,
        "loopback lost {} frames without shaping",
        clean.lost()
    );
    assert_eq!(alice.stats().loss_profile, LossProfile::Low);
    assert!(
        clean.bob_after.mos >= 4.0,
        "clean MOS {:.2}",
        clean.bob_after.mos
    );

    // 2. downlink loss → the receiver's loss drives the sender's profile
    shaper.apply(Some("loss 20%"), None);
    let onset = Instant::now();
    let mut went_high: Option<(f32, Duration)> = None;
    let mut pre_lost = 0u64;
    let mut pre_recovered = 0u64;
    while went_high.is_none() {
        assert!(
            onset.elapsed() < Duration::from_secs(20),
            "alice never reached the High profile from Bob's downlink loss; server quality: {:?}",
            alice.stats().server
        );
        let st = stream(&alice, &bob, 1.0, &mut clock).await;
        pre_lost += st.lost();
        pre_recovered += st.recovered();
        went_high = st
            .alice_profiles()
            .into_iter()
            .find(|(p, _)| *p == LossProfile::High)
            .map(|(_, loss)| (loss, onset.elapsed()));
    }
    let (protect_loss, after) = went_high.unwrap();
    eprintln!(
        "alice → High after {after:?} protecting {protect_loss:.1}% (before: lost {pre_lost}, recovered {pre_recovered})"
    );
    assert!(protect_loss >= 10.0, "High protects only {protect_loss}%");
    let server = alice
        .stats()
        .server
        .expect("a NetworkQuality reached alice");
    assert!(
        server.receivers_loss_percent >= 10.0 && server.uplink_loss_percent < 5.0,
        "the loss must show up on the receivers' side only: {server:?}"
    );
    let lossy = stream(&alice, &bob, 8.0, &mut clock).await;
    assert_heard(&lossy, "20% downlink loss", 0.7);
    let expected_lost = (lossy.frames as f32 * 0.2) as u64;
    assert!(
        lossy.lost() >= expected_lost / 2 && lossy.lost() <= expected_lost * 2,
        "20 % loss over {} frames lost {} (expected ≈ {expected_lost})",
        lossy.frames,
        lossy.lost()
    );
    assert!(
        lossy.recovery_ratio() >= 0.5,
        "High profile rebuilt only {:.2} of the lost frames ({} of {})",
        lossy.recovery_ratio(),
        lossy.recovered(),
        lossy.lost()
    );
    assert!(
        lossy.bob_after.loss_percent >= 10.0,
        "bob measured {:.1}% loss",
        lossy.bob_after.loss_percent
    );
    assert!(
        lossy.bob_after.mos < 3.5,
        "MOS {:.2} under 20 % loss",
        lossy.bob_after.mos
    );
    assert_eq!(alice.stats().loss_profile, LossProfile::High);

    // 3. cleared → relaxes after the dwell (the node pushes a report on the tier change, then
    //    every fifth period), MOS back up
    shaper.clear();
    let onset = Instant::now();
    loop {
        let st = stream(&alice, &bob, 2.0, &mut clock).await;
        if alice.stats().loss_profile != LossProfile::High && st.lost() == 0 {
            break;
        }
        assert!(
            onset.elapsed() < Duration::from_secs(40),
            "alice still in {:?} {:?} after the link cleared (server {:?})",
            alice.stats().loss_profile,
            onset.elapsed(),
            alice.stats().server
        );
    }
    let recovered = stream(&alice, &bob, 3.0, &mut clock).await;
    assert_heard(&recovered, "cleared", 0.8);
    assert!(
        recovered.bob_after.mos >= 4.0,
        "MOS {:.2} after the link cleared",
        recovered.bob_after.mos
    );

    // 4. uplink loss + jitter + reordering → the node's own measurement drives the profile
    shaper.apply(None, Some("loss 12% delay 20ms 10ms reorder 25% 50%"));
    let onset = Instant::now();
    loop {
        let st = stream(&alice, &bob, 1.0, &mut clock).await;
        if st
            .alice_profiles()
            .iter()
            .any(|(p, _)| *p == LossProfile::High)
        {
            break;
        }
        assert!(
            onset.elapsed() < Duration::from_secs(20),
            "alice never reached High from uplink loss; server quality: {:?}",
            alice.stats().server
        );
    }
    let server = alice
        .stats()
        .server
        .expect("a NetworkQuality reached alice");
    assert!(
        server.uplink_loss_percent >= 6.0,
        "the node measured only {:.1}% uplink loss: {server:?}",
        server.uplink_loss_percent
    );
    let uplink = stream(&alice, &bob, 6.0, &mut clock).await;
    assert_heard(&uplink, "12% uplink loss + reorder", 0.7);
    assert!(
        uplink.recovery_ratio() >= 0.4,
        "rebuilt only {:.2} of the frames lost on the uplink ({} of {})",
        uplink.recovery_ratio(),
        uplink.recovered(),
        uplink.lost()
    );
    shaper.clear();
    alice.disconnect();
    bob.disconnect();
}

/// A QUIC session under WAN-like delay/jitter/loss changes its local address mid-talk
/// (`network_changed()`, the Wi-Fi ↔ cellular case): the connection migrates, no re-bind, no
/// new session, audio keeps flowing, and the added delay is visible in the media heartbeat
/// RTT.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn quic_session_migrates_under_wan_conditions() {
    let Some(env) = env() else {
        eprintln!("skipping");
        return;
    };
    let _lock = SHAPER.lock().await;
    let shaper = Shaper::new(env.udp);
    let (alice, bob, _channel) = pair(&env, MediaPathPolicy::Auto).await;
    assert_eq!(
        alice.media_path(),
        Some(MediaPath::Quic),
        "the node under test must offer QUIC"
    );
    let session = alice.session().expect("alice session");
    let mut clock = 0usize;

    // 40 ± 15 ms each way, 3 % loss in both directions: a decent mobile link.
    shaper.apply(
        Some("delay 40ms 15ms loss 3%"),
        Some("delay 40ms 15ms loss 3%"),
    );
    let before = stream(&alice, &bob, 6.0, &mut clock).await;
    assert_heard(&before, "wan before migration", 0.7);
    // 2 × (40 ± 15) ms on top of loopback: every heartbeat sees 50..110 ms.
    let media = alice.stats().media;
    assert!(
        (45.0..200.0).contains(&media.rtt_ms) && media.rtt_max_ms >= 60.0,
        "heartbeat RTT {:.1} ms (max {:.1}) does not show the 2 × 40 ms of added delay",
        media.rtt_ms,
        media.rtt_max_ms
    );
    assert!(
        before.bob_after.jitter_ms >= 3.0,
        "jitter {:.1} ms under ±15 ms netem",
        before.bob_after.jitter_ms
    );
    let addr_before = alice.media_local_addr().expect("quic local addr");

    alice.network_changed().unwrap();
    let migrated = wait_for(&alice, "migration", Duration::from_secs(10), |e| {
        matches!(e, Event::MediaPathChanged { path: MediaPath::Quic, reason } if reason.contains("migrated"))
    })
    .await;
    eprintln!("{migrated:?}");
    let addr_after = alice.media_local_addr().expect("quic local addr");
    assert_ne!(addr_before, addr_after, "the local address did not change");

    let after = stream(&alice, &bob, 6.0, &mut clock).await;
    assert_heard(&after, "wan after migration", 0.7);
    assert!(
        !after
            .alice
            .iter()
            .any(|e| matches!(e, Event::SessionReady(_) | Event::MediaBound)),
        "migration re-bound or re-created the session: {:?}",
        after.alice
    );
    assert_eq!(
        alice.session().map(|s| s.session_id),
        Some(session.session_id)
    );
    assert_eq!(alice.media_path(), Some(MediaPath::Quic));
    let lost_share = after.lost() as f32 / after.frames as f32;
    assert!(
        lost_share < 0.12,
        "lost {:.2} of the frames after the migration (3 % loss link)",
        lost_share
    );

    shaper.clear();
    alice.disconnect();
    bob.disconnect();
}
