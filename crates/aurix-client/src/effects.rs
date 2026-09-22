//! Voice effects: the extension point between the capture DSP and the encoder. Every 20 ms
//! 48 kHz frame — cleaned up (high-pass / AEC / NS / AGC) and gain-staged — runs through the
//! [`EffectChain`] before VAD metering and Opus/PCMU encoding, so what peers hear, what the
//! level byte says and what the server transcribes is the processed voice. Effects see the
//! microphone only (injected audio, TTS and the downlink are untouched) and run on the audio
//! thread: no allocation or blocking inside [`VoiceEffect::process`].
//!
//! The built-in library — [`PitchShift`], [`FormantShift`], [`RingModulator`], [`Biquad`]
//! filters, [`Distortion`], [`Tremolo`], [`Static`], [`Reverb`] — is parameterised by
//! [`VoiceEffectParams`] (one struct = one chain, mirrored 1:1 by the C ABI) and ships the
//! [`EffectPreset`]s games usually want (robot, monster, radio, helium, ghost). Every stage
//! preallocates in its constructor and is stereo-safe (per-channel state, no bleed). Games
//! plug their own through the trait (or the C ABI callback) for anything fancier.

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
/// Largest formant shift either way (one octave of vocal-tract scaling).
pub const MAX_FORMANT_SEMITONES: f32 = 12.0;
/// Hardest distortion drive (`1` is the softest audible saturation).
pub const MAX_DISTORTION_DRIVE: f32 = 20.0;
/// Fastest tremolo.
pub const MAX_TREMOLO_HZ: f32 = 20.0;
/// Lowest usable filter corner.
pub const MIN_FILTER_HZ: f32 = 20.0;
/// Highest usable filter corner (just under Nyquist).
pub const MAX_FILTER_HZ: f32 = 20_000.0;

/// Every built-in stage in one place: zero (or `false`) means "that stage is off", so the
/// default is a bypass. [`VoiceEffectParams::chain`] builds the chain in a fixed order —
/// filters, formant, pitch, ring modulator, distortion, tremolo, static, reverb — which is
/// the order the presets were tuned for.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct VoiceEffectParams {
    /// High-pass corner in Hz (`0`: off) — thins the voice (radio, phone).
    pub highpass_hz: f32,
    /// Low-pass corner in Hz (`0`: off) — muffles it (ghost, distance).
    pub lowpass_hz: f32,
    /// Formant shift in semitones (`±MAX_FORMANT_SEMITONES`): vocal-tract size without
    /// changing the pitch — up sounds small/childlike, down large/monstrous.
    pub formant_semitones: f32,
    /// Pitch shift in semitones (`±MAX_PITCH_SEMITONES`), formants follow.
    pub pitch_semitones: f32,
    /// Ring-modulator carrier in Hz (`0..=MAX_RING_MOD_HZ`) — the robot / Dalek.
    pub ring_mod_hz: f32,
    /// Saturation drive (`0`: off, `1..=MAX_DISTORTION_DRIVE`).
    pub distortion_drive: f32,
    /// Tremolo rate in Hz (`0`: off, up to `MAX_TREMOLO_HZ`).
    pub tremolo_hz: f32,
    /// Tremolo depth `0..=1` (how deep the level dips).
    pub tremolo_depth: f32,
    /// Static / hiss level `0..=1` mixed in while the voice is active (radio).
    pub static_level: f32,
    /// Reverb wet mix `0..=1` (`0`: off).
    pub reverb_mix: f32,
    /// Reverb room size `0..=1` (decay length).
    pub reverb_size: f32,
    /// Reverb high-frequency damping `0..=1`.
    pub reverb_damping: f32,
}

impl VoiceEffectParams {
    /// Every stage off.
    pub const BYPASS: Self = Self {
        highpass_hz: 0.0,
        lowpass_hz: 0.0,
        formant_semitones: 0.0,
        pitch_semitones: 0.0,
        ring_mod_hz: 0.0,
        distortion_drive: 0.0,
        tremolo_hz: 0.0,
        tremolo_depth: 0.0,
        static_level: 0.0,
        reverb_mix: 0.0,
        reverb_size: 0.0,
        reverb_damping: 0.0,
    };

    /// Clamps every field into its documented range (NaN → off).
    pub fn sanitized(self) -> Self {
        fn clamp(v: f32, lo: f32, hi: f32) -> f32 {
            if v.is_finite() {
                v.clamp(lo, hi)
            } else {
                0.0
            }
        }
        fn corner(v: f32) -> f32 {
            if v.is_finite() && v > 0.0 {
                v.clamp(MIN_FILTER_HZ, MAX_FILTER_HZ)
            } else {
                0.0
            }
        }
        Self {
            highpass_hz: corner(self.highpass_hz),
            lowpass_hz: corner(self.lowpass_hz),
            formant_semitones: clamp(
                self.formant_semitones,
                -MAX_FORMANT_SEMITONES,
                MAX_FORMANT_SEMITONES,
            ),
            pitch_semitones: clamp(
                self.pitch_semitones,
                -MAX_PITCH_SEMITONES,
                MAX_PITCH_SEMITONES,
            ),
            ring_mod_hz: clamp(self.ring_mod_hz, 0.0, MAX_RING_MOD_HZ),
            distortion_drive: if self.distortion_drive.is_finite() && self.distortion_drive > 0.0 {
                self.distortion_drive.clamp(1.0, MAX_DISTORTION_DRIVE)
            } else {
                0.0
            },
            tremolo_hz: clamp(self.tremolo_hz, 0.0, MAX_TREMOLO_HZ),
            tremolo_depth: clamp(self.tremolo_depth, 0.0, 1.0),
            static_level: clamp(self.static_level, 0.0, 1.0),
            reverb_mix: clamp(self.reverb_mix, 0.0, 1.0),
            reverb_size: clamp(self.reverb_size, 0.0, 1.0),
            reverb_damping: clamp(self.reverb_damping, 0.0, 1.0),
        }
    }

    /// Whether [`chain`](Self::chain) would be empty.
    pub fn is_bypass(&self) -> bool {
        self.sanitized() == Self::BYPASS
    }

    /// Builds the chain (allocating here, on the caller's thread — the stages themselves do
    /// not allocate while processing).
    pub fn chain(&self) -> EffectChain {
        let p = self.sanitized();
        let mut chain = EffectChain::default();
        if p.highpass_hz > 0.0 {
            chain.push(Box::new(Biquad::highpass(p.highpass_hz, 0.707)));
        }
        if p.lowpass_hz > 0.0 {
            chain.push(Box::new(Biquad::lowpass(p.lowpass_hz, 0.707)));
        }
        if p.formant_semitones != 0.0 {
            chain.push(Box::new(FormantShift::new(p.formant_semitones)));
        }
        if p.pitch_semitones != 0.0 {
            chain.push(Box::new(PitchShift::new(p.pitch_semitones)));
        }
        if p.ring_mod_hz > 0.0 {
            chain.push(Box::new(RingModulator::new(p.ring_mod_hz)));
        }
        if p.distortion_drive > 0.0 {
            chain.push(Box::new(Distortion::new(p.distortion_drive)));
        }
        if p.tremolo_hz > 0.0 && p.tremolo_depth > 0.0 {
            chain.push(Box::new(Tremolo::new(p.tremolo_hz, p.tremolo_depth)));
        }
        if p.static_level > 0.0 {
            chain.push(Box::new(Static::new(p.static_level)));
        }
        if p.reverb_mix > 0.0 {
            chain.push(Box::new(Reverb::new(
                p.reverb_size,
                p.reverb_damping,
                p.reverb_mix,
            )));
        }
        chain
    }
}

/// Ready-made voices; [`EffectPreset::params`] is a starting point games tweak.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum EffectPreset {
    /// Ring-modulated, band-limited, slightly saturated.
    Robot,
    /// Pitched and formant-shifted down, growly, in a large space.
    Monster,
    /// Telephone band, crunchy, with static under the voice.
    Radio,
    /// Pitched and formant-shifted up.
    Helium,
    /// Hollow, wavering, drenched in reverb.
    Ghost,
}

impl EffectPreset {
    pub const ALL: [EffectPreset; 5] = [
        EffectPreset::Robot,
        EffectPreset::Monster,
        EffectPreset::Radio,
        EffectPreset::Helium,
        EffectPreset::Ghost,
    ];

    pub fn params(self) -> VoiceEffectParams {
        let off = VoiceEffectParams::BYPASS;
        match self {
            EffectPreset::Robot => VoiceEffectParams {
                highpass_hz: 200.0,
                lowpass_hz: 4000.0,
                ring_mod_hz: 60.0,
                distortion_drive: 2.0,
                ..off
            },
            EffectPreset::Monster => VoiceEffectParams {
                formant_semitones: -5.0,
                pitch_semitones: -7.0,
                distortion_drive: 1.5,
                reverb_mix: 0.15,
                reverb_size: 0.6,
                reverb_damping: 0.5,
                ..off
            },
            EffectPreset::Radio => VoiceEffectParams {
                highpass_hz: 400.0,
                lowpass_hz: 3000.0,
                distortion_drive: 3.0,
                static_level: 0.03,
                ..off
            },
            EffectPreset::Helium => VoiceEffectParams {
                formant_semitones: 6.0,
                pitch_semitones: 6.0,
                ..off
            },
            EffectPreset::Ghost => VoiceEffectParams {
                lowpass_hz: 5000.0,
                formant_semitones: 2.0,
                pitch_semitones: -3.0,
                tremolo_hz: 5.0,
                tremolo_depth: 0.5,
                reverb_mix: 0.6,
                reverb_size: 0.9,
                reverb_damping: 0.3,
                ..off
            },
        }
    }

    pub fn chain(self) -> EffectChain {
        self.params().chain()
    }

    pub fn name(self) -> &'static str {
        match self {
            EffectPreset::Robot => "robot",
            EffectPreset::Monster => "monster",
            EffectPreset::Radio => "radio",
            EffectPreset::Helium => "helium",
            EffectPreset::Ghost => "ghost",
        }
    }
}

impl std::str::FromStr for EffectPreset {
    type Err = ();

    fn from_str(s: &str) -> std::result::Result<Self, ()> {
        EffectPreset::ALL
            .into_iter()
            .find(|p| p.name().eq_ignore_ascii_case(s.trim()))
            .ok_or(())
    }
}

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
    /// One ring per channel, both allocated up front (the audio thread never allocates).
    ring: Vec<Vec<f32>>,
    channels: usize,
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
            ring: vec![vec![0.0; GRAIN * 2]; 2],
            channels: 0,
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
        if self.channels != channels {
            self.channels = channels;
            for ring in &mut self.ring {
                ring.fill(0.0);
            }
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
        for ring in &mut self.ring {
            ring.fill(0.0);
        }
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

/// Shortest / longest pitch period the formant shifter tracks (800 Hz … 80 Hz).
const MIN_PERIOD: usize = 60;
const MAX_PERIOD: usize = 600;
/// Period assumed while the voice is unvoiced (noise, silence): grains still have to be
/// spaced somehow, and 240 Hz keeps the artefacts above the speech fundamental.
const UNVOICED_PERIOD: usize = 200;
/// Samples of look-ahead the grain synthesis needs (≥ `MAX_PERIOD × (1 + max ratio)`).
const FORMANT_LATENCY: usize = 2048;
const FORMANT_RING: usize = 8192;
/// Autocorrelation window for pitch tracking.
const PITCH_WINDOW: usize = FRAME_SAMPLES;
const PITCH_HISTORY: usize = PITCH_WINDOW + MAX_PERIOD;

/// Pitch-synchronous overlap-add formant shifter: grains two pitch periods long are cut at
/// the tracked pitch marks, resampled by the formant ratio and laid back down at the same
/// marks — the periodicity (pitch) stays, the spectral envelope (vocal tract) scales. Adds
/// ~43 ms of latency (`FORMANT_LATENCY`). Stereo channels are processed independently with
/// the pitch tracked on their mix.
#[derive(Debug, Clone)]
pub struct FormantShift {
    semitones: f32,
    ratio: f32,
    channels: usize,
    input: Vec<Vec<f32>>,
    output: Vec<Vec<f32>>,
    mono: Vec<f32>,
    /// Samples pushed so far (absolute input time).
    written: u64,
    /// Absolute input time of the next grain centre.
    next_mark: u64,
    period: usize,
}

impl FormantShift {
    /// `semitones` is clamped to `±MAX_FORMANT_SEMITONES`; `0` passes audio through.
    pub fn new(semitones: f32) -> Self {
        let semitones = semitones.clamp(-MAX_FORMANT_SEMITONES, MAX_FORMANT_SEMITONES);
        Self {
            semitones,
            ratio: 2f32.powf(semitones / 12.0),
            channels: 0,
            input: vec![vec![0.0; FORMANT_RING]; 2],
            output: vec![vec![0.0; FORMANT_RING]; 2],
            mono: vec![0.0; PITCH_HISTORY],
            written: 0,
            next_mark: 0,
            period: UNVOICED_PERIOD,
        }
    }

    pub fn semitones(&self) -> f32 {
        self.semitones
    }

    /// Latency in samples between input and output.
    pub const fn latency(&self) -> usize {
        FORMANT_LATENCY
    }

    fn ensure_channels(&mut self, channels: usize) {
        if self.channels != channels {
            self.channels = channels;
            self.clear_history();
        }
    }

    fn clear_history(&mut self) {
        for ring in self.input.iter_mut().chain(self.output.iter_mut()) {
            ring.fill(0.0);
        }
        self.mono.fill(0.0);
        self.written = 0;
        self.next_mark = 0;
        self.period = UNVOICED_PERIOD;
    }

    /// Normalised autocorrelation over the last `PITCH_WINDOW` samples; picks the shortest lag
    /// within 15 % of the best correlation (against octave errors). `None` when unvoiced.
    fn track_pitch(&self) -> Option<usize> {
        let x = &self.mono;
        let end = x.len();
        let cur = &x[end - PITCH_WINDOW..];
        let energy: f32 = cur.iter().map(|s| s * s).sum();
        if energy < 1e-4 * PITCH_WINDOW as f32 {
            return None;
        }
        let corr = |lag: usize| -> f32 {
            let past = &x[end - PITCH_WINDOW - lag..end - lag];
            let dot: f32 = cur.iter().zip(past).map(|(a, b)| a * b).sum();
            let past_energy: f32 = past.iter().map(|s| s * s).sum();
            dot / (energy * past_energy).sqrt().max(1e-9)
        };
        let mut best = (0usize, f32::MIN);
        let mut lag = MIN_PERIOD;
        while lag <= MAX_PERIOD {
            let r = corr(lag);
            if r > best.1 {
                best = (lag, r);
            }
            lag += 4;
        }
        if best.1 < 0.5 {
            return None;
        }
        // Sub-harmonics correlate too; prefer the shortest strong lag, then refine it.
        let mut chosen = best.0;
        let mut lag = MIN_PERIOD;
        while lag < best.0 {
            if corr(lag) >= best.1 * 0.85 {
                chosen = lag;
                break;
            }
            lag += 4;
        }
        let lo = chosen.saturating_sub(3).max(MIN_PERIOD);
        let hi = (chosen + 3).min(MAX_PERIOD);
        let mut refined = (chosen, f32::MIN);
        for lag in lo..=hi {
            let r = corr(lag);
            if r > refined.1 {
                refined = (lag, r);
            }
        }
        Some(refined.0)
    }

    fn read(ring: &[f32], pos: f64) -> f32 {
        let len = ring.len();
        let base = pos.floor();
        let frac = (pos - base) as f32;
        let i = (base as u64 % len as u64) as usize;
        let a = ring[i];
        let b = ring[(i + 1) % len];
        a + (b - a) * frac
    }

    /// Overlap-adds one grain centred on `mark` into the output rings.
    fn synthesize(&mut self, mark: u64, period: usize) {
        let half = period as f32;
        let ratio = self.ratio as f64;
        for ch in 0..self.channels {
            for k in -(period as i64)..(period as i64) {
                let w = 0.5 + 0.5 * (PI * k as f32 / half).cos();
                let src = mark as f64 + k as f64 * ratio;
                let sample = Self::read(&self.input[ch], src);
                let dst = ((mark as i64 + k) as u64 % FORMANT_RING as u64) as usize;
                self.output[ch][dst] += w * sample;
            }
        }
    }
}

impl VoiceEffect for FormantShift {
    fn process(&mut self, frame: &mut [f32], channels: u8) {
        if self.semitones == 0.0 {
            return;
        }
        let channels = channels.clamp(1, 2) as usize;
        self.ensure_channels(channels);
        let positions = frame.len() / channels;

        // Ingest: per-channel rings plus the mono pitch-tracking history.
        self.mono.copy_within(positions.., 0);
        let mono_from = PITCH_HISTORY - positions;
        for (n, pos) in frame.chunks_exact(channels).enumerate() {
            let idx = ((self.written + n as u64) % FORMANT_RING as u64) as usize;
            let mut sum = 0.0;
            for (ch, s) in pos.iter().enumerate() {
                self.input[ch][idx] = *s;
                sum += *s;
            }
            self.mono[mono_from + n] = sum / channels as f32;
        }
        self.written += positions as u64;

        if let Some(p) = self.track_pitch() {
            self.period = p;
        } else {
            self.period = UNVOICED_PERIOD;
        }

        // Lay down every grain whose input span is available.
        let reach = (self.period as f64 * self.ratio.max(1.0) as f64).ceil() as u64;
        while self.next_mark + reach <= self.written {
            let mark = self.next_mark;
            let period = self.period;
            self.synthesize(mark, period);
            self.next_mark += period as u64;
        }

        // Emit the delayed output and clear it for the next lap of the ring.
        let end = self.written.saturating_sub(FORMANT_LATENCY as u64);
        for (n, pos) in frame.chunks_exact_mut(channels).enumerate() {
            let t = end as i64 - positions as i64 + n as i64;
            for (ch, s) in pos.iter_mut().enumerate() {
                if t < 0 {
                    *s = 0.0;
                    continue;
                }
                let idx = (t as u64 % FORMANT_RING as u64) as usize;
                *s = self.output[ch][idx];
                self.output[ch][idx] = 0.0;
            }
        }
    }

    fn reset(&mut self) {
        self.clear_history();
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum BiquadKind {
    LowPass,
    HighPass,
}

/// Second-order IIR filter (RBJ cookbook), transposed direct form II, per-channel state.
#[derive(Debug, Clone)]
pub struct Biquad {
    kind: BiquadKind,
    hz: f32,
    b0: f32,
    b1: f32,
    b2: f32,
    a1: f32,
    a2: f32,
    state: [[f32; 2]; 2],
}

impl Biquad {
    fn design(kind: BiquadKind, hz: f32, q: f32) -> Self {
        let hz = hz.clamp(MIN_FILTER_HZ, MAX_FILTER_HZ);
        let q = q.max(0.1);
        let w0 = 2.0 * PI * hz / SAMPLE_RATE as f32;
        let (sin, cos) = w0.sin_cos();
        let alpha = sin / (2.0 * q);
        let a0 = 1.0 + alpha;
        let (b0, b1, b2) = match kind {
            BiquadKind::LowPass => ((1.0 - cos) / 2.0, 1.0 - cos, (1.0 - cos) / 2.0),
            BiquadKind::HighPass => ((1.0 + cos) / 2.0, -(1.0 + cos), (1.0 + cos) / 2.0),
        };
        Self {
            kind,
            hz,
            b0: b0 / a0,
            b1: b1 / a0,
            b2: b2 / a0,
            a1: -2.0 * cos / a0,
            a2: (1.0 - alpha) / a0,
            state: [[0.0; 2]; 2],
        }
    }

    /// Low-pass with corner `hz` and quality `q` (0.707 = Butterworth).
    pub fn lowpass(hz: f32, q: f32) -> Self {
        Self::design(BiquadKind::LowPass, hz, q)
    }

    /// High-pass with corner `hz` and quality `q`.
    pub fn highpass(hz: f32, q: f32) -> Self {
        Self::design(BiquadKind::HighPass, hz, q)
    }

    pub fn corner_hz(&self) -> f32 {
        self.hz
    }

    pub fn is_lowpass(&self) -> bool {
        self.kind == BiquadKind::LowPass
    }
}

impl VoiceEffect for Biquad {
    fn process(&mut self, frame: &mut [f32], channels: u8) {
        let channels = channels.clamp(1, 2) as usize;
        for pos in frame.chunks_exact_mut(channels) {
            for (ch, s) in pos.iter_mut().enumerate() {
                let x = *s;
                let [z1, z2] = &mut self.state[ch];
                let y = self.b0 * x + *z1;
                *z1 = self.b1 * x - self.a1 * y + *z2;
                *z2 = self.b2 * x - self.a2 * y;
                *s = y;
            }
        }
    }

    fn reset(&mut self) {
        self.state = [[0.0; 2]; 2];
    }
}

/// Soft saturation: `tanh`-shaped waveshaper normalised so full scale stays full scale.
#[derive(Debug, Clone)]
pub struct Distortion {
    drive: f32,
    norm: f32,
}

impl Distortion {
    /// `drive` is clamped to `1..=MAX_DISTORTION_DRIVE`.
    pub fn new(drive: f32) -> Self {
        let drive = drive.clamp(1.0, MAX_DISTORTION_DRIVE);
        Self {
            drive,
            norm: 1.0 / Self::shape(drive),
        }
    }

    pub fn drive(&self) -> f32 {
        self.drive
    }

    /// Rational tanh approximation, monotonic and bounded on `-3..=3`, hard-clipped beyond.
    fn shape(x: f32) -> f32 {
        let x = x.clamp(-3.0, 3.0);
        let x2 = x * x;
        x * (27.0 + x2) / (27.0 + 9.0 * x2)
    }
}

impl VoiceEffect for Distortion {
    fn process(&mut self, frame: &mut [f32], _channels: u8) {
        for s in frame.iter_mut() {
            *s = Self::shape(*s * self.drive) * self.norm;
        }
    }
}

/// Amplitude modulation by a slow sine: level dips by `depth` at `rate_hz`.
#[derive(Debug, Clone)]
pub struct Tremolo {
    rate_hz: f32,
    depth: f32,
    phase: f32,
}

impl Tremolo {
    /// `rate_hz` is clamped to `0..=MAX_TREMOLO_HZ`, `depth` to `0..=1`.
    pub fn new(rate_hz: f32, depth: f32) -> Self {
        Self {
            rate_hz: rate_hz.clamp(0.0, MAX_TREMOLO_HZ),
            depth: depth.clamp(0.0, 1.0),
            phase: 0.0,
        }
    }

    pub fn rate_hz(&self) -> f32 {
        self.rate_hz
    }

    pub fn depth(&self) -> f32 {
        self.depth
    }
}

impl VoiceEffect for Tremolo {
    fn process(&mut self, frame: &mut [f32], channels: u8) {
        if self.rate_hz <= 0.0 || self.depth <= 0.0 {
            return;
        }
        let channels = channels.clamp(1, 2) as usize;
        let step = 2.0 * PI * self.rate_hz / SAMPLE_RATE as f32;
        for pos in frame.chunks_exact_mut(channels) {
            let gain = 1.0 - self.depth * (0.5 - 0.5 * self.phase.cos());
            for s in pos.iter_mut() {
                *s *= gain;
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

/// White-noise hiss gated by the voice level (silence stays silent, so it does not trip the
/// VAD or hold a transmission open).
#[derive(Debug, Clone)]
pub struct Static {
    level: f32,
    seed: u32,
    envelope: f32,
}

impl Static {
    /// `level` is clamped to `0..=1` (full scale white noise at `1`).
    pub fn new(level: f32) -> Self {
        Self {
            level: level.clamp(0.0, 1.0),
            seed: 0x9E37_79B9,
            envelope: 0.0,
        }
    }

    pub fn level(&self) -> f32 {
        self.level
    }

    fn noise(&mut self) -> f32 {
        self.seed = self
            .seed
            .wrapping_mul(1_664_525)
            .wrapping_add(1_013_904_223);
        (self.seed >> 8) as f32 / (1u32 << 24) as f32 * 2.0 - 1.0
    }
}

impl VoiceEffect for Static {
    fn process(&mut self, frame: &mut [f32], channels: u8) {
        if self.level <= 0.0 {
            return;
        }
        let channels = channels.clamp(1, 2) as usize;
        let level = crate::audio::rms(frame);
        // Fast attack, ~100 ms release: the hiss rides the speech and tails off after it.
        self.envelope = level.max(self.envelope * 0.8);
        let gate = (self.envelope * 20.0).min(1.0);
        if gate <= 0.0 {
            return;
        }
        let gain = self.level * gate;
        for pos in frame.chunks_exact_mut(channels) {
            let n = self.noise() * gain;
            for s in pos.iter_mut() {
                *s += n;
            }
        }
    }

    fn reset(&mut self) {
        self.envelope = 0.0;
    }
}

/// Comb delays of the reverb at 48 kHz (Freeverb's 44.1 kHz tuning rescaled).
const REVERB_COMBS: [usize; 4] = [1215, 1293, 1390, 1476];
/// Series all-pass delays.
const REVERB_ALLPASSES: [usize; 2] = [605, 480];
/// Extra samples on the right channel's delays, for stereo width.
const REVERB_SPREAD: usize = 23;

#[derive(Debug, Clone)]
struct Comb {
    buf: Vec<f32>,
    idx: usize,
    store: f32,
}

#[derive(Debug, Clone)]
struct AllPass {
    buf: Vec<f32>,
    idx: usize,
}

/// Schroeder / Freeverb-style reverb: parallel damped feedback combs into series all-passes,
/// mixed with the dry voice. Per-channel networks (right one detuned) — a proper stereo tail
/// for stereo uplinks, plain mono otherwise.
#[derive(Debug, Clone)]
pub struct Reverb {
    size: f32,
    damping: f32,
    mix: f32,
    feedback: f32,
    damp: f32,
    combs: [Vec<Comb>; 2],
    allpasses: [Vec<AllPass>; 2],
}

impl Reverb {
    /// All three arguments are clamped to `0..=1`.
    pub fn new(size: f32, damping: f32, mix: f32) -> Self {
        let size = size.clamp(0.0, 1.0);
        let damping = damping.clamp(0.0, 1.0);
        let make = |spread: usize| {
            (
                REVERB_COMBS
                    .iter()
                    .map(|d| Comb {
                        buf: vec![0.0; d + spread],
                        idx: 0,
                        store: 0.0,
                    })
                    .collect::<Vec<_>>(),
                REVERB_ALLPASSES
                    .iter()
                    .map(|d| AllPass {
                        buf: vec![0.0; d + spread],
                        idx: 0,
                    })
                    .collect::<Vec<_>>(),
            )
        };
        let (cl, al) = make(0);
        let (cr, ar) = make(REVERB_SPREAD);
        Self {
            size,
            damping,
            mix: mix.clamp(0.0, 1.0),
            feedback: 0.7 + 0.28 * size,
            damp: damping * 0.4,
            combs: [cl, cr],
            allpasses: [al, ar],
        }
    }

    pub fn size(&self) -> f32 {
        self.size
    }

    pub fn damping(&self) -> f32 {
        self.damping
    }

    pub fn mix(&self) -> f32 {
        self.mix
    }

    fn tick(&mut self, ch: usize, input: f32) -> f32 {
        let mut out = 0.0;
        for comb in &mut self.combs[ch] {
            let y = comb.buf[comb.idx];
            comb.store = y * (1.0 - self.damp) + comb.store * self.damp;
            comb.buf[comb.idx] = input + comb.store * self.feedback;
            comb.idx = (comb.idx + 1) % comb.buf.len();
            out += y;
        }
        for ap in &mut self.allpasses[ch] {
            let buffered = ap.buf[ap.idx];
            ap.buf[ap.idx] = out + buffered * 0.5;
            ap.idx = (ap.idx + 1) % ap.buf.len();
            out = buffered - out;
        }
        out
    }
}

impl VoiceEffect for Reverb {
    fn process(&mut self, frame: &mut [f32], channels: u8) {
        if self.mix <= 0.0 {
            return;
        }
        let channels = channels.clamp(1, 2) as usize;
        let wet = self.mix * 0.1;
        let dry = 1.0 - self.mix * 0.5;
        for pos in frame.chunks_exact_mut(channels) {
            for (ch, s) in pos.iter_mut().enumerate() {
                let x = *s;
                let tail = self.tick(ch, x);
                *s = x * dry + tail * wet;
            }
        }
    }

    fn reset(&mut self) {
        for ch in 0..2 {
            for comb in &mut self.combs[ch] {
                comb.buf.fill(0.0);
                comb.idx = 0;
                comb.store = 0.0;
            }
            for ap in &mut self.allpasses[ch] {
                ap.buf.fill(0.0);
                ap.idx = 0;
            }
        }
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

    /// Energy of `pcm` inside `lo..hi` Hz relative to its total (Goertzel over 50 Hz steps).
    fn band_fraction(pcm: &[f32], lo: f32, hi: f32) -> f32 {
        let power = |hz: f32| {
            let w = 2.0 * PI * hz / SAMPLE_RATE as f32;
            let (mut s1, mut s2) = (0.0f32, 0.0f32);
            for &x in pcm {
                let s0 = x + 2.0 * w.cos() * s1 - s2;
                s2 = s1;
                s1 = s0;
            }
            s1 * s1 + s2 * s2 - 2.0 * w.cos() * s1 * s2
        };
        let mut total = 0.0;
        let mut band = 0.0;
        let mut hz = 50.0;
        while hz < 20_000.0 {
            let p = power(hz);
            total += p;
            if hz >= lo && hz < hi {
                band += p;
            }
            hz += 50.0;
        }
        band / total.max(1e-9)
    }

    /// A pulse train at `f0` Hz shaped by a resonance at `formant` Hz — a crude vowel.
    fn vowel(f0: f32, formant: f32, frames: usize) -> Vec<f32> {
        let period = (SAMPLE_RATE as f32 / f0) as usize;
        let mut pulses: Vec<f32> = (0..frames * FRAME_SAMPLES)
            .map(|n| if n % period == 0 { 1.0 } else { 0.0 })
            .collect();
        // Narrow resonance around `formant`, applied in place.
        let mut lp = Biquad::design(BiquadKind::LowPass, formant, 4.0);
        let mut hp = Biquad::design(BiquadKind::HighPass, formant, 4.0);
        for f in pulses.as_chunks_mut::<FRAME_SAMPLES>().0 {
            lp.process(f, 1);
            hp.process(f, 1);
        }
        let peak = pulses.iter().fold(0.0f32, |m, s| m.max(s.abs()));
        for s in &mut pulses {
            *s *= 0.5 / peak;
        }
        pulses
    }

    #[test]
    fn formant_shift_moves_the_envelope_but_keeps_the_pitch() {
        let input = vowel(150.0, 1000.0, 60);
        let mut fx = FormantShift::new(7.0); // ×1.5 → resonance to ~1500 Hz
        let out = run(&mut fx, &input, 1);
        let steady = &out[FRAME_SAMPLES * 10..];
        // Pitch: the pulse-train period survives (autocorrelation peak at 320 samples).
        let period = 320;
        let corr = |lag: usize| {
            steady
                .iter()
                .zip(&steady[lag..])
                .map(|(a, b)| a * b)
                .sum::<f32>()
        };
        let at_period = corr(period);
        let off_period = corr(period * 3 / 4);
        assert!(
            at_period > off_period * 2.0,
            "periodicity lost: {at_period} vs {off_period}"
        );
        // Envelope: energy moved from the 800–1200 band up towards 1300–1800.
        let before_hi = band_fraction(&input[FRAME_SAMPLES * 10..], 1300.0, 1800.0);
        let after_hi = band_fraction(steady, 1300.0, 1800.0);
        let before_lo = band_fraction(&input[FRAME_SAMPLES * 10..], 800.0, 1200.0);
        let after_lo = band_fraction(steady, 800.0, 1200.0);
        assert!(
            after_hi > before_hi * 1.5 && after_lo < before_lo * 0.8,
            "envelope did not move: hi {before_hi}→{after_hi}, lo {before_lo}→{after_lo}"
        );
        let level = crate::audio::rms(steady);
        assert!(level > 0.05, "level collapsed to {level}");
        assert_eq!(fx.latency(), FORMANT_LATENCY);
    }

    #[test]
    fn formant_shift_is_a_delay_at_zero_and_isolates_stereo_channels() {
        let mut fx = FormantShift::new(0.0);
        let mut frame = vec![0.3; frame_len(1)];
        fx.process(&mut frame, 1);
        assert!(frame.iter().all(|&s| s == 0.3));

        let mut fx = FormantShift::new(-5.0);
        let left = sine(200.0, 40);
        let mut stereo = Vec::with_capacity(left.len() * 2);
        for s in &left {
            stereo.push(*s);
            stereo.push(0.0);
        }
        let out = run(&mut fx, &stereo, 2);
        assert!(out.iter().skip(1).step_by(2).all(|&r| r == 0.0));
        assert!(out[FRAME_SAMPLES * 2 * 10..]
            .iter()
            .step_by(2)
            .any(|&l| l.abs() > 0.1));
        assert_eq!(FormantShift::new(40.0).semitones(), MAX_FORMANT_SEMITONES);
        fx.reset();
        let mut silence = vec![0.0; frame_len(2)];
        fx.process(&mut silence, 2);
        assert!(silence.iter().all(|&s| s == 0.0));
    }

    #[test]
    fn biquads_shape_the_spectrum() {
        let low = sine(200.0, 20);
        let high = sine(5000.0, 20);
        let mut mixed: Vec<f32> = low.iter().zip(&high).map(|(a, b)| a + b).collect();
        let before = band_fraction(&mixed[FRAME_SAMPLES * 5..], 4000.0, 6000.0);
        let mut lp = Biquad::lowpass(1000.0, 0.707);
        assert!(lp.is_lowpass() && lp.corner_hz() == 1000.0);
        for f in mixed.as_chunks_mut::<FRAME_SAMPLES>().0 {
            lp.process(f, 1);
        }
        let after = band_fraction(&mixed[FRAME_SAMPLES * 5..], 4000.0, 6000.0);
        assert!(
            after < before * 0.05,
            "low-pass left {after} (was {before})"
        );

        let mut mixed: Vec<f32> = low.iter().zip(&high).map(|(a, b)| a + b).collect();
        let before = band_fraction(&mixed[FRAME_SAMPLES * 5..], 100.0, 300.0);
        let mut hp = Biquad::highpass(1000.0, 0.707);
        for f in mixed.as_chunks_mut::<FRAME_SAMPLES>().0 {
            hp.process(f, 1);
        }
        let after = band_fraction(&mixed[FRAME_SAMPLES * 5..], 100.0, 300.0);
        assert!(
            after < before * 0.05,
            "high-pass left {after} (was {before})"
        );
        assert_eq!(Biquad::lowpass(1.0, 0.707).corner_hz(), MIN_FILTER_HZ);
    }

    #[test]
    fn distortion_saturates_and_keeps_full_scale() {
        let mut fx = Distortion::new(5.0);
        let mut frame = vec![1.0; FRAME_SAMPLES];
        fx.process(&mut frame, 1);
        assert!((frame[0] - 1.0).abs() < 1e-4);
        let out = run(&mut Distortion::new(5.0), &sine(440.0, 5), 1);
        // Sine squashed towards a square: peak/RMS ratio drops below √2.
        let peak = out.iter().fold(0.0f32, |m, s| m.max(s.abs()));
        let crest = peak / crate::audio::rms(&out);
        assert!(crest < 1.3, "crest {crest}");
        assert_eq!(Distortion::new(0.1).drive(), 1.0);
        assert_eq!(Distortion::new(1e9).drive(), MAX_DISTORTION_DRIVE);
    }

    #[test]
    fn tremolo_dips_the_level_periodically() {
        let mut fx = Tremolo::new(10.0, 1.0);
        let out = run(&mut fx, &vec![0.5; FRAME_SAMPLES * 6], 1);
        // 10 Hz = 4800 samples per cycle: full at 0, silent at 2400.
        assert!((out[0] - 0.5).abs() < 1e-3);
        assert!(out[2400].abs() < 1e-3, "no dip: {}", out[2400]);
        assert!((out[4800] - 0.5).abs() < 1e-3);
        let mut fx = Tremolo::new(0.0, 1.0);
        let mut frame = vec![0.5; FRAME_SAMPLES];
        fx.process(&mut frame, 1);
        assert!(frame.iter().all(|&s| s == 0.5));
        assert_eq!(Tremolo::new(99.0, 5.0).rate_hz(), MAX_TREMOLO_HZ);
        assert_eq!(Tremolo::new(1.0, 5.0).depth(), 1.0);
    }

    #[test]
    fn static_rides_the_voice_and_leaves_silence_alone() {
        let mut fx = Static::new(0.1);
        let mut silence = vec![0.0; FRAME_SAMPLES];
        fx.process(&mut silence, 1);
        assert!(silence.iter().all(|&s| s == 0.0));
        let voiced = run(&mut fx, &sine(440.0, 5), 1);
        let residual: Vec<f32> = voiced
            .iter()
            .zip(&sine(440.0, 5))
            .map(|(a, b)| a - b)
            .collect();
        let hiss = crate::audio::rms(&residual[FRAME_SAMPLES..]);
        assert!((0.03..0.08).contains(&hiss), "hiss {hiss}");
        // Stereo: the same noise sample on both channels (no phasey width).
        let mut fx = Static::new(0.5);
        let mut frame = vec![0.5; frame_len(2)];
        fx.process(&mut frame, 2);
        assert!(frame.as_chunks::<2>().0.iter().all(|p| p[0] == p[1]));
        assert_eq!(Static::new(7.0).level(), 1.0);
    }

    #[test]
    fn reverb_adds_a_decaying_tail() {
        let mut fx = Reverb::new(0.8, 0.2, 1.0);
        let mut impulse = vec![0.0; FRAME_SAMPLES * 60];
        impulse[0] = 1.0;
        let out = run(&mut fx, &impulse, 1);
        let frame_rms =
            |i: usize| crate::audio::rms(&out[i * FRAME_SAMPLES..(i + 1) * FRAME_SAMPLES]);
        // Tail present after the first comb delay (~25 ms) and decaying over half a second.
        assert!(frame_rms(2) > 1e-4, "no tail: {}", frame_rms(2));
        assert!(frame_rms(30) < frame_rms(2), "not decaying");
        assert!(frame_rms(59) < frame_rms(2) * 0.2, "decay too slow");
        // Dry passes through when the mix is off; stereo channels get different tails.
        let mut off = Reverb::new(0.8, 0.2, 0.0);
        let mut frame = vec![0.25; FRAME_SAMPLES];
        off.process(&mut frame, 1);
        assert!(frame.iter().all(|&s| s == 0.25));
        let mut fx = Reverb::new(0.5, 0.5, 0.5);
        let mut stereo = vec![0.0; frame_len(2) * 10];
        stereo[0] = 1.0;
        stereo[1] = 1.0;
        for f in stereo.as_chunks_mut::<{ frame_len(2) }>().0 {
            fx.process(f, 2);
        }
        let tail = &stereo[frame_len(2) * 3..];
        assert!(tail
            .as_chunks::<2>()
            .0
            .iter()
            .any(|p| (p[0] - p[1]).abs() > 1e-5));
        assert_eq!((fx.size(), fx.damping(), fx.mix()), (0.5, 0.5, 0.5));
        fx.reset();
        let mut silence = vec![0.0; frame_len(2)];
        fx.process(&mut silence, 2);
        assert!(silence.iter().all(|&s| s == 0.0));
    }

    #[test]
    fn params_sanitize_and_build_in_order() {
        assert!(VoiceEffectParams::default().is_bypass());
        assert!(VoiceEffectParams::default().chain().is_empty());
        let wild = VoiceEffectParams {
            highpass_hz: -5.0,
            lowpass_hz: 1e9,
            formant_semitones: f32::NAN,
            pitch_semitones: 100.0,
            ring_mod_hz: -1.0,
            distortion_drive: 0.5,
            tremolo_hz: 50.0,
            tremolo_depth: 3.0,
            static_level: -1.0,
            reverb_mix: 2.0,
            reverb_size: 2.0,
            reverb_damping: -2.0,
        };
        let s = wild.sanitized();
        assert_eq!(s.highpass_hz, 0.0);
        assert_eq!(s.lowpass_hz, MAX_FILTER_HZ);
        assert_eq!(s.formant_semitones, 0.0);
        assert_eq!(s.pitch_semitones, MAX_PITCH_SEMITONES);
        assert_eq!(s.ring_mod_hz, 0.0);
        assert_eq!(s.distortion_drive, 1.0);
        assert_eq!(s.tremolo_hz, MAX_TREMOLO_HZ);
        assert_eq!(s.tremolo_depth, 1.0);
        assert_eq!(s.static_level, 0.0);
        assert_eq!(
            (s.reverb_mix, s.reverb_size, s.reverb_damping),
            (1.0, 1.0, 0.0)
        );
        let all = VoiceEffectParams {
            highpass_hz: 100.0,
            lowpass_hz: 8000.0,
            formant_semitones: 1.0,
            pitch_semitones: 1.0,
            ring_mod_hz: 30.0,
            distortion_drive: 2.0,
            tremolo_hz: 3.0,
            tremolo_depth: 0.5,
            static_level: 0.1,
            reverb_mix: 0.3,
            reverb_size: 0.5,
            reverb_damping: 0.5,
        };
        assert_eq!(all.chain().len(), 9);
        // Tremolo without depth is not a stage.
        let no_depth = VoiceEffectParams {
            tremolo_depth: 0.0,
            ..all
        };
        assert_eq!(no_depth.chain().len(), 8);
    }

    #[test]
    fn presets_run_on_speech_like_input_in_realtime_budget() {
        let input = vowel(120.0, 800.0, 25);
        for preset in EffectPreset::ALL {
            assert_eq!(preset.name().parse::<EffectPreset>(), Ok(preset));
            assert!(!preset.params().is_bypass(), "{preset:?} is a bypass");
            for channels in [1u8, 2] {
                let mut chain = preset.chain();
                let mut pcm: Vec<f32> = if channels == 1 {
                    input.clone()
                } else {
                    input.iter().flat_map(|s| [*s, *s]).collect()
                };
                let start = std::time::Instant::now();
                for f in pcm.chunks_exact_mut(frame_len(channels)) {
                    chain.process(f, channels);
                }
                let elapsed = start.elapsed();
                assert!(
                    pcm.iter().all(|s| s.is_finite() && s.abs() <= 1.0),
                    "{preset:?} produced out-of-range samples"
                );
                let level = crate::audio::rms(&pcm[pcm.len() / 2..]);
                assert!(
                    level > 0.01,
                    "{preset:?} ({channels}ch) went silent: {level}"
                );
                // 25 frames = 500 ms of audio. A smoke check against pathological cost, not a
                // benchmark: debug builds on shared CI runners (macOS x64 has been seen at
                // 2.3× real time) need the 4× headroom.
                assert!(
                    elapsed < std::time::Duration::from_millis(2000),
                    "{preset:?} ({channels}ch) took {elapsed:?} for 500 ms of audio"
                );
                chain.reset();
            }
        }
        assert!("nope".parse::<EffectPreset>().is_err());
    }
}
