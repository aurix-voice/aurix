//! Headless peer for cross-implementation E2EE checks: joins an end-to-end encrypted channel,
//! streams a sine tone for a while, mixes whatever it can decrypt and prints what it saw
//! (its own fingerprint, every peer's fingerprint, frames sealed/dropped, playout RMS).
//! Run one of these next to the C# demo (`--scenario e2ee-peer`) or a browser and compare.
//!
//! ```text
//! cargo run -p aurix-client --example e2ee_peer -- ws://127.0.0.1:8081/ws <jwt> <channel-uuid> [seconds] [tone-hz] [identity-hex32]
//! ```

use std::time::{Duration, Instant};

use aurix_client::audio::{FRAME_SAMPLES, SAMPLE_RATE};
use aurix_client::{ChannelId, Client, ClientConfig, DspConfig, Event};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.len() < 3 {
        eprintln!(
            "usage: e2ee_peer <ws-url> <token> <channel-uuid> [seconds=8] [tone-hz=440] [identity-hex32]"
        );
        std::process::exit(2);
    }
    let channel = ChannelId::from_uuid(args[2].parse().expect("channel uuid"));
    let seconds: f32 = args.get(3).map_or(8.0, |s| s.parse().expect("seconds"));
    let tone: f32 = args.get(4).map_or(440.0, |s| s.parse().expect("tone hz"));

    let mut cfg = ClientConfig::new(args[0].clone(), args[1].clone());
    cfg.dsp = DspConfig::BYPASS;
    if let Some(hex) = args.get(5) {
        let bytes: Vec<u8> = (0..hex.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).expect("identity hex"))
            .collect();
        cfg.e2ee_identity = Some(bytes.try_into().expect("identity must be 32 bytes"));
    }
    let client = Client::new(cfg).expect("client");
    println!("fingerprint {}", client.e2ee_fingerprint());
    client.connect().expect("connect");
    wait_for(&client, "media", |e| matches!(e, Event::MediaBound));
    let rid = client.join_channel(channel, None).expect("join");
    let joined = wait_for(&client, "join", |e| {
        matches!(e, Event::ChannelJoined { request_id, .. } if *request_id == rid)
            || matches!(e, Event::RequestFailed { request_id, .. } if *request_id == rid)
    });
    if let Event::RequestFailed { code, message, .. } = joined {
        println!("join refused {code} {message}");
        std::process::exit(1);
    }

    let frames = (seconds * 50.0) as usize;
    let mut phase = 0.0f32;
    let mut pcm = vec![0f32; FRAME_SAMPLES];
    let mut out = vec![0i16; FRAME_SAMPLES * 2];
    let (mut active, mut sum_sq, mut samples) = (0usize, 0f64, 0usize);
    let start = Instant::now();
    for i in 0..frames {
        for s in pcm.iter_mut() {
            *s = 0.3 * phase.sin();
            phase += 2.0 * std::f32::consts::PI * tone / SAMPLE_RATE as f32;
        }
        client.push_capture_f32(&pcm, SAMPLE_RATE, 1);
        if client.mix_output_i16(&mut out, 2) > 0 {
            active += 1;
            for s in &out {
                let v = *s as f64 / 32768.0;
                sum_sq += v * v;
            }
            samples += out.len();
        }
        drain(&client);
        let due = start + Duration::from_millis(20 * (i as u64 + 1));
        if let Some(d) = due.checked_duration_since(Instant::now()) {
            std::thread::sleep(d);
        }
    }
    drain(&client);
    let rms = if samples > 0 {
        (sum_sq / samples as f64).sqrt()
    } else {
        0.0
    };
    let tx = client.stats().transmit;
    println!(
        "heard active_frames={active} rms={rms:.3} frames_sent={} frames_e2ee={} undecryptable={}",
        tx.frames_sent, tx.frames_e2ee, tx.e2ee_undecryptable
    );
    client.leave_channel(channel).ok();
    std::thread::sleep(Duration::from_millis(200));
    drain(&client);
    client.disconnect();
}

fn drain(client: &Client) {
    while let Some(ev) = client.poll_event() {
        report(&ev);
    }
}

fn report(ev: &Event) {
    match ev {
        Event::E2eePeerKey {
            user_id,
            fingerprint,
            previous_fingerprint,
        } => println!(
            "peer {} fingerprint {fingerprint} previous {previous_fingerprint:?}",
            user_id.0
        ),
        Event::E2eePeerDecryptable {
            user_id,
            decryptable,
        } => println!("peer {} decryptable {decryptable}", user_id.0),
        Event::E2eeKeyRotated { generation } => println!("rotated {generation}"),
        Event::ParticipantJoined { participant, .. } => {
            println!("participant joined {}", participant.user_id.0)
        }
        Event::ParticipantLeft { user_id, .. } => println!("participant left {}", user_id.0),
        Event::Disconnected { reason } | Event::FailedToRecover { reason } => {
            println!("connection ended: {reason}");
            std::process::exit(1);
        }
        _ => {}
    }
}

fn wait_for<F: FnMut(&Event) -> bool>(client: &Client, what: &str, mut pred: F) -> Event {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        while let Some(ev) = client.poll_event() {
            report(&ev);
            if pred(&ev) {
                return ev;
            }
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for {what} (state {:?})",
            client.state()
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}
