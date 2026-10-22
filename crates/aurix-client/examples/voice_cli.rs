//! Interactive console voice client for support and QA — reproduce a player's problem without
//! an engine: connect, join, mute, move, inject a WAV, record what you hear, print every event
//! and the stats.
//!
//! ```text
//! cargo run -p aurix-client --example voice_cli -- <ws-url> <token> [channel-uuid] [--json]
//! ```
//!
//! Then type `help`. No audio device is used: `play file.wav [loop]` is the microphone,
//! `record out.wav` captures the 48 kHz stereo mix of everyone else.
use std::collections::VecDeque;
use std::io::{BufRead, Write};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use aurix_client::audio::{FRAME_SAMPLES, SAMPLE_RATE};
use aurix_client::{ChannelId, Client, ClientConfig, Event, UserId};
use aurix_common::protocol::{TransmissionMode, UserPosition};
use aurix_common::types::{Orientation3D, Position3D};

const HELP: &str = "\
commands (ids are UUIDs; `last` = the channel of the last join):
  join <channel> [token]       join a channel (positional ones need `pos` afterwards)
  leave <channel>|all          leave
  mute on|off                  microphone mute (what the server and others see)
  tx all|none|<channel>        transmission target
  focus <channel>|none         channel heard at full volume
  pos <channel> x y z [fx fy fz] send the local 3D pose (default forward +z, up +y)
  vol <user> <0..2>            how loud a participant is for me
  priority on|off [channel]    priority speaker (needs a moderator grant)
  play <file.wav> [loop]       send a 16-bit PCM WAV as the microphone (stop: `play stop`)
  record <out.wav>|stop        write the mixed remote audio to a 48 kHz stereo WAV
  who [channel]                roster with mute / speaking / priority / no-mic flags
  stats                        transport, loss, jitter, MOS, per-stream counters
  session                      session info (node, media path, endpoint)
  events on|off                print events as they arrive (default on)
  reconnect                    force a reconnect (network change simulation)
  quit";

#[derive(Default)]
struct Shared {
    play: Option<Wav>,
    play_pos: usize,
    play_loop: bool,
    record: Option<(std::fs::File, u32)>,
    pending: VecDeque<String>,
}

struct Wav {
    rate: u32,
    channels: u8,
    pcm: Vec<f32>,
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.len() < 2 {
        eprintln!("usage: voice_cli <ws-url> <token> [channel-uuid] [--json]");
        std::process::exit(2);
    }
    let json = args.iter().any(|a| a == "--json");
    let cfg = ClientConfig::new(args[0].clone(), args[1].clone());
    let client = Arc::new(Client::new(cfg).expect("client"));
    let shared = Arc::new(Mutex::new(Shared::default()));
    let print_events = Arc::new(std::sync::atomic::AtomicBool::new(true));

    // Audio clock: 50 Hz, pushes the WAV "microphone" and pulls the mix for the recorder.
    {
        let client = client.clone();
        let shared = shared.clone();
        std::thread::spawn(move || {
            let mut next = Instant::now();
            let mut mix = vec![0f32; FRAME_SAMPLES * 2];
            loop {
                next += Duration::from_millis(20);
                let mut s = shared.lock().unwrap();
                if let Some(w) = &s.play {
                    let frame = (w.rate as usize / 50) * w.channels as usize;
                    let end = (s.play_pos + frame).min(w.pcm.len());
                    let chunk = &w.pcm[s.play_pos..end];
                    client.push_capture_f32(chunk, w.rate, w.channels);
                    let finished = end >= w.pcm.len();
                    s.play_pos = end;
                    if finished {
                        if s.play_loop {
                            s.play_pos = 0;
                        } else {
                            s.play = None;
                            s.pending.push_back("play: finished".into());
                        }
                    }
                }
                if s.record.is_some() {
                    mix.iter_mut().for_each(|x| *x = 0.0);
                    client.mix_output_f32(&mut mix, 2);
                    let (file, frames) = s.record.as_mut().unwrap();
                    let bytes: Vec<u8> = mix
                        .iter()
                        .flat_map(|x| ((x.clamp(-1.0, 1.0) * 32767.0) as i16).to_le_bytes())
                        .collect();
                    let _ = file.write_all(&bytes);
                    *frames += FRAME_SAMPLES as u32;
                }
                drop(s);
                let now = Instant::now();
                if next > now {
                    std::thread::sleep(next - now);
                }
            }
        });
    }
    // Event printer.
    {
        let client = client.clone();
        let shared = shared.clone();
        let print_events = print_events.clone();
        std::thread::spawn(move || loop {
            if let Some(ev) = client.wait_event(Duration::from_millis(250)) {
                if print_events.load(std::sync::atomic::Ordering::Relaxed) {
                    let line = if json {
                        serde_json::to_string(&ev).unwrap_or_else(|_| format!("{ev:?}"))
                    } else {
                        describe(&ev)
                    };
                    println!("\x1b[2m[event]\x1b[0m {line}");
                }
            }
            let mut s = shared.lock().unwrap();
            while let Some(m) = s.pending.pop_front() {
                println!("\x1b[2m[audio]\x1b[0m {m}");
            }
        });
    }

    client.connect().expect("connect");
    let mut last: Option<ChannelId> = args
        .get(2)
        .filter(|a| !a.starts_with("--"))
        .and_then(|a| a.parse().ok())
        .map(ChannelId::from_uuid);
    if let Some(ch) = last {
        // Join once media is up; the event printer shows the outcome.
        let client = client.clone();
        std::thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(10);
            while Instant::now() < deadline {
                if client.state() == aurix_client::ConnectionState::MediaBound {
                    let _ = client.join_channel(ch, None);
                    return;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            eprintln!("media not bound after 10 s; type `join` yourself");
        });
    }
    println!("connected to {} — type `help`", args[0]);

    let stdin = std::io::stdin();
    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        let parts: Vec<&str> = line.split_whitespace().collect();
        let Some(&cmd) = parts.first() else { continue };
        let ch = |i: usize| -> Option<ChannelId> {
            match parts.get(i) {
                Some(&"last") | None => last,
                Some(s) => s.parse().ok().map(ChannelId::from_uuid),
            }
        };
        let r: Result<String, String> = match cmd {
            "help" | "?" => Ok(HELP.into()),
            "quit" | "exit" => break,
            "join" => match ch(1) {
                Some(c) => {
                    last = Some(c);
                    client
                        .join_channel(c, parts.get(2).copied())
                        .map(|rid| format!("join requested (request {rid:?})"))
                        .map_err(|e| e.to_string())
                }
                None => Err("join <channel-uuid> [token]".into()),
            },
            "leave" => {
                if parts.get(1) == Some(&"all") {
                    for c in client.joined_channels() {
                        let _ = client.leave_channel(c);
                    }
                    Ok("left all".into())
                } else {
                    ch(1)
                        .ok_or("leave <channel>|all".to_string())
                        .and_then(|c| {
                            client
                                .leave_channel(c)
                                .map(|_| "left".into())
                                .map_err(|e| e.to_string())
                        })
                }
            }
            "mute" => {
                let on = parts.get(1) == Some(&"on");
                client.set_muted(on);
                Ok(format!("muted = {on}"))
            }
            "tx" => {
                let mode = match parts.get(1) {
                    Some(&"all") => TransmissionMode::All,
                    Some(&"none") => TransmissionMode::None,
                    _ => match ch(1) {
                        Some(c) => TransmissionMode::Single { channel_id: c },
                        None => return_err("tx all|none|<channel>"),
                    },
                };
                client
                    .set_transmission(mode)
                    .map(|_| "ok".into())
                    .map_err(|e| e.to_string())
            }
            "focus" => {
                let target = if parts.get(1) == Some(&"none") {
                    None
                } else {
                    ch(1)
                };
                client
                    .set_channel_focus(target)
                    .map(|_| "ok".into())
                    .map_err(|e| e.to_string())
            }
            "pos" => {
                let f = |i: usize, d: f32| parts.get(i).and_then(|s| s.parse().ok()).unwrap_or(d);
                match (ch(1), client.user_id()) {
                    (Some(c), Some(me)) if parts.len() >= 5 => client
                        .update_positions(
                            c,
                            vec![UserPosition {
                                user_id: me,
                                position: Position3D {
                                    x: f(2, 0.0),
                                    y: f(3, 0.0),
                                    z: f(4, 0.0),
                                },
                                orientation: Orientation3D {
                                    forward_x: f(5, 0.0),
                                    forward_y: f(6, 0.0),
                                    forward_z: f(7, 1.0),
                                    up_x: 0.0,
                                    up_y: 1.0,
                                    up_z: 0.0,
                                },
                            }],
                        )
                        .map(|_| "pose sent".into())
                        .map_err(|e| e.to_string()),
                    (_, None) => Err("no session yet".into()),
                    _ => Err("pos <channel> x y z [fx fy fz]".into()),
                }
            }
            "vol" => match (
                parts.get(1).and_then(|s| s.parse().ok()),
                parts.get(2).and_then(|s| s.parse().ok()),
            ) {
                (Some(u), Some(v)) => client
                    .set_participant_volume(UserId::from_uuid(u), v)
                    .map(|_| "ok".into())
                    .map_err(|e| e.to_string()),
                _ => Err("vol <user-uuid> <0..2>".into()),
            },
            "priority" => match ch(2) {
                Some(c) => client
                    .set_priority(c, None, parts.get(1) == Some(&"on"))
                    .map(|_| "ok".into())
                    .map_err(|e| e.to_string()),
                None => Err("priority on|off [channel]".into()),
            },
            "play" => match parts.get(1) {
                Some(&"stop") => {
                    shared.lock().unwrap().play = None;
                    Ok("stopped".into())
                }
                Some(path) => match read_wav(path) {
                    Ok(w) => {
                        let secs = w.pcm.len() as f32 / (w.rate as f32 * w.channels as f32);
                        let mut s = shared.lock().unwrap();
                        s.play_loop = parts.get(2) == Some(&"loop");
                        s.play_pos = 0;
                        s.play = Some(w);
                        Ok(format!(
                            "playing {secs:.1} s{}",
                            if s.play_loop { " (loop)" } else { "" }
                        ))
                    }
                    Err(e) => Err(e),
                },
                None => Err("play <file.wav> [loop]|stop".into()),
            },
            "record" => match parts.get(1) {
                Some(&"stop") | None => finish_record(&shared),
                Some(path) => match std::fs::File::create(path) {
                    Ok(mut f) => {
                        let _ = f.write_all(&wav_header(0));
                        let _ = finish_record(&shared);
                        shared.lock().unwrap().record = Some((f, 0));
                        Ok(format!("recording to {path}"))
                    }
                    Err(e) => Err(e.to_string()),
                },
            },
            "who" => {
                let chans = match ch(1) {
                    Some(c) if parts.len() > 1 => vec![c],
                    _ => client.joined_channels(),
                };
                let mut out = String::new();
                for c in chans {
                    out += &format!("channel {}\n", c.0);
                    for p in client.participants(c) {
                        out += &format!(
                            "  {} {:<20} {:?}{}{}{}{}\n",
                            p.user_id.0,
                            p.display_name,
                            p.role,
                            if p.muted { " muted" } else { "" },
                            if p.speaking { " speaking" } else { "" },
                            if p.priority { " priority" } else { "" },
                            if p.capture_unavailable { " no-mic" } else { "" }
                        );
                    }
                }
                Ok(out.trim_end().to_string())
            }
            "stats" => Ok(format!("{:#?}", client.stats())),
            "session" => Ok(format!("{:#?}", client.session())),
            "events" => {
                print_events.store(
                    parts.get(1) != Some(&"off"),
                    std::sync::atomic::Ordering::Relaxed,
                );
                Ok("ok".into())
            }
            "reconnect" => {
                let _ = client.network_changed();
                Ok("reconnecting".into())
            }
            _ => Err(format!("unknown command `{cmd}` — type help")),
        };
        match r {
            Ok(m) => println!("{m}"),
            Err(e) => println!("\x1b[31merror:\x1b[0m {e}"),
        }
    }
    let _ = finish_record(&shared);
    client.disconnect();
}

fn return_err(msg: &str) -> ! {
    println!("\x1b[31merror:\x1b[0m {msg}");
    std::process::exit(2)
}

fn describe(ev: &Event) -> String {
    match ev {
        Event::ParticipantLeft {
            channel_id,
            user_id,
            reason,
        } => {
            format!(
                "ParticipantLeft {} {} reason={}",
                channel_id.0,
                user_id.0,
                reason.as_str()
            )
        }
        Event::RequestFailed { code, message, .. } => format!("RequestFailed {code}: {message}"),
        Event::ServerError { code, message, .. } => format!("ServerError {code}: {message}"),
        Event::Recovering {
            attempt,
            delay,
            cause,
            deadline_in,
        } => {
            format!("Recovering attempt={attempt} delay={delay:?} deadline_in={deadline_in:?} cause={cause}")
        }
        other => format!("{other:?}"),
    }
}

fn finish_record(shared: &Arc<Mutex<Shared>>) -> Result<String, String> {
    let mut s = shared.lock().unwrap();
    let Some((mut f, frames)) = s.record.take() else {
        return Ok("not recording".into());
    };
    use std::io::Seek;
    f.seek(std::io::SeekFrom::Start(0))
        .map_err(|e| e.to_string())?;
    f.write_all(&wav_header(frames))
        .map_err(|e| e.to_string())?;
    Ok(format!(
        "recorded {:.1} s",
        frames as f32 / SAMPLE_RATE as f32
    ))
}

/// 44-byte PCM header: 48 kHz, stereo, 16-bit.
fn wav_header(frames: u32) -> Vec<u8> {
    let data = frames * 4;
    let mut h = Vec::with_capacity(44);
    h.extend_from_slice(b"RIFF");
    h.extend_from_slice(&(36 + data).to_le_bytes());
    h.extend_from_slice(b"WAVEfmt ");
    h.extend_from_slice(&16u32.to_le_bytes());
    h.extend_from_slice(&1u16.to_le_bytes());
    h.extend_from_slice(&2u16.to_le_bytes());
    h.extend_from_slice(&SAMPLE_RATE.to_le_bytes());
    h.extend_from_slice(&(SAMPLE_RATE * 4).to_le_bytes());
    h.extend_from_slice(&4u16.to_le_bytes());
    h.extend_from_slice(&16u16.to_le_bytes());
    h.extend_from_slice(b"data");
    h.extend_from_slice(&data.to_le_bytes());
    h
}

/// Minimal 16-bit PCM WAV reader (RIFF chunks walked; other formats rejected with a message).
fn read_wav(path: &str) -> Result<Wav, String> {
    let b = std::fs::read(path).map_err(|e| format!("{path}: {e}"))?;
    if b.len() < 12 || &b[0..4] != b"RIFF" || &b[8..12] != b"WAVE" {
        return Err(format!("{path}: not a RIFF/WAVE file"));
    }
    let (mut rate, mut channels, mut bits, mut fmt) = (0u32, 0u16, 0u16, 0u16);
    let mut data: Option<&[u8]> = None;
    let mut i = 12;
    while i + 8 <= b.len() {
        let id = &b[i..i + 4];
        let len = u32::from_le_bytes(b[i + 4..i + 8].try_into().unwrap()) as usize;
        let body = &b[i + 8..(i + 8 + len).min(b.len())];
        if id == b"fmt " && body.len() >= 16 {
            fmt = u16::from_le_bytes(body[0..2].try_into().unwrap());
            channels = u16::from_le_bytes(body[2..4].try_into().unwrap());
            rate = u32::from_le_bytes(body[4..8].try_into().unwrap());
            bits = u16::from_le_bytes(body[14..16].try_into().unwrap());
        } else if id == b"data" {
            data = Some(body);
        }
        i += 8 + len + (len & 1);
    }
    let data = data.ok_or_else(|| format!("{path}: no data chunk"))?;
    if fmt != 1 || bits != 16 || !(1..=2).contains(&channels) || rate == 0 {
        return Err(format!(
            "{path}: need 16-bit PCM mono/stereo (got format {fmt}, {bits} bit, {channels} ch)"
        ));
    }
    let pcm = (0..data.len() / 2)
        .map(|k| i16::from_le_bytes([data[2 * k], data[2 * k + 1]]) as f32 / 32768.0)
        .collect();
    Ok(Wav {
        rate,
        channels: channels as u8,
        pcm,
    })
}
