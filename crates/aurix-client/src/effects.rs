//! Voice effects: the extension point between the capture DSP and the encoder. Every 20 ms
//! 48 kHz frame — cleaned up (high-pass / AEC / NS / AGC) and gain-staged — runs through the
//! [`EffectChain`] before VAD metering and Opus/PCMU encoding, so what peers hear, what the
//! level byte says and what the server transcribes is the processed voice. Effects see the
//! microphone only (injected audio, TTS and the downlink are untouched) and run on the audio
//! thread: no allocation or blocking inside [`VoiceEffect::process`].
//!
//! Two reference effects ship with the core — a delay-line [`PitchShift`] and a
//! [`RingModulator`] — mostly as a template; games plug their own through the trait (or the
//! C ABI callback) for anything fancier (formant shifting, radio bandpass + static, reverb).

use std::f32::consts::PI;

use crate::audio::{FRAME_SAMPLES, SAMPLE_RATE};

/// One stage of the capture effect chain.
pub trait VoiceEffect: Send {
    /// Process one 20 ms frame in place: `frame` is interleaved 48 kHz PCM in `-1..=1` with
    /// `channels` (1 or 2) samples per position, `FRAME_SAMPLES * channels` long.
    fn process(&mut self, frame: &mut [f32], channels: u8);

    /// Forget history (device switch, long mute); the next frame starts clean.
    fn reset(&mut self) {}
}

/// Ordered stages applied to every capture frame; empty = bypass.
#[derive(Default)]
pub struct EffectChain {
    stages: Vec<Box<dyn VoiceEffect>>,
}

impl std::fmt::Debug for EffectChain {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EffectChain")
            .field("stages", &self.stages.len())
            .finish()
    }
}

impl EffectChain {
    pub fn new(stages: Vec<Box<dyn VoiceEffect>>) -> Self {
        Self { stages }
    }

    pub fn is_empty(&self) -> bool {
        self.stages.is_empty()
    }

    pub fn len(&self) -> usize {
        self.stages.len()
    }

    pub fn push(&mut self, stage: Box<dyn VoiceEffect>) {
        self.stages.push(stage);
    }

    pub fn clear(&mut self) {
        self.stages.clear();
    }

    pub fn process(&mut self, frame: &mut [f32], channels: u8) {
        if self.stages.is_empty() {
            return;
        }
        for stage in &mut self.stages {
            stage.process(frame, channels);
        }
        for s in frame.iter_mut() {
            *s = s.clamp(-1.0, 1.0);
        }
    }

    pub fn reset(&mut self) {
        for stage in &mut self.stages {
            stage.reset();
        }
    }
}

/// Largest pitch shift either way (two octaves).
pub const MAX_PITCH_SEMITONES: f32 = 24.0;
/// Highest ring-modulator carrier (above this the effect is just noise).
pub const MAX_RING_MOD_HZ: f32 = 2000.0;

/// Grain length of the delay-line pitch shifter (~21 ms at 48 kHz): long enough for speech
/// pitch, short enough that the doubled/halved read speed stays inside one frame's latency.
const GRAIN: usize = 1024;

/// Delay-line pitch shifter: two read taps sweep a ring buffer at `ratio` while a raised-cosine
/// crossfade hides the wrap-around discontinuity (the classic "granular" voice changer — cheap,
/// zero look-ahead, a slight tremolo on sustained tones; formants shift with the pitch).
#[derive(Debug, Clone)]
pub struct PitchShift {
    semitones: f32,
    ratio: f32,
    ring: Vec<Vec<f32>>,
    write: usize,
    phase: f32,
}

impl PitchShift {
    /// `semitones` is clamped to `±MAX_PITCH_SEMITONES`; `0` passes audio through.
    pub fn new(semitones: f32) -> Self {
        let semitones = semitones.clamp(-MAX_PITCH_SEMITONES, MAX_PITCH_SEMITONES);
        Self {
            semitones,
            ratio: 2f32.powf(semitones / 12.0),
            ring: Vec::new(),
            write: 0,
            phase: 0.0,
        }
    }

    pub fn semitones(&self) -> f32 {
        self.semitones
    }

    fn read(ring: &[f32], pos: f32) -> f32 {
        let len = ring.len();
        let i = pos.floor() as usize % len;
        let frac = pos - pos.floor();
        let a = ring[i];
        let b = ring[(i + 1) % len];
        a + (b - a) * frac
    }
}

impl VoiceEffect for PitchShift {
    fn process(&mut self, frame: &mut [f32], channels: u8) {
        if self.semitones == 0.0 {
            return;
        }
        let channels = channels.clamp(1, 2) as usize;
        if self.ring.len() != channels {
            self.ring = vec![vec![0.0; GRAIN * 2]; channels];
            self.write = 0;
            self.phase = 0.0;
        }
        let len = GRAIN * 2;
        for pos in frame.chunks_exact_mut(channels) {
            for (ch, s) in pos.iter_mut().enumerate() {
                self.ring[ch][self.write] = *s;
            }
            // Tap A trails the write head by `phase` samples, tap B by `phase + GRAIN`; the
            // read speed differs from the write speed by `ratio`, so the trail grows/shrinks.
            let delay_a = self.phase;
            let delay_b = (self.phase + GRAIN as f32) % (GRAIN as f32 * 2.0);
            // Raised-cosine weights: a tap fades out as its delay approaches the wrap point.
            let w_a = 0.5 - 0.5 * (2.0 * PI * delay_a / (GRAIN as f32 * 2.0)).cos();
            let w_b = 1.0 - w_a;
            let write = self.write as f32;
            for (ch, s) in pos.iter_mut().enumerate() {
                let ring = &self.ring[ch];
                let pa = (write - delay_a).rem_euclid(len as f32);
                let pb = (write - delay_b).rem_euclid(len as f32);
                *s = Self::read(ring, pa) * w_a + Self::read(ring, pb) * w_b;
            }
            self.write = (self.write + 1) % len;
            self.phase = (self.phase + (1.0 - self.ratio)).rem_euclid(GRAIN as f32 * 2.0);
        }
    }

    fn reset(&mut self) {
        self.ring.clear();
        self.write = 0;
        self.phase = 0.0;
    }
}

/// Multiplies the voice by a sine carrier — the "robot" / Dalek effect.
#[derive(Debug, Clone)]
pub struct RingModulator {
    carrier_hz: f32,
    phase: f32,
}

impl RingModulator {
    /// `carrier_hz` is clamped to `0..=MAX_RING_MOD_HZ`; `0` passes audio through.
    pub fn new(carrier_hz: f32) -> Self {
        Self {
            carrier_hz: carrier_hz.clamp(0.0, MAX_RING_MOD_HZ),
            phase: 0.0,
        }
    }

    pub fn carrier_hz(&self) -> f32 {
        self.carrier_hz
    }
}

impl VoiceEffect for RingModulator {
    fn process(&mut self, frame: &mut [f32], channels: u8) {
        if self.carrier_hz <= 0.0 {
            return;
        }
        let channels = channels.clamp(1, 2) as usize;
        let step = 2.0 * PI * self.carrier_hz / SAMPLE_RATE as f32;
        for pos in frame.chunks_exact_mut(channels) {
            let carrier = self.phase.sin();
            for s in pos.iter_mut() {
                *s *= carrier;
            }
            self.phase += step;
            if self.phase >= 2.0 * PI {
                self.phase -= 2.0 * PI;
            }
        }
    }

    fn reset(&mut self) {
        self.phase = 0.0;
    }
}

/// An effect implemented by the host through a plain function pointer (the C ABI hook).
pub struct CallbackEffect<F: FnMut(&mut [f32], u8) + Send> {
    f: F,
}

impl<F: FnMut(&mut [f32], u8) + Send> CallbackEffect<F> {
    pub fn new(f: F) -> Self {
        Self { f }
    }
}

impl<F: FnMut(&mut [f32], u8) + Send> VoiceEffect for CallbackEffect<F> {
    fn process(&mut self, frame: &mut [f32], channels: u8) {
        (self.f)(frame, channels);
    }
}

/// Frames the reference effects are designed for (one 20 ms frame per call).
pub const fn frame_len(channels: u8) -> usize {
    FRAME_SAMPLES * channels as usize
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sine(hz: f32, frames: usize) -> Vec<f32> {
        (0..frames * FRAME_SAMPLES)
            .map(|n| (2.0 * PI * hz * n as f32 / SAMPLE_RATE as f32).sin() * 0.5)
            .collect()
    }

    /// Dominant frequency by zero-crossing rate (good enough for a clean tone).
    fn zero_crossing_hz(pcm: &[f32]) -> f32 {
        let crossings = pcm
            .windows(2)
            .filter(|w| (w[0] < 0.0) != (w[1] < 0.0))
            .count();
        crossings as f32 / 2.0 * SAMPLE_RATE as f32 / pcm.len() as f32
    }

    fn run(effect: &mut dyn VoiceEffect, pcm: &[f32], channels: u8) -> Vec<f32> {
        let mut out = pcm.to_vec();
        for frame in out.chunks_exact_mut(frame_len(channels)) {
            effect.process(frame, channels);
        }
        out
    }

    #[test]
    fn empty_chain_is_a_bypass() {
        let mut chain = EffectChain::default();
        let input = sine(440.0, 2);
        let mut frame = input.clone();
        chain.process(&mut frame, 1);
        assert_eq!(frame, input);
        assert!(chain.is_empty());
    }

    #[test]
    fn pitch_shift_up_an_octave_doubles_the_frequency() {
        let mut fx = PitchShift::new(12.0);
        let out = run(&mut fx, &sine(220.0, 50), 1);
        // Skip the first grain while the ring buffer fills.
        let steady = &out[FRAME_SAMPLES * 5..];
        let hz = zero_crossing_hz(steady);
        assert!((400.0..480.0).contains(&hz), "got {hz} Hz");
        let energy = crate::audio::rms(steady);
        assert!(energy > 0.2, "level collapsed to {energy}");
    }

    #[test]
    fn pitch_shift_down_an_octave_halves_the_frequency() {
        let mut fx = PitchShift::new(-12.0);
        let out = run(&mut fx, &sine(440.0, 50), 1);
        let hz = zero_crossing_hz(&out[FRAME_SAMPLES * 5..]);
        assert!((190.0..250.0).contains(&hz), "got {hz} Hz");
    }

    #[test]
    fn pitch_shift_keeps_stereo_channels_apart_and_zero_is_bypass() {
        let mut fx = PitchShift::new(0.0);
        let mut frame = vec![0.25; frame_len(2)];
        fx.process(&mut frame, 2);
        assert!(frame.iter().all(|&s| s == 0.25));

        let mut fx = PitchShift::new(7.0);
        // Left carries a tone, right is silent: the shifter must not bleed between them.
        let left = sine(300.0, 20);
        let mut stereo = Vec::with_capacity(left.len() * 2);
        for s in &left {
            stereo.push(*s);
            stereo.push(0.0);
        }
        let out = run(&mut fx, &stereo, 2);
        assert!(out.iter().skip(1).step_by(2).all(|&r| r == 0.0));
        assert!(out.iter().step_by(2).any(|&l| l.abs() > 0.1));
    }

    #[test]
    fn pitch_shift_clamps_and_resets() {
        let fx = PitchShift::new(99.0);
        assert_eq!(fx.semitones(), MAX_PITCH_SEMITONES);
        let mut fx = PitchShift::new(5.0);
        let _ = run(&mut fx, &sine(440.0, 3), 1);
        fx.reset();
        let mut silence = vec![0.0; FRAME_SAMPLES];
        fx.process(&mut silence, 1);
        assert!(silence.iter().all(|&s| s == 0.0), "history survived reset");
    }

    #[test]
    fn ring_modulator_produces_sidebands_and_no_dc() {
        let mut fx = RingModulator::new(100.0);
        let out = run(&mut fx, &sine(440.0, 10), 1);
        // 440 × 100 Hz → 340 + 540 Hz, no 440 Hz component: the mean stays ~0 and the level
        // halves (sin·sin = ½(cos − cos)).
        let mean: f32 = out.iter().sum::<f32>() / out.len() as f32;
        assert!(mean.abs() < 0.01);
        let level = crate::audio::rms(&out);
        let input_level = crate::audio::rms(&sine(440.0, 10));
        assert!(
            (level / input_level - 0.707).abs() < 0.05,
            "ratio {}",
            level / input_level
        );
        assert_eq!(RingModulator::new(-5.0).carrier_hz(), 0.0);
        assert_eq!(RingModulator::new(1e9).carrier_hz(), MAX_RING_MOD_HZ);
    }

    #[test]
    fn chain_runs_stages_in_order_and_clamps() {
        let mut chain = EffectChain::default();
        chain.push(Box::new(CallbackEffect::new(|f: &mut [f32], _| {
            for s in f.iter_mut() {
                *s += 1.0;
            }
        })));
        chain.push(Box::new(CallbackEffect::new(|f: &mut [f32], _| {
            for s in f.iter_mut() {
                *s *= 3.0;
            }
        })));
        let mut frame = vec![0.1; FRAME_SAMPLES];
        chain.process(&mut frame, 1);
        // (0.1 + 1) × 3 = 3.3 → clamped to 1.0 (not 0.1 × 3 + 1 = 1.3, either way clamped, so
        // check ordering with a negative input: (−0.5 + 1) × 3 = 1.5 → 1, vs −0.5 × 3 + 1 = −0.5).
        assert!(frame.iter().all(|&s| s == 1.0));
        let mut frame = vec![-0.5; FRAME_SAMPLES];
        chain.process(&mut frame, 1);
        assert!(frame.iter().all(|&s| s == 1.0));
        assert_eq!(chain.len(), 2);
        chain.clear();
        assert!(chain.is_empty());
    }
}
