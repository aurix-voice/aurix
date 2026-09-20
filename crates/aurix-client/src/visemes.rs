//! Lip-sync from audio, locally. Every decoded 20 ms frame of a participant (or of our own
//! processed microphone) is reduced to a compact mouth state — a weight per [`Viseme`]
//! bucket plus openness, level and confidence — for the game to drive blend shapes or a
//! sprite sheet with. The analysis runs on the receiver over audio it plays anyway: nothing
//! is sent to the server, no phoneme data crosses the wire, and E2EE audio works because it
//! is decrypted here.
//!
//! This is a signal-processing heuristic, not a phoneme recogniser: a 1024-point spectrum per
//! frame gives level, a voicing / friction split (zero-crossing rate and high-band ratio) and
//! the first two formant peaks, and the vowels are the nearest of five formant centroids
//! (`AA E IH OH OU`); fricatives land on `SS` (sibilant) or `FF` (soft), a quiet voiced
//! low-frequency hum on `PP` (lips closed / nasal). Weights are smoothed with a fast attack
//! and slower release so the mouth does not flicker at frame rate.

use std::sync::Arc;

use realfft::num_complex::Complex;
use realfft::{RealFftPlanner, RealToComplex};

use crate::audio::{FRAME_SAMPLES, SAMPLE_RATE};

/// Mouth-shape buckets, ordered as in [`VisemeFrame::weights`]. `PP`/`FF`/`SS` follow the
/// common lip-sync naming (bilabial closure, labiodental, sibilant); the rest are vowels.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[repr(u8)]
pub enum Viseme {
    /// Mouth closed, no speech.
    Silence = 0,
    /// Lips together: p / b / m, and nasal humming.
    PP = 1,
    /// Soft friction: f / v / th.
    FF = 2,
    /// Sibilant: s / z / sh.
    SS = 3,
    /// Open vowel: "father".
    AA = 4,
    /// Mid front vowel: "bed".
    E = 5,
    /// Close front vowel: "see" / "sit".
    IH = 6,
    /// Mid back rounded vowel: "law" / "go".
    OH = 7,
    /// Close back rounded vowel: "boot".
    OU = 8,
}

/// Number of [`Viseme`] buckets.
pub const VISEME_COUNT: usize = 9;

impl Viseme {
    pub const ALL: [Viseme; VISEME_COUNT] = [
        Viseme::Silence,
        Viseme::PP,
        Viseme::FF,
        Viseme::SS,
        Viseme::AA,
        Viseme::E,
        Viseme::IH,
        Viseme::OH,
        Viseme::OU,
    ];

    pub fn index(self) -> usize {
        self as usize
    }

    pub fn from_index(i: usize) -> Option<Viseme> {
        Viseme::ALL.get(i).copied()
    }

    pub fn name(self) -> &'static str {
        match self {
            Viseme::Silence => "sil",
            Viseme::PP => "PP",
            Viseme::FF => "FF",
            Viseme::SS => "SS",
            Viseme::AA => "aa",
            Viseme::E => "E",
            Viseme::IH => "ih",
            Viseme::OH => "oh",
            Viseme::OU => "ou",
        }
    }

    /// How wide the jaw is for this shape at full level, `0..=1`.
    fn openness(self) -> f32 {
        match self {
            Viseme::Silence | Viseme::PP => 0.0,
            Viseme::FF => 0.2,
            Viseme::SS => 0.25,
            Viseme::AA => 1.0,
            Viseme::E => 0.65,
            Viseme::IH => 0.4,
            Viseme::OH => 0.6,
            Viseme::OU => 0.3,
        }
    }
}

/// Mouth state derived from the latest analysed frame.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct VisemeFrame {
    /// Smoothed weight per [`Viseme`] (index = `Viseme::index`), summing to ~1.
    pub weights: [f32; VISEME_COUNT],
    /// The heaviest bucket.
    pub dominant: Viseme,
    /// Jaw openness `0..=1`: level × the dominant shape's openness, smoothed.
    pub mouth_open: f32,
    /// RMS level of the frame, `0..=1`.
    pub energy: f32,
    /// How clear-cut the classification is, `0..=1` (margin between the top two buckets).
    pub confidence: f32,
    /// Frames analysed so far; unchanged between two reads means no new audio arrived.
    pub sequence: u64,
}

impl Default for VisemeFrame {
    fn default() -> Self {
        let mut weights = [0.0; VISEME_COUNT];
        weights[Viseme::Silence.index()] = 1.0;
        Self {
            weights,
            dominant: Viseme::Silence,
            mouth_open: 0.0,
            energy: 0.0,
            confidence: 1.0,
            sequence: 0,
        }
    }
}

/// FFT size (the 960-sample frame is zero-padded).
const FFT_SIZE: usize = 1024;
const BINS: usize = FFT_SIZE / 2 + 1;
const BIN_HZ: f32 = SAMPLE_RATE as f32 / FFT_SIZE as f32;
/// Below this RMS (about −54 dBFS) a frame is silence regardless of the noise floor.
const SILENCE_RMS: f32 = 0.002;
/// Level at which the mouth is considered fully open (about −20 dBFS).
const FULL_OPEN_RMS: f32 = 0.1;
/// Smoothing: weights reach ~63 % of a new target in one frame going up, three going down.
const ATTACK: f32 = 0.65;
/// First-order pre-emphasis coefficient for the formant spectrum.
const PRE_EMPHASIS: f32 = 0.97;
/// Half-width of the spectral smoothing in bins (~330 Hz total: merges harmonics up to a
/// child's pitch into an envelope, keeps F1 and F2 apart).
const SMOOTH_BINS: usize = 3;
const RELEASE: f32 = 0.3;

/// Vowel formant centroids (F1, F2) in Hz — adult averages, wide enough for most voices.
const VOWELS: [(Viseme, f32, f32); 5] = [
    (Viseme::AA, 750.0, 1250.0),
    (Viseme::E, 520.0, 1900.0),
    (Viseme::IH, 330.0, 2350.0),
    (Viseme::OH, 520.0, 900.0),
    (Viseme::OU, 330.0, 780.0),
];

fn bin(hz: f32) -> usize {
    ((hz / BIN_HZ) as usize).min(BINS - 1)
}

fn peak(smooth: &[f32], lo: usize, hi: usize) -> (usize, f32) {
    let mut best = (lo, 0.0f32);
    for (i, &p) in smooth.iter().enumerate().take(hi).skip(lo) {
        if p > best.1 {
            best = (i, p);
        }
    }
    best
}

/// `(F1 Hz, F1 power, F2 Hz, F2 power)` from a smoothed pre-emphasised spectrum: F1 is the
/// strongest peak in 200–1000 Hz, F2 the strongest one above the valley that follows F1.
fn formants(smooth: &[f32]) -> (f32, f32, f32, f32) {
    let (i1, p1) = peak(smooth, bin(200.0), bin(1000.0) + 1);
    let mut valley = i1 + 1;
    let f2_end = bin(3200.0) + 1;
    while valley + 1 < f2_end && smooth[valley + 1] <= smooth[valley] {
        valley += 1;
    }
    let f2_start = valley.max(bin(700.0));
    let (i2, p2) = peak(smooth, f2_start, f2_end);
    (i1 as f32 * BIN_HZ, p1, i2 as f32 * BIN_HZ, p2)
}

/// Per-stream lip-sync analyser; feed it every decoded frame, read [`Self::frame`] at will.
pub struct VisemeAnalyzer {
    fft: Arc<dyn RealToComplex<f32>>,
    window: Vec<f32>,
    input: Vec<f32>,
    spectrum: Vec<Complex<f32>>,
    scratch: Vec<Complex<f32>>,
    power: Vec<f32>,
    /// Undoes the pre-emphasis per bin, for band ratios of the voice as heard.
    deemphasis: Vec<f32>,
    smooth: Vec<f32>,
    noise_floor: f32,
    current: VisemeFrame,
}

impl std::fmt::Debug for VisemeAnalyzer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VisemeAnalyzer")
            .field("current", &self.current)
            .finish()
    }
}

impl Default for VisemeAnalyzer {
    fn default() -> Self {
        Self::new()
    }
}

impl VisemeAnalyzer {
    /// Allocates every buffer up front; [`Self::push`] never allocates.
    pub fn new() -> Self {
        let mut planner = RealFftPlanner::<f32>::new();
        let fft = planner.plan_fft_forward(FFT_SIZE);
        let window = (0..FRAME_SAMPLES)
            .map(|n| {
                0.5 - 0.5 * (2.0 * std::f32::consts::PI * n as f32 / FRAME_SAMPLES as f32).cos()
            })
            .collect();
        let scratch = fft.make_scratch_vec();
        let spectrum = fft.make_output_vec();
        let deemphasis = (0..BINS)
            .map(|i| {
                let w = 2.0 * std::f32::consts::PI * i as f32 / FFT_SIZE as f32;
                1.0 / (1.0 + PRE_EMPHASIS * PRE_EMPHASIS - 2.0 * PRE_EMPHASIS * w.cos())
            })
            .collect();
        Self {
            fft,
            window,
            input: vec![0.0; FFT_SIZE],
            spectrum,
            scratch,
            power: vec![0.0; BINS],
            deemphasis,
            smooth: vec![0.0; BINS],
            noise_floor: SILENCE_RMS,
            current: VisemeFrame::default(),
        }
    }

    /// Latest mouth state.
    pub fn frame(&self) -> VisemeFrame {
        self.current
    }

    /// Forget smoothing history (a new talk spurt after a long gap starts clean).
    pub fn reset(&mut self) {
        let sequence = self.current.sequence;
        self.current = VisemeFrame {
            sequence,
            ..VisemeFrame::default()
        };
        self.noise_floor = SILENCE_RMS;
    }

    /// Analyse one 20 ms frame of interleaved 48 kHz PCM with `channels` (downmixed to mono).
    /// Shorter frames are zero-padded, longer ones truncated.
    pub fn push(&mut self, pcm: &[f32], channels: u8) {
        let channels = channels.clamp(1, 8) as usize;
        self.input.fill(0.0);
        let mut energy = 0.0f32;
        let mut crossings = 0u32;
        let mut prev = 0.0f32;
        let n = (pcm.len() / channels).min(FRAME_SAMPLES);
        for (i, pos) in pcm.chunks_exact(channels).take(n).enumerate() {
            let s = pos.iter().sum::<f32>() / channels as f32;
            energy += s * s;
            if i > 0 && (s < 0.0) != (prev < 0.0) {
                crossings += 1;
            }
            // Pre-emphasis flattens the voice's spectral tilt so F2 is not buried under F1.
            self.input[i] = (s - PRE_EMPHASIS * prev) * self.window[i];
            prev = s;
        }
        let rms = if n > 0 {
            (energy / n as f32).sqrt()
        } else {
            0.0
        };
        let zcr = if n > 1 {
            crossings as f32 / (n - 1) as f32
        } else {
            0.0
        };
        let target = self.classify(rms, zcr);
        self.smooth_into(target, rms);
    }

    /// Raw (unsmoothed) weights for the frame currently in `input`.
    fn classify(&mut self, rms: f32, zcr: f32) -> [f32; VISEME_COUNT] {
        let mut w = [0.0f32; VISEME_COUNT];
        // Noise floor: slow upward drift, instant drop — speech pauses reveal it.
        if rms < self.noise_floor {
            self.noise_floor = rms.max(SILENCE_RMS * 0.25);
        } else {
            self.noise_floor += (rms - self.noise_floor) * 0.002;
        }
        if rms < SILENCE_RMS || rms < self.noise_floor * 3.0 {
            w[Viseme::Silence.index()] = 1.0;
            return w;
        }

        if self
            .fft
            .process_with_scratch(&mut self.input, &mut self.spectrum, &mut self.scratch)
            .is_err()
        {
            w[Viseme::Silence.index()] = 1.0;
            return w;
        }
        let mut total = 0.0f32;
        for ((p, c), d) in self
            .power
            .iter_mut()
            .zip(&self.spectrum)
            .zip(&self.deemphasis)
        {
            *p = c.norm_sqr();
            total += *p * d;
        }
        if total <= 0.0 {
            w[Viseme::Silence.index()] = 1.0;
            return w;
        }
        let band = |lo: f32, hi: f32| -> f32 {
            let a = bin(lo);
            let b = bin(hi) + 1;
            self.power[a.min(b)..b]
                .iter()
                .zip(&self.deemphasis[a.min(b)..b])
                .map(|(p, d)| p * d)
                .sum::<f32>()
                / total
        };
        let high = band(4000.0, SAMPLE_RATE as f32 / 2.0);
        let mid_high = band(2000.0, 4000.0);
        let low = band(0.0, 500.0);

        // Friction: energy lives above 2 kHz and the waveform crosses zero like noise.
        let friction = (high * 2.0 + mid_high).min(1.0);
        if zcr > 0.16 || high > 0.45 {
            // Sibilants are bright and loud, soft fricatives dim and duller.
            let sibilant = ((high - 0.3) / 0.4).clamp(0.0, 1.0);
            w[Viseme::SS.index()] = sibilant;
            w[Viseme::FF.index()] = 1.0 - sibilant;
            return w;
        }

        // Voiced: smooth the spectrum and pick F1 / F2 peaks.
        for i in 0..BINS {
            let lo = i.saturating_sub(SMOOTH_BINS);
            let hi = (i + SMOOTH_BINS + 1).min(BINS);
            self.smooth[i] = self.power[lo..hi].iter().sum::<f32>() / (hi - lo) as f32;
        }
        let (f1, f1_power, f2, f2_power) = formants(&self.smooth);
        let f2_ratio = f2_power / f1_power.max(1e-12);

        // Closed lips / nasal: quiet, everything below 500 Hz, no second formant to speak of.
        let hum = low > 0.85 && f2_ratio < 0.02 && friction < 0.05;
        if hum {
            w[Viseme::PP.index()] = 1.0;
            return w;
        }

        // Nearest vowel centroid in log-frequency, soft-assigned.
        let mut sum = 0.0;
        for (v, c1, c2) in VOWELS {
            let d1 = (f1 / c1).ln();
            let d2 = (f2 / c2).ln();
            let d = d1 * d1 * 6.0 + d2 * d2 * 6.0;
            let weight = (-d).exp();
            w[v.index()] = weight;
            sum += weight;
        }
        if sum > 0.0 {
            for v in &mut w {
                *v /= sum;
            }
        }
        // A bit of friction on a vowel (voiced fricatives, breathy vowels) leaks into FF.
        if friction > 0.15 {
            let leak = ((friction - 0.15) / 0.5).min(0.5);
            for v in &mut w {
                *v *= 1.0 - leak;
            }
            w[Viseme::FF.index()] += leak;
        }
        w
    }

    fn smooth_into(&mut self, target: [f32; VISEME_COUNT], rms: f32) {
        let cur = &mut self.current;
        let mut sum = 0.0f32;
        for (w, t) in cur.weights.iter_mut().zip(target) {
            let alpha = if t > *w { ATTACK } else { RELEASE };
            *w += (t - *w) * alpha;
            sum += *w;
        }
        let mut best = (Viseme::Silence, 0.0f32);
        let mut second = 0.0f32;
        for (i, w) in cur.weights.iter_mut().enumerate() {
            if sum > 0.0 {
                *w /= sum;
            }
            if *w > best.1 {
                second = best.1;
                best = (Viseme::from_index(i).unwrap_or(Viseme::Silence), *w);
            } else if *w > second {
                second = *w;
            }
        }
        cur.dominant = best.0;
        cur.energy = rms.min(1.0);
        cur.confidence = (best.1 - second).clamp(0.0, 1.0);
        let level = (rms / FULL_OPEN_RMS).min(1.0).sqrt();
        let openness: f32 = Viseme::ALL
            .iter()
            .map(|v| cur.weights[v.index()] * v.openness())
            .sum();
        let target_open = (level * openness).clamp(0.0, 1.0);
        let alpha = if target_open > cur.mouth_open {
            ATTACK
        } else {
            RELEASE
        };
        cur.mouth_open += (target_open - cur.mouth_open) * alpha;
        cur.sequence += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Pulse train at `f0` through two resonators — a synthetic vowel.
    fn vowel(f0: f32, f1: f32, f2: f32, frames: usize) -> Vec<f32> {
        let period = (SAMPLE_RATE as f32 / f0) as usize;
        let len = frames * FRAME_SAMPLES;
        let mut out = vec![0.0f32; len];
        for (formant, gain) in [(f1, 1.0f32), (f2, 0.5)] {
            // Two-pole resonator, bandwidth ~80 Hz.
            let r = (-std::f32::consts::PI * 80.0 / SAMPLE_RATE as f32).exp();
            let theta = 2.0 * std::f32::consts::PI * formant / SAMPLE_RATE as f32;
            let a1 = -2.0 * r * theta.cos();
            let a2 = r * r;
            let (mut y1, mut y2) = (0.0f32, 0.0f32);
            for (n, o) in out.iter_mut().enumerate().take(len) {
                let x = if n % period == 0 { 1.0 } else { 0.0 };
                let y = x - a1 * y1 - a2 * y2;
                y2 = y1;
                y1 = y;
                *o += y * gain;
            }
        }
        let peak = out.iter().fold(0.0f32, |m, s| m.max(s.abs()));
        for s in &mut out {
            *s *= 0.4 / peak;
        }
        out
    }

    fn analyse(pcm: &[f32]) -> VisemeFrame {
        let mut a = VisemeAnalyzer::new();
        for f in pcm.as_chunks::<FRAME_SAMPLES>().0 {
            a.push(f, 1);
        }
        a.frame()
    }

    fn noise(seed: &mut u32, len: usize, amp: f32) -> Vec<f32> {
        (0..len)
            .map(|_| {
                *seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                ((*seed >> 8) as f32 / (1u32 << 24) as f32 * 2.0 - 1.0) * amp
            })
            .collect()
    }

    #[test]
    fn silence_is_closed_and_default_is_silence() {
        let d = VisemeFrame::default();
        assert_eq!(d.dominant, Viseme::Silence);
        assert_eq!(d.mouth_open, 0.0);
        let f = analyse(&vec![0.0; FRAME_SAMPLES * 5]);
        assert_eq!(f.dominant, Viseme::Silence);
        assert!(f.mouth_open < 0.01);
        assert_eq!(f.sequence, 5);
        // Faint hiss under the floor is silence too.
        let mut seed = 7;
        let f = analyse(&noise(&mut seed, FRAME_SAMPLES * 5, 0.001));
        assert_eq!(f.dominant, Viseme::Silence);
    }

    #[test]
    fn vowels_land_on_their_formant_buckets() {
        for (expect, f1, f2) in [
            (Viseme::AA, 750.0, 1250.0),
            (Viseme::E, 520.0, 1900.0),
            (Viseme::IH, 330.0, 2350.0),
            (Viseme::OH, 520.0, 900.0),
            (Viseme::OU, 330.0, 780.0),
        ] {
            let f = analyse(&vowel(130.0, f1, f2, 12));
            assert_eq!(f.dominant, expect, "{f1}/{f2} Hz → {:?}", f.weights);
            assert!(f.confidence > 0.2, "{expect:?} confidence {}", f.confidence);
            assert!(f.mouth_open > 0.15, "{expect:?} mouth {}", f.mouth_open);
            let sum: f32 = f.weights.iter().sum();
            assert!((sum - 1.0).abs() < 0.05, "weights sum {sum}");
        }
        // A higher voice (female / child) still classifies by formants, not pitch.
        let f = analyse(&vowel(220.0, 750.0, 1250.0, 12));
        assert_eq!(f.dominant, Viseme::AA);
        // Open vowels open the mouth wider than closed ones.
        let aa = analyse(&vowel(130.0, 750.0, 1250.0, 12)).mouth_open;
        let ou = analyse(&vowel(130.0, 330.0, 780.0, 12)).mouth_open;
        assert!(aa > ou * 1.5, "aa {aa} vs ou {ou}");
    }

    #[test]
    fn fricatives_and_hums_have_their_own_buckets() {
        let mut seed = 1;
        // Bright noise = sibilant.
        let hiss = noise(&mut seed, FRAME_SAMPLES * 6, 0.2);
        let mut hp = crate::effects::Biquad::highpass(4000.0, 0.707);
        let mut bright = hiss.clone();
        for f in bright.as_chunks_mut::<FRAME_SAMPLES>().0 {
            crate::effects::VoiceEffect::process(&mut hp, f, 1);
        }
        let f = analyse(&bright);
        assert_eq!(f.dominant, Viseme::SS, "{:?}", f.weights);
        // Softer, duller noise = FF.
        let mut lp = crate::effects::Biquad::lowpass(3000.0, 0.707);
        let mut hp = crate::effects::Biquad::highpass(1500.0, 0.707);
        let mut dull = noise(&mut seed, FRAME_SAMPLES * 6, 0.05);
        for f in dull.as_chunks_mut::<FRAME_SAMPLES>().0 {
            crate::effects::VoiceEffect::process(&mut lp, f, 1);
            crate::effects::VoiceEffect::process(&mut hp, f, 1);
        }
        let f = analyse(&dull);
        assert!(
            matches!(f.dominant, Viseme::FF | Viseme::SS),
            "{:?}",
            f.weights
        );
        assert!(f.weights[Viseme::FF.index()] > 0.3, "{:?}", f.weights);
        // A quiet low hum = closed lips.
        let hum: Vec<f32> = (0..FRAME_SAMPLES * 6)
            .map(|n| {
                (2.0 * std::f32::consts::PI * 140.0 * n as f32 / SAMPLE_RATE as f32).sin() * 0.05
            })
            .collect();
        let f = analyse(&hum);
        assert_eq!(f.dominant, Viseme::PP, "{:?}", f.weights);
        assert!(f.mouth_open < 0.05);
    }

    #[test]
    fn weights_smooth_and_release_and_reset_clears() {
        let mut a = VisemeAnalyzer::new();
        let aa = vowel(130.0, 750.0, 1250.0, 6);
        for f in aa.as_chunks::<FRAME_SAMPLES>().0 {
            a.push(f, 1);
        }
        let open = a.frame().mouth_open;
        a.push(&vec![0.0; FRAME_SAMPLES], 1);
        let after_one = a.frame();
        // One silent frame does not slam the mouth shut …
        assert!(after_one.mouth_open > open * 0.4 && after_one.mouth_open < open);
        for _ in 0..15 {
            a.push(&vec![0.0; FRAME_SAMPLES], 1);
        }
        // … fifteen do.
        assert_eq!(a.frame().dominant, Viseme::Silence);
        assert!(a.frame().mouth_open < 0.02);
        for f in aa.as_chunks::<FRAME_SAMPLES>().0.iter().take(2) {
            a.push(f, 1);
        }
        assert!(a.frame().mouth_open > 0.1);
        let seq = a.frame().sequence;
        a.reset();
        assert_eq!(a.frame().dominant, Viseme::Silence);
        assert_eq!(a.frame().sequence, seq);
        // Stereo input is downmixed; a short frame is zero-padded, not rejected.
        let stereo: Vec<f32> = aa[..FRAME_SAMPLES].iter().flat_map(|s| [*s, *s]).collect();
        a.push(&stereo, 2);
        a.push(&aa[..FRAME_SAMPLES / 2], 1);
        assert_eq!(a.frame().sequence, seq + 2);
        assert_eq!(Viseme::from_index(VISEME_COUNT), None);
        assert_eq!(Viseme::from_index(4), Some(Viseme::AA));
        assert_eq!(Viseme::AA.name(), "aa");
    }
}
