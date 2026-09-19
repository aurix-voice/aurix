//! Audio building blocks shared by every native integration: level metering + VAD, capture
//! framing (any PCM format → 20 ms mono 48 kHz Opus frames) and the receive side (per-sender
//! jitter buffer + Opus decoder, mixed into one interleaved output with per-participant gain and
//! constant-power panning). Mirrors `sdk/unity/Runtime/Audio` so all clients sound the same.

use aurix_common::protocol::{decode_audio_level, encode_audio_level, AUDIO_LEVEL_SILENCE};
use aurix_common::types::Direction;
use std::collections::{BTreeMap, HashMap};
use std::time::{Duration, Instant};

/// Everything on the wire is Opus at 48 kHz.
pub const SAMPLE_RATE: u32 = 48_000;
/// One packet carries 20 ms of audio.
pub const FRAME_SAMPLES: usize = 960;
/// Largest software input gain (+12 dB).
pub const MAX_INPUT_GAIN: f32 = 4.0;
/// Largest master output volume (+6 dB).
pub const MAX_OUTPUT_VOLUME: f32 = 2.0;
/// Decoders accept frames up to 60 ms.
const MAX_DECODE_SAMPLES: usize = FRAME_SAMPLES * 3;
const STREAM_IDLE_TIMEOUT: Duration = Duration::from_secs(30);

/// RMS of a PCM float buffer, `0..=1`.
pub fn rms(pcm: &[f32]) -> f32 {
    if pcm.is_empty() {
        return 0.0;
    }
    let sum: f64 = pcm.iter().map(|&s| (s as f64) * (s as f64)).sum();
    (sum / pcm.len() as f64).sqrt() as f32
}

/// Clamp a gain into `0..=max`; NaN means "unity".
pub fn clamp_gain(gain: f32, max: f32) -> f32 {
    if gain.is_nan() {
        1.0
    } else {
        gain.clamp(0.0, max)
    }
}

/// Frame-by-frame level meter with an energy VAD: speech starts as soon as one frame crosses
/// `threshold` and ends once `hangover_frames` consecutive frames stay below it.
#[derive(Debug, Clone)]
pub struct VoiceActivityDetector {
    /// Linear RMS threshold (0.01 ≈ -40 dBov).
    pub threshold: f32,
    /// Quiet 20 ms frames before speech is considered over (15 = 300 ms).
    pub hangover_frames: u32,
    /// Smoothing factor for [`Self::energy`] (0 = raw, 1 = frozen).
    pub smoothing: f32,
    quiet: u32,
    energy: f32,
    level: u8,
    speaking: bool,
}

impl Default for VoiceActivityDetector {
    fn default() -> Self {
        Self {
            threshold: 0.01,
            hangover_frames: 15,
            smoothing: 0.5,
            quiet: 0,
            energy: 0.0,
            level: AUDIO_LEVEL_SILENCE,
            speaking: false,
        }
    }
}

impl VoiceActivityDetector {
    /// Smoothed linear energy of the most recent frames.
    pub fn energy(&self) -> f32 {
        self.energy
    }

    /// Wire level (RFC 6464 style, 127 = silence) of the most recent frame.
    pub fn level(&self) -> u8 {
        self.level
    }

    pub fn speaking(&self) -> bool {
        self.speaking
    }

    /// Feed one frame; returns `true` when the speaking state flipped.
    pub fn process(&mut self, pcm: &[f32]) -> bool {
        let rms = rms(pcm);
        self.level = encode_audio_level(rms);
        let s = self.smoothing.clamp(0.0, 1.0);
        self.energy = self.energy * s + rms * (1.0 - s);
        let was = self.speaking;
        if rms >= self.threshold {
            self.quiet = 0;
            self.speaking = true;
        } else if self.speaking {
            self.quiet += 1;
            if self.quiet >= self.hangover_frames.max(1) {
                self.speaking = false;
            }
        }
        was != self.speaking
    }

    pub fn reset(&mut self) {
        self.quiet = 0;
        self.energy = 0.0;
        self.level = AUDIO_LEVEL_SILENCE;
        self.speaking = false;
    }
}

/// Linear-interpolating mono resampler that keeps its fractional position between calls so
/// consecutive buffers join without clicks. Voice-grade; engines that already run at 48 kHz
/// never hit it.
#[derive(Debug)]
struct Resampler {
    from: u32,
    to: u32,
    pos: f64,
    last: f32,
}

impl Resampler {
    fn new(from: u32, to: u32) -> Self {
        Self {
            from,
            to,
            pos: 0.0,
            last: 0.0,
        }
    }

    fn push(&mut self, input: &[f32], out: &mut Vec<f32>) {
        if self.from == self.to {
            out.extend_from_slice(input);
            return;
        }
        if input.is_empty() {
            return;
        }
        let step = self.from as f64 / self.to as f64;
        // Virtual index -1 is the last sample of the previous buffer.
        let sample = |i: i64| -> f32 {
            if i < 0 {
                self.last
            } else {
                input[(i as usize).min(input.len() - 1)]
            }
        };
        let mut pos = self.pos - 1.0;
        let end = input.len() as f64 - 1.0;
        while pos <= end {
            let i = pos.floor();
            let frac = (pos - i) as f32;
            let a = sample(i as i64);
            let b = sample(i as i64 + 1);
            out.push(a + (b - a) * frac);
            pos += step;
        }
        self.pos = pos - end;
        self.last = input[input.len() - 1];
    }
}

/// One encoded uplink frame.
#[derive(Debug, Clone)]
pub struct EncodedFrame {
    pub opus: Vec<u8>,
    /// Wire audio level of the frame before encoding.
    pub level: u8,
    /// RMS of the frame, `0..=1`.
    pub energy: f32,
    /// The VAD considers this frame speech.
    pub speech: bool,
}

/// Turns arbitrary captured PCM (any rate, 1..=8 interleaved channels, i16 or f32) into
/// 20 ms mono 48 kHz Opus frames with input gain and VAD metering applied.
pub struct CaptureEncoder {
    encoder: opus::Encoder,
    resampler: Option<Resampler>,
    mono: Vec<f32>,
    pending: Vec<f32>,
    gain: f32,
    pub vad: VoiceActivityDetector,
    bitrate_bps: u32,
    out: [u8; 1275],
}

impl CaptureEncoder {
    pub fn new(bitrate_bps: u32) -> Result<Self, opus::Error> {
        let mut encoder =
            opus::Encoder::new(SAMPLE_RATE, opus::Channels::Mono, opus::Application::Voip)?;
        let bitrate = bitrate_bps.clamp(6_000, 128_000);
        encoder.set_bitrate(opus::Bitrate::Bits(bitrate as i32))?;
        encoder.set_inband_fec(true)?;
        encoder.set_packet_loss_perc(5)?;
        Ok(Self {
            encoder,
            resampler: None,
            mono: Vec::with_capacity(FRAME_SAMPLES * 4),
            pending: Vec::with_capacity(FRAME_SAMPLES * 4),
            gain: 1.0,
            vad: VoiceActivityDetector::default(),
            bitrate_bps: bitrate,
            out: [0u8; 1275],
        })
    }

    pub fn bitrate(&self) -> u32 {
        self.bitrate_bps
    }

    pub fn set_bitrate(&mut self, bitrate_bps: u32) -> Result<(), opus::Error> {
        let bitrate = bitrate_bps.clamp(6_000, 128_000);
        self.encoder
            .set_bitrate(opus::Bitrate::Bits(bitrate as i32))?;
        self.bitrate_bps = bitrate;
        Ok(())
    }

    /// Software input gain, clamped to `0..=MAX_INPUT_GAIN`.
    pub fn set_gain(&mut self, gain: f32) {
        self.gain = clamp_gain(gain, MAX_INPUT_GAIN);
    }

    pub fn gain(&self) -> f32 {
        self.gain
    }

    /// Feed interleaved f32 PCM. Every complete 20 ms frame is encoded and handed to `sink`.
    pub fn push_f32(
        &mut self,
        pcm: &[f32],
        sample_rate: u32,
        channels: u8,
        mut sink: impl FnMut(EncodedFrame),
    ) {
        let channels = channels.clamp(1, 8) as usize;
        if pcm.is_empty() || sample_rate == 0 {
            return;
        }
        self.mono.clear();
        for frame in pcm.chunks_exact(channels) {
            let sum: f32 = frame.iter().sum();
            self.mono.push(sum / channels as f32);
        }
        match &mut self.resampler {
            Some(r) if r.from == sample_rate => {}
            _ => self.resampler = Some(Resampler::new(sample_rate, SAMPLE_RATE)),
        }
        let mono = std::mem::take(&mut self.mono);
        if let Some(r) = self.resampler.as_mut() {
            r.push(&mono, &mut self.pending);
        }
        self.mono = mono;
        let mut offset = 0;
        while self.pending.len() - offset >= FRAME_SAMPLES {
            let frame = &mut self.pending[offset..offset + FRAME_SAMPLES];
            if self.gain != 1.0 {
                for s in frame.iter_mut() {
                    *s = (*s * self.gain).clamp(-1.0, 1.0);
                }
            }
            self.vad.process(frame);
            let energy = rms(frame);
            match self.encoder.encode_float(frame, &mut self.out) {
                Ok(n) if n > 0 => sink(EncodedFrame {
                    opus: self.out[..n].to_vec(),
                    level: self.vad.level(),
                    energy,
                    speech: self.vad.speaking(),
                }),
                _ => {}
            }
            offset += FRAME_SAMPLES;
        }
        self.pending.drain(..offset);
    }

    /// Feed interleaved i16 PCM (see [`Self::push_f32`]).
    pub fn push_i16(
        &mut self,
        pcm: &[i16],
        sample_rate: u32,
        channels: u8,
        sink: impl FnMut(EncodedFrame),
    ) {
        let f: Vec<f32> = pcm.iter().map(|&s| s as f32 / 32768.0).collect();
        self.push_f32(&f, sample_rate, channels, sink);
    }

    /// Drop buffered samples (device switch, unmute after a long pause).
    pub fn reset(&mut self) {
        self.pending.clear();
        self.resampler = None;
        self.vad.reset();
        let _ = self.encoder.reset_state();
    }
}

/// Per-sender jitter buffer: reorders by sequence, absorbs jitter with a small target depth and
/// hands frames (or loss markers for PLC) to the decoder at a steady 20 ms cadence.
#[derive(Debug)]
pub struct JitterBuffer {
    frames: BTreeMap<u32, Vec<u8>>,
    target_depth: usize,
    max_depth: usize,
    next_seq: u32,
    started: bool,
    pub lost: u64,
    pub late: u64,
}

#[derive(Debug, PartialEq)]
pub enum JitterSlot {
    /// Nothing to play yet (still filling or starved).
    Wait,
    Frame(Vec<u8>),
    /// The packet for this slot is missing; run PLC.
    Lost,
}

impl JitterBuffer {
    pub fn new(target_depth: usize, max_depth: usize) -> Self {
        let target_depth = target_depth.max(1);
        Self {
            frames: BTreeMap::new(),
            target_depth,
            max_depth: max_depth.max(target_depth + 1),
            next_seq: 0,
            started: false,
            lost: 0,
            late: 0,
        }
    }

    pub fn len(&self) -> usize {
        self.frames.len()
    }

    pub fn is_empty(&self) -> bool {
        self.frames.is_empty()
    }

    fn seq_before(a: u32, b: u32) -> bool {
        (a.wrapping_sub(b) as i32) < 0
    }

    pub fn push(&mut self, seq: u32, opus: Vec<u8>) {
        if self.started && Self::seq_before(seq, self.next_seq) {
            self.late += 1;
            return;
        }
        self.frames.insert(seq, opus);
        if self.frames.len() > self.max_depth {
            while self.frames.len() > self.target_depth {
                self.frames.pop_first();
            }
            if let Some(&first) = self.frames.keys().next() {
                self.next_seq = first;
            }
        }
    }

    pub fn pop(&mut self) -> JitterSlot {
        let Some(&first) = self.frames.keys().next() else {
            return JitterSlot::Wait;
        };
        if !self.started {
            if self.frames.len() < self.target_depth {
                return JitterSlot::Wait;
            }
            self.next_seq = first;
            self.started = true;
        }
        if let Some(f) = self.frames.remove(&self.next_seq) {
            self.next_seq = self.next_seq.wrapping_add(1);
            return JitterSlot::Frame(f);
        }
        if Self::seq_before(self.next_seq, first) {
            self.lost += 1;
            self.next_seq = self.next_seq.wrapping_add(1);
            return JitterSlot::Lost;
        }
        JitterSlot::Wait
    }

    pub fn reset(&mut self) {
        self.frames.clear();
        self.started = false;
    }
}

struct Stream {
    jitter: JitterBuffer,
    decoder: opus::Decoder,
    volume: f32,
    left: f32,
    right: f32,
    frame: Vec<f32>,
    pos: usize,
    len: usize,
    last_activity: Instant,
    starved: bool,
}

/// Statistics of one decoded sender stream.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct StreamStats {
    pub ssrc: u32,
    pub buffered_frames: usize,
    pub lost: u64,
    pub late: u64,
}

/// Decodes and mixes every remote sender into one interleaved float buffer. Each sender has
/// its own decoder because Opus decoder state is per-stream. Safe to drive from the audio
/// thread: pushes only touch a `BTreeMap` behind the caller's lock.
pub struct RemoteMixer {
    streams: HashMap<u32, Stream>,
    output_volume: f32,
    output_muted: bool,
    target_depth: usize,
    max_depth: usize,
    scratch: Vec<f32>,
}

impl RemoteMixer {
    pub fn new(target_depth: usize, max_depth: usize) -> Self {
        Self {
            streams: HashMap::new(),
            output_volume: 1.0,
            output_muted: false,
            target_depth,
            max_depth,
            scratch: Vec::new(),
        }
    }

    /// Master volume on top of per-participant gains, `0..=MAX_OUTPUT_VOLUME`.
    pub fn set_output_volume(&mut self, volume: f32) {
        self.output_volume = clamp_gain(volume, MAX_OUTPUT_VOLUME);
    }

    pub fn output_volume(&self) -> f32 {
        self.output_volume
    }

    /// Speaker mute: streams keep being decoded (jitter buffers stay in sync, unmute is
    /// instant) but nothing reaches the output.
    pub fn set_output_muted(&mut self, muted: bool) {
        self.output_muted = muted;
    }

    pub fn output_muted(&self) -> bool {
        self.output_muted
    }

    /// Queue one verified downlink frame. `volume` is the server-applied gain byte and
    /// `direction` the speaker's bearing relative to this listener (`None` = centred).
    pub fn push(
        &mut self,
        ssrc: u32,
        seq: u32,
        volume: f32,
        direction: Option<Direction>,
        opus: Vec<u8>,
    ) -> Result<(), opus::Error> {
        let stream = match self.streams.get_mut(&ssrc) {
            Some(s) => s,
            None => {
                let decoder = opus::Decoder::new(SAMPLE_RATE, opus::Channels::Mono)?;
                self.streams.entry(ssrc).or_insert(Stream {
                    jitter: JitterBuffer::new(self.target_depth, self.max_depth),
                    decoder,
                    volume: 1.0,
                    left: 1.0,
                    right: 1.0,
                    frame: vec![0.0; MAX_DECODE_SAMPLES],
                    pos: 0,
                    len: 0,
                    last_activity: Instant::now(),
                    starved: false,
                })
            }
        };
        stream.volume = volume;
        match direction {
            Some(d) => (stream.left, stream.right) = d.stereo_gains(),
            None => (stream.left, stream.right) = (1.0, 1.0),
        }
        stream.last_activity = Instant::now();
        if stream.starved {
            // A talk spurt after silence: refill the target depth before playing again.
            stream.jitter.reset();
            stream.starved = false;
        }
        stream.jitter.push(seq, opus);
        Ok(())
    }

    pub fn remove(&mut self, ssrc: u32) {
        self.streams.remove(&ssrc);
    }

    pub fn clear(&mut self) {
        self.streams.clear();
    }

    pub fn stream_stats(&self) -> Vec<StreamStats> {
        self.streams
            .iter()
            .map(|(&ssrc, s)| StreamStats {
                ssrc,
                buffered_frames: s.jitter.len(),
                lost: s.jitter.lost,
                late: s.jitter.late,
            })
            .collect()
    }

    /// Mix into `output` (interleaved, `channels` wide; **adds** to its contents). Mono
    /// streams with a direction are panned between the first two channels. Returns the
    /// number of streams that contributed audio.
    pub fn mix(&mut self, output: &mut [f32], channels: u8) -> usize {
        let channels = channels.clamp(1, 8) as usize;
        let frames_needed = output.len() / channels;
        let master = if self.output_muted {
            0.0
        } else {
            self.output_volume
        };
        let now = Instant::now();
        let mut active = 0;
        self.streams
            .retain(|_, s| now.duration_since(s.last_activity) < STREAM_IDLE_TIMEOUT);
        for s in self.streams.values_mut() {
            let mut written = 0;
            let mut contributed = false;
            while written < frames_needed {
                if s.pos >= s.len {
                    let slot = s.jitter.pop();
                    let n = match slot {
                        JitterSlot::Wait => {
                            if s.jitter.is_empty() {
                                s.starved = true;
                            }
                            break;
                        }
                        JitterSlot::Frame(opus) => {
                            s.decoder.decode_float(&opus, &mut s.frame, false)
                        }
                        JitterSlot::Lost => {
                            s.decoder
                                .decode_float(&[], &mut s.frame[..FRAME_SAMPLES], false)
                        }
                    };
                    s.len = n.unwrap_or(0);
                    s.pos = 0;
                    if s.len == 0 {
                        break;
                    }
                }
                let take = (s.len - s.pos).min(frames_needed - written);
                let gain = s.volume * master;
                if gain != 0.0 {
                    contributed = true;
                    let pan = channels >= 2;
                    for f in 0..take {
                        let sample = s.frame[s.pos + f];
                        let base = (written + f) * channels;
                        for c in 0..channels {
                            let g = if pan {
                                match c {
                                    0 => gain * s.left,
                                    1 => gain * s.right,
                                    _ => gain,
                                }
                            } else {
                                gain
                            };
                            output[base + c] += sample * g;
                        }
                    }
                }
                s.pos += take;
                written += take;
            }
            if contributed {
                active += 1;
            }
        }
        for v in output.iter_mut() {
            *v = v.clamp(-1.0, 1.0);
        }
        active
    }

    /// [`Self::mix`] into an i16 buffer (overwrites `output`).
    pub fn mix_i16(&mut self, output: &mut [i16], channels: u8) -> usize {
        self.scratch.clear();
        self.scratch.resize(output.len(), 0.0);
        let mut scratch = std::mem::take(&mut self.scratch);
        let active = self.mix(&mut scratch, channels);
        for (o, s) in output.iter_mut().zip(scratch.iter()) {
            *o = (s * 32767.0).round().clamp(-32768.0, 32767.0) as i16;
        }
        self.scratch = scratch;
        active
    }
}

/// Linear energy `0..=1` of a wire level byte.
pub fn level_to_energy(level: u8) -> f32 {
    decode_audio_level(level)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sine(len: usize, rate: u32, hz: f32, amp: f32) -> Vec<f32> {
        (0..len)
            .map(|i| (i as f32 * hz * std::f32::consts::TAU / rate as f32).sin() * amp)
            .collect()
    }

    #[test]
    fn vad_flips_with_hangover() {
        let mut vad = VoiceActivityDetector {
            hangover_frames: 2,
            ..Default::default()
        };
        let loud = vec![0.5f32; FRAME_SAMPLES];
        let quiet = vec![0.0f32; FRAME_SAMPLES];
        assert!(vad.process(&loud));
        assert!(vad.speaking());
        assert!(!vad.process(&quiet));
        assert!(vad.process(&quiet));
        assert!(!vad.speaking());
        assert_eq!(vad.level(), AUDIO_LEVEL_SILENCE);
    }

    #[test]
    fn resampler_preserves_duration_and_tone() {
        let mut r = Resampler::new(16_000, 48_000);
        let input = sine(16_000, 16_000, 440.0, 0.5);
        let mut out = Vec::new();
        for chunk in input.chunks(320) {
            r.push(chunk, &mut out);
        }
        assert!((out.len() as i64 - 48_000).abs() <= 3, "{}", out.len());
        let e = rms(&out[4800..]);
        assert!((e - 0.3535).abs() < 0.02, "{e}");
    }

    #[test]
    fn capture_encoder_frames_and_mixer_reproduces_tone() {
        let mut enc = CaptureEncoder::new(32_000).unwrap();
        let pcm = sine(44_100, 44_100, 440.0, 0.5);
        // Stereo, 44.1 kHz, chunked like a typical device callback.
        let stereo: Vec<f32> = pcm.iter().flat_map(|&s| [s, s]).collect();
        let mut frames = Vec::new();
        for chunk in stereo.chunks(2 * 441) {
            enc.push_f32(chunk, 44_100, 2, |f| frames.push(f));
        }
        assert!((frames.len() as i64 - 50).abs() <= 1, "{}", frames.len());
        assert!(frames.iter().all(|f| f.speech && f.level < 20));

        let mut mixer = RemoteMixer::new(2, 12);
        // Paced like a real downlink: one frame in, one 20 ms mix out.
        let mut out = vec![0i16; FRAME_SAMPLES * 2 * frames.len()];
        let mut active = 0;
        for (i, f) in frames.iter().enumerate() {
            mixer.push(7, i as u32, 1.0, None, f.opus.clone()).unwrap();
            let slice = &mut out[i * FRAME_SAMPLES * 2..(i + 1) * FRAME_SAMPLES * 2];
            active += mixer.mix_i16(slice, 2);
        }
        // Playout starts once the target depth (2 frames) is filled.
        assert!(active >= frames.len() - 3, "{active}");
        let tail: Vec<f32> = out[FRAME_SAMPLES * 2 * 10..]
            .iter()
            .map(|&s| s as f32 / 32768.0)
            .collect();
        let e = rms(&tail);
        assert!((e - 0.3535).abs() < 0.05, "{e}");
    }

    #[test]
    fn jitter_buffer_reorders_and_marks_loss() {
        let mut jb = JitterBuffer::new(2, 8);
        assert_eq!(jb.pop(), JitterSlot::Wait);
        jb.push(11, vec![1]);
        assert_eq!(jb.pop(), JitterSlot::Wait);
        jb.push(10, vec![0]);
        assert_eq!(jb.pop(), JitterSlot::Frame(vec![0]));
        assert_eq!(jb.pop(), JitterSlot::Frame(vec![1]));
        jb.push(13, vec![3]);
        assert_eq!(jb.pop(), JitterSlot::Lost);
        assert_eq!(jb.pop(), JitterSlot::Frame(vec![3]));
        jb.push(5, vec![9]);
        assert_eq!(jb.late, 1);
        assert_eq!(jb.lost, 1);
    }

    #[test]
    fn mixer_pans_directional_streams() {
        let mut enc = CaptureEncoder::new(32_000).unwrap();
        let pcm = sine(FRAME_SAMPLES * 10, 48_000, 440.0, 0.5);
        let mut frames = Vec::new();
        enc.push_f32(&pcm, 48_000, 1, |f| frames.push(f));
        let mut mixer = RemoteMixer::new(1, 12);
        let right = Direction {
            azimuth: std::f32::consts::FRAC_PI_2,
            elevation: 0.0,
        };
        for (i, f) in frames.iter().enumerate() {
            mixer
                .push(1, i as u32, 0.5, Some(right), f.opus.clone())
                .unwrap();
        }
        let mut out = vec![0f32; FRAME_SAMPLES * 2 * 10];
        mixer.mix(&mut out, 2);
        let l: Vec<f32> = out
            .iter()
            .step_by(2)
            .skip(FRAME_SAMPLES * 2)
            .copied()
            .collect();
        let r: Vec<f32> = out
            .iter()
            .skip(1)
            .step_by(2)
            .skip(FRAME_SAMPLES * 2)
            .copied()
            .collect();
        assert!(rms(&l) < 0.01, "{}", rms(&l));
        assert!((rms(&r) - 0.25).abs() < 0.05, "{}", rms(&r));
    }
}
