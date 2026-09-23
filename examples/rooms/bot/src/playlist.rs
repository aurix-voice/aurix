//! What the bot plays: WAV files listed in `playlist.json` (fetched and normalised by
//! `fetch-assets.sh`) plus built-in test signals that need no assets. Every track yields
//! interleaved f32 PCM at 48 kHz in 20 ms frames.

use std::f32::consts::TAU;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

pub const SAMPLE_RATE: u32 = 48_000;
pub const FRAME_SAMPLES: usize = 960;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    Music,
    Speech,
    Ambience,
    Signal,
}

/// Card the Rooms backend shows next to the bot (`PUT /api/bot/status`).
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TrackInfo {
    pub title: String,
    pub kind: Kind,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub artist: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub license: Option<String>,
    pub duration_ms: u64,
    pub stereo: bool,
}

#[derive(Debug, Deserialize)]
struct PlaylistFile {
    tracks: Vec<TrackEntry>,
}

#[derive(Debug, Deserialize)]
struct TrackEntry {
    title: String,
    kind: Kind,
    #[serde(default)]
    artist: Option<String>,
    #[serde(default)]
    license: Option<String>,
    file: PathBuf,
}

enum Source {
    Pcm { samples: Vec<f32>, channels: u8 },
    Synth(Signal),
}

pub struct Track {
    pub info: TrackInfo,
    source: Source,
}

impl Track {
    pub fn channels(&self) -> u8 {
        match &self.source {
            Source::Pcm { channels, .. } => *channels,
            Source::Synth(s) => s.channels(),
        }
    }

    fn total_frames(&self) -> u64 {
        match &self.source {
            Source::Pcm { samples, channels } => {
                (samples.len() / usize::from(*channels)) as u64 / FRAME_SAMPLES as u64
            }
            Source::Synth(s) => s.duration_ms() * SAMPLE_RATE as u64 / 1000 / FRAME_SAMPLES as u64,
        }
    }

    /// Write frame `index` (20 ms) into `out`; `false` once the track is over.
    pub fn frame(&self, index: u64, out: &mut Vec<f32>) -> bool {
        if index >= self.total_frames() {
            return false;
        }
        out.clear();
        match &self.source {
            Source::Pcm { samples, channels } => {
                let per_frame = FRAME_SAMPLES * usize::from(*channels);
                let start = index as usize * per_frame;
                out.extend_from_slice(&samples[start..start + per_frame]);
            }
            Source::Synth(s) => s.render(index, out),
        }
        true
    }
}

/// Deterministic test signals: what a listener hears tells them what the codec path keeps.
#[derive(Debug, Clone, Copy)]
pub enum Signal {
    /// 1 kHz at −20 dBFS, mono: level reference.
    Tone,
    /// Logarithmic sweep 20 Hz → 20 kHz, mono: shows the coded bandwidth.
    Sweep,
    /// 440 Hz left, then right, then centre, stereo: proves the stereo policy end to end.
    StereoCheck,
    /// Pink-ish noise at −26 dBFS, mono: dense spectrum for artefact spotting.
    PinkNoise,
}

impl Signal {
    pub const ALL: [Signal; 4] = [
        Signal::Tone,
        Signal::Sweep,
        Signal::StereoCheck,
        Signal::PinkNoise,
    ];

    fn title(self) -> &'static str {
        match self {
            Signal::Tone => "Reference tone 1 kHz, −20 dBFS",
            Signal::Sweep => "Sine sweep 20 Hz → 20 kHz",
            Signal::StereoCheck => "Stereo check: left · right · centre",
            Signal::PinkNoise => "Pink noise, −26 dBFS",
        }
    }

    fn duration_ms(self) -> u64 {
        match self {
            Signal::Tone => 6_000,
            Signal::Sweep => 12_000,
            Signal::StereoCheck => 9_000,
            Signal::PinkNoise => 6_000,
        }
    }

    fn channels(self) -> u8 {
        match self {
            Signal::StereoCheck => 2,
            _ => 1,
        }
    }

    fn render(self, frame: u64, out: &mut Vec<f32>) {
        let base = frame * FRAME_SAMPLES as u64;
        let total = self.duration_ms() as f32 / 1000.0;
        for i in 0..FRAME_SAMPLES {
            let n = base + i as u64;
            let t = n as f32 / SAMPLE_RATE as f32;
            // 30 ms fade at both ends keeps the transitions click-free.
            let env = fade(t, total, 0.03);
            match self {
                Signal::Tone => out.push(env * 0.1 * (TAU * 1000.0 * t).sin()),
                Signal::Sweep => {
                    let (f0, f1) = (20.0f32, 20_000.0f32);
                    let k = (f1 / f0).ln() / total;
                    let phase = TAU * f0 * ((k * t).exp() - 1.0) / k;
                    out.push(env * 0.1 * phase.sin());
                }
                Signal::StereoCheck => {
                    let third = total / 3.0;
                    let seg = (t / third).floor().min(2.0);
                    let local = t - seg * third;
                    let s = fade(local, third, 0.03) * 0.15 * (TAU * 440.0 * t).sin();
                    let (l, r) = match seg as u8 {
                        0 => (s, 0.0),
                        1 => (0.0, s),
                        _ => (s * 0.707, s * 0.707),
                    };
                    out.push(l);
                    out.push(r);
                }
                Signal::PinkNoise => out.push(env * 0.05 * pink(n)),
            }
        }
    }
}

fn fade(t: f32, total: f32, ramp: f32) -> f32 {
    (t / ramp).min((total - t) / ramp).clamp(0.0, 1.0)
}

/// Voss–McCartney pink noise from a hash-based white source: stateless per sample so frames
/// can be rendered by index.
fn pink(n: u64) -> f32 {
    let mut sum = 0.0;
    for octave in 0..8u32 {
        let idx = n >> octave;
        sum += white(
            idx.wrapping_mul(0x9E37_79B9_7F4A_7C15)
                .wrapping_add(octave as u64),
        );
    }
    sum / 8.0
}

fn white(mut x: u64) -> f32 {
    x ^= x >> 33;
    x = x.wrapping_mul(0xFF51_AFD7_ED55_8CCD);
    x ^= x >> 33;
    x = x.wrapping_mul(0xC4CE_B9FE_1A85_EC53);
    x ^= x >> 33;
    (x >> 40) as f32 / (1u64 << 24) as f32 * 2.0 - 1.0
}

/// Load `playlist.json` next to its files and interleave one built-in signal after every
/// two file tracks; with no playlist at all the bot plays only the signals.
pub fn load(dir: Option<&Path>) -> Result<Vec<Track>> {
    let mut files = Vec::new();
    if let Some(dir) = dir {
        let manifest = dir.join("playlist.json");
        let raw = fs::read(&manifest).with_context(|| format!("read {}", manifest.display()))?;
        let parsed: PlaylistFile = serde_json::from_slice(&raw)
            .with_context(|| format!("parse {}", manifest.display()))?;
        for entry in parsed.tracks {
            let path = dir.join(&entry.file);
            let (samples, channels) =
                read_wav(&path).with_context(|| format!("load {}", path.display()))?;
            let duration_ms =
                (samples.len() / usize::from(channels)) as u64 * 1000 / SAMPLE_RATE as u64;
            files.push(Track {
                info: TrackInfo {
                    title: entry.title,
                    kind: entry.kind,
                    artist: entry.artist,
                    license: entry.license,
                    duration_ms,
                    stereo: channels == 2,
                },
                source: Source::Pcm { samples, channels },
            });
        }
    }
    let mut signals = Signal::ALL.iter().copied().cycle();
    let mut out = Vec::new();
    let count = files.len();
    for (i, track) in files.into_iter().enumerate() {
        out.push(track);
        if i % 2 == 1 || i + 1 == count {
            if let Some(s) = signals.next() {
                out.push(synth(s));
            }
        }
    }
    if count == 0 {
        out.extend(Signal::ALL.iter().copied().map(synth));
    }
    Ok(out)
}

fn synth(s: Signal) -> Track {
    Track {
        info: TrackInfo {
            title: s.title().to_string(),
            kind: Kind::Signal,
            artist: None,
            license: None,
            duration_ms: s.duration_ms(),
            stereo: s.channels() == 2,
        },
        source: Source::Synth(s),
    }
}

/// 16-bit PCM RIFF/WAVE at 48 kHz, mono or stereo (what `fetch-assets.sh` writes).
fn read_wav(path: &Path) -> Result<(Vec<f32>, u8)> {
    let bytes = fs::read(path)?;
    if bytes.len() < 12 || &bytes[0..4] != b"RIFF" || &bytes[8..12] != b"WAVE" {
        bail!("not a RIFF/WAVE file");
    }
    let u16_at = |i: usize| u16::from_le_bytes([bytes[i], bytes[i + 1]]);
    let u32_at =
        |i: usize| u32::from_le_bytes([bytes[i], bytes[i + 1], bytes[i + 2], bytes[i + 3]]);
    let mut pos = 12;
    let mut fmt: Option<(u16, u16, u32, u16)> = None;
    while pos + 8 <= bytes.len() {
        let id = &bytes[pos..pos + 4];
        let size = u32_at(pos + 4) as usize;
        let body = pos + 8;
        let end = body.saturating_add(size).min(bytes.len());
        match id {
            b"fmt " if end - body >= 16 => {
                let mut tag = u16_at(body);
                if tag == 0xFFFE && end - body >= 26 {
                    tag = u16_at(body + 24);
                }
                fmt = Some((tag, u16_at(body + 2), u32_at(body + 4), u16_at(body + 14)));
            }
            b"data" => {
                let Some((tag, channels, rate, bits)) = fmt else {
                    bail!("data chunk before fmt");
                };
                if tag != 1 || bits != 16 {
                    bail!("expected 16-bit PCM, got format {tag} / {bits} bit");
                }
                if rate != SAMPLE_RATE {
                    bail!("expected {SAMPLE_RATE} Hz, got {rate}");
                }
                if !(1..=2).contains(&channels) {
                    bail!("expected mono or stereo, got {channels} channels");
                }
                let data = &bytes[body..end];
                let samples: Vec<f32> = data
                    .as_chunks::<2>()
                    .0
                    .iter()
                    .map(|c| i16::from_le_bytes(*c) as f32 / 32768.0)
                    .collect();
                return Ok((samples, channels as u8));
            }
            _ => {}
        }
        pos = end + (size & 1);
    }
    bail!("no data chunk")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signals_render_full_length_and_stay_in_range() {
        for s in Signal::ALL {
            let track = synth(s);
            let mut out = Vec::new();
            let mut frames = 0;
            while track.frame(frames, &mut out) {
                assert_eq!(out.len(), FRAME_SAMPLES * usize::from(track.channels()));
                assert!(out.iter().all(|v| v.abs() <= 1.0), "{s:?} clipped");
                frames += 1;
            }
            assert_eq!(frames, s.duration_ms() * 50 / 1000);
        }
    }

    #[test]
    fn stereo_check_puts_energy_where_announced() {
        let track = synth(Signal::StereoCheck);
        let mut out = Vec::new();
        let mut energy = |frame: u64| {
            assert!(track.frame(frame, &mut out));
            out.as_chunks::<2>()
                .0
                .iter()
                .fold((0.0f32, 0.0f32), |(l, r), [a, b]| {
                    (l + a.abs(), r + b.abs())
                })
        };
        let (l, r) = energy(50);
        assert!(l > 1.0 && r == 0.0);
        let (l, r) = energy(200);
        assert!(l == 0.0 && r > 1.0);
        let (l, r) = energy(350);
        assert!(l > 1.0 && (l - r).abs() < 1e-3);
    }

    #[test]
    fn playlist_without_assets_is_the_signal_set() {
        let tracks = load(None).unwrap();
        assert_eq!(tracks.len(), Signal::ALL.len());
        assert!(tracks.iter().all(|t| t.info.kind == Kind::Signal));
    }

    #[test]
    fn reads_16_bit_wav() {
        let dir = std::env::temp_dir().join(format!("aurix-rooms-bot-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let frames = FRAME_SAMPLES * 3;
        let mut wav = Vec::new();
        wav.extend_from_slice(b"RIFF");
        wav.extend_from_slice(&(36 + frames as u32 * 4).to_le_bytes());
        wav.extend_from_slice(b"WAVEfmt ");
        wav.extend_from_slice(&16u32.to_le_bytes());
        wav.extend_from_slice(&1u16.to_le_bytes());
        wav.extend_from_slice(&2u16.to_le_bytes());
        wav.extend_from_slice(&SAMPLE_RATE.to_le_bytes());
        wav.extend_from_slice(&(SAMPLE_RATE * 4).to_le_bytes());
        wav.extend_from_slice(&4u16.to_le_bytes());
        wav.extend_from_slice(&16u16.to_le_bytes());
        wav.extend_from_slice(b"data");
        wav.extend_from_slice(&(frames as u32 * 4).to_le_bytes());
        for _ in 0..frames {
            wav.extend_from_slice(&i16::MAX.to_le_bytes());
            wav.extend_from_slice(&i16::MIN.to_le_bytes());
        }
        let path = dir.join("t.wav");
        fs::write(&path, &wav).unwrap();
        let (samples, channels) = read_wav(&path).unwrap();
        fs::remove_dir_all(&dir).unwrap();
        assert_eq!(channels, 2);
        assert_eq!(samples.len(), frames * 2);
        assert!((samples[0] - 0.99997).abs() < 1e-4 && samples[1] == -1.0);
    }
}
