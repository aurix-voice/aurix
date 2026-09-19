//! Capture-side DSP: high-pass filter, acoustic echo cancellation, noise suppression and
//! automatic gain control, applied to the mono 48 kHz capture stream before input gain, VAD
//! and encoding.
//!
//! Everything here is pure Rust (no system libraries) so it builds wherever the client core
//! builds: RNNoise-derived neural noise suppression (`nnnoiseless`), a partitioned-block
//! frequency-domain adaptive filter (multi-delay filter, as in Speex) with a spectral
//! residual-echo suppressor and an energy-envelope delay estimator for the AEC, and a
//! speech-gated RMS AGC with a soft limiter. The pipeline works on 10 ms blocks (480 samples)
//! and adds one block of algorithmic latency (the residual-echo suppressor's overlap-add).
//!
//! The AEC needs to know what the device is playing. [`crate::Client::mix_output_f32`] /
//! `mix_output_i16` feed the mixed downlink automatically; hosts that play Aurix audio through
//! their own path (or want game audio cancelled too) push it with
//! [`crate::Client::push_render_f32`].

use std::collections::VecDeque;
use std::sync::Arc;

use nnnoiseless::DenoiseState;
use parking_lot::Mutex;
use realfft::num_complex::Complex;
use realfft::{ComplexToReal, RealFftPlanner, RealToComplex};
use serde::{Deserialize, Serialize};

#[cfg(test)]
use crate::audio::FRAME_SAMPLES;
use crate::audio::SAMPLE_RATE;

/// DSP block size in samples (10 ms at 48 kHz); a 20 ms capture frame is two blocks.
pub const BLOCK: usize = 480;
const FFT_SIZE: usize = 2 * BLOCK;
const BINS: usize = BLOCK + 1;
const BLOCK_MS: u32 = (BLOCK as u32 * 1000) / SAMPLE_RATE;

/// Longest echo tail the adaptive filter can model.
pub const MAX_ECHO_TAIL_MS: u32 = 500;
/// Shortest echo tail (a single partition beyond the minimum makes adaptation meaningful).
pub const MIN_ECHO_TAIL_MS: u32 = 40;
/// Largest playback-to-capture delay hint / estimate the far-end queue will hold.
pub const MAX_STREAM_DELAY_MS: u32 = 500;
/// Extra alignment slack the far-end queue tolerates before it re-aligns (in blocks).
const FAR_SLACK_BLOCKS: usize = 4;

/// How much of the RNNoise output is used (dry/wet blend); `High` is the raw network output.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NoiseSuppression {
    Off,
    Low,
    Moderate,
    #[default]
    High,
}

impl NoiseSuppression {
    fn wet(self) -> f32 {
        match self {
            NoiseSuppression::Off => 0.0,
            NoiseSuppression::Low => 0.5,
            NoiseSuppression::Moderate => 0.75,
            NoiseSuppression::High => 1.0,
        }
    }

    /// Stable integer form for the C ABI (`0` off … `3` high).
    pub fn as_u8(self) -> u8 {
        match self {
            NoiseSuppression::Off => 0,
            NoiseSuppression::Low => 1,
            NoiseSuppression::Moderate => 2,
            NoiseSuppression::High => 3,
        }
    }

    pub fn from_u8(v: u8) -> Self {
        match v {
            0 => NoiseSuppression::Off,
            1 => NoiseSuppression::Low,
            2 => NoiseSuppression::Moderate,
            _ => NoiseSuppression::High,
        }
    }
}

/// Capture DSP configuration. Every stage can be switched off individually; the defaults are
/// what a game client on a laptop/phone wants (everything on, 200 ms echo tail).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct DspConfig {
    /// 80 Hz second-order high-pass (removes DC, rumble and handling noise).
    pub high_pass: bool,
    /// Acoustic echo cancellation against the rendered output.
    pub echo_cancellation: bool,
    /// Echo tail the adaptive filter models, `40..=500` ms (in 10 ms partitions).
    pub echo_tail_ms: u32,
    /// Initial playback→capture delay hint, `0..=500` ms. The delay estimator refines it at
    /// runtime; leave `0` unless the platform's buffering is known.
    pub stream_delay_ms: u32,
    pub noise_suppression: NoiseSuppression,
    /// Automatic gain control (speech-gated RMS levelling with a soft limiter).
    pub agc: bool,
    /// AGC target speech level in dBFS RMS, `-30..=-6`.
    pub agc_target_dbfs: f32,
    /// Maximum AGC boost in dB, `0..=40`.
    pub agc_max_gain_db: f32,
}

impl Default for DspConfig {
    fn default() -> Self {
        Self {
            high_pass: true,
            echo_cancellation: true,
            echo_tail_ms: 200,
            stream_delay_ms: 0,
            noise_suppression: NoiseSuppression::High,
            agc: true,
            agc_target_dbfs: -18.0,
            agc_max_gain_db: 24.0,
        }
    }
}

impl DspConfig {
    /// Everything off: the capture stream passes through untouched (no added latency).
    pub const BYPASS: DspConfig = DspConfig {
        high_pass: false,
        echo_cancellation: false,
        echo_tail_ms: 200,
        stream_delay_ms: 0,
        noise_suppression: NoiseSuppression::Off,
        agc: false,
        agc_target_dbfs: -18.0,
        agc_max_gain_db: 24.0,
    };

    pub fn clamped(mut self) -> Self {
        self.echo_tail_ms = self
            .echo_tail_ms
            .clamp(MIN_ECHO_TAIL_MS, MAX_ECHO_TAIL_MS)
            .div_ceil(BLOCK_MS)
            * BLOCK_MS;
        self.stream_delay_ms = self.stream_delay_ms.min(MAX_STREAM_DELAY_MS);
        if !self.agc_target_dbfs.is_finite() {
            self.agc_target_dbfs = -18.0;
        }
        if !self.agc_max_gain_db.is_finite() {
            self.agc_max_gain_db = 24.0;
        }
        self.agc_target_dbfs = self.agc_target_dbfs.clamp(-30.0, -6.0);
        self.agc_max_gain_db = self.agc_max_gain_db.clamp(0.0, 40.0);
        self
    }

    fn partitions(&self) -> usize {
        (self.echo_tail_ms / BLOCK_MS).max(1) as usize
    }

    fn delay_blocks(&self) -> usize {
        (self.stream_delay_ms / BLOCK_MS) as usize
    }

    fn any_enabled(&self) -> bool {
        self.high_pass
            || self.echo_cancellation
            || self.noise_suppression != NoiseSuppression::Off
            || self.agc
    }
}

/// Runtime picture of the DSP (for diagnostics UIs; cheap to read).
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
pub struct DspStats {
    /// Echo return loss enhancement of the linear filter in dB while the far end is active
    /// (how much echo the adaptive filter removes before residual suppression); `0` when idle.
    pub erle_db: f32,
    /// Playback→capture delay the AEC is currently aligned to (hint + estimator), ms.
    pub echo_delay_ms: u32,
    /// `true` once the filter has seen enough far-end audio to have converged.
    pub echo_converged: bool,
    /// Far-end (render) audio was present in the last second.
    pub far_end_active: bool,
    /// Speech probability of the last block from the noise suppressor (`0..=1`; energy-based
    /// when suppression is off).
    pub speech_probability: f32,
    /// Current AGC gain in dB (`0` when AGC is off).
    pub agc_gain_db: f32,
    /// Blocks where the far-end queue had no reference audio while the AEC was enabled.
    pub far_end_underruns: u64,
}

/// Handle for feeding rendered (played) audio to the AEC from the playback thread.
#[derive(Clone)]
pub struct FarEndHandle(Arc<Mutex<FarEnd>>);

impl FarEndHandle {
    /// Push interleaved 48 kHz playback audio; it is down-mixed to mono. Calling this while
    /// the AEC is disabled is a no-op.
    pub fn push(&self, pcm: &[f32], channels: u8) {
        let channels = channels.clamp(1, 8) as usize;
        let mut far = self.0.lock();
        if !far.enabled {
            return;
        }
        for frame in pcm.chunks_exact(channels) {
            let sum: f32 = frame.iter().sum();
            far.push_sample(sum / channels as f32);
        }
        far.last_push_block = far.capture_blocks;
    }
}

struct FarEnd {
    enabled: bool,
    q: VecDeque<f32>,
    delay_blocks: usize,
    capture_blocks: u64,
    last_push_block: u64,
    underruns: u64,
}

impl FarEnd {
    fn new(enabled: bool, delay_blocks: usize) -> Self {
        let mut this = Self {
            enabled,
            q: VecDeque::with_capacity(
                (MAX_STREAM_DELAY_MS as usize / BLOCK_MS as usize + 8) * BLOCK,
            ),
            delay_blocks,
            capture_blocks: 0,
            last_push_block: 0,
            underruns: 0,
        };
        this.prefill();
        this
    }

    fn prefill(&mut self) {
        self.q.clear();
        self.q.resize(self.delay_blocks * BLOCK, 0.0);
    }

    fn push_sample(&mut self, s: f32) {
        let cap = (self.delay_blocks + 1 + FAR_SLACK_BLOCKS * 2) * BLOCK;
        if self.q.len() >= cap {
            self.q.pop_front();
        }
        self.q.push_back(s);
    }

    fn set_delay(&mut self, blocks: usize) {
        if blocks > self.delay_blocks {
            for _ in 0..(blocks - self.delay_blocks) * BLOCK {
                self.q.push_front(0.0);
            }
        } else {
            let drop = (self.delay_blocks - blocks) * BLOCK;
            let drop = drop.min(self.q.len());
            self.q.drain(..drop);
        }
        self.delay_blocks = blocks;
    }

    /// Take the reference block aligned with the capture block being processed. Returns
    /// `false` (and zeros) when there is no reference audio.
    fn take_block(&mut self, out: &mut [f32]) -> bool {
        self.capture_blocks += 1;
        let target = (self.delay_blocks + 1) * BLOCK;
        if self.q.len() > target + FAR_SLACK_BLOCKS * BLOCK {
            let excess = self.q.len() - target;
            self.q.drain(..excess);
        }
        if self.q.len() >= BLOCK {
            for o in out.iter_mut() {
                *o = self.q.pop_front().unwrap_or(0.0);
            }
            true
        } else {
            out.fill(0.0);
            if self.capture_blocks.saturating_sub(self.last_push_block) < 100 {
                self.underruns += 1;
            }
            false
        }
    }

    fn recently_fed(&self) -> bool {
        self.last_push_block != 0 && self.capture_blocks.saturating_sub(self.last_push_block) < 100
    }
}

/// Second-order Butterworth high-pass.
struct HighPass {
    b0: f32,
    b1: f32,
    b2: f32,
    a1: f32,
    a2: f32,
    z1: f32,
    z2: f32,
}

impl HighPass {
    fn new(cutoff_hz: f32) -> Self {
        let w0 = 2.0 * std::f32::consts::PI * cutoff_hz / SAMPLE_RATE as f32;
        let (sin, cos) = w0.sin_cos();
        let alpha = sin / (2.0 * std::f32::consts::FRAC_1_SQRT_2);
        let a0 = 1.0 + alpha;
        Self {
            b0: (1.0 + cos) / 2.0 / a0,
            b1: -(1.0 + cos) / a0,
            b2: (1.0 + cos) / 2.0 / a0,
            a1: -2.0 * cos / a0,
            a2: (1.0 - alpha) / a0,
            z1: 0.0,
            z2: 0.0,
        }
    }

    fn process(&mut self, block: &mut [f32]) {
        for s in block.iter_mut() {
            let x = *s;
            let y = self.b0 * x + self.z1;
            self.z1 = self.b1 * x - self.a1 * y + self.z2;
            self.z2 = self.b2 * x - self.a2 * y;
            *s = y;
        }
    }
}

/// Multi-delay block frequency-domain adaptive filter + spectral residual echo suppression.
struct Aec {
    fwd: Arc<dyn RealToComplex<f32>>,
    inv: Arc<dyn ComplexToReal<f32>>,
    partitions: usize,
    // Far-end history as spectra of `[x_{k-1}, x_k]` blocks, newest at `head`.
    far_spec: Vec<Vec<Complex<f32>>>,
    head: usize,
    far_prev: Vec<f32>,
    far_power: Vec<f32>,
    w: Vec<Vec<Complex<f32>>>,
    constrain_next: usize,
    // Scratch.
    time: Vec<f32>,
    spec_y: Vec<Complex<f32>>,
    spec_e: Vec<Complex<f32>>,
    fft_scratch: Vec<Complex<f32>>,
    // Adaptation control.
    see: f32,
    sdd: f32,
    syy: f32,
    pey: f32,
    pyy: f32,
    far_blocks: u64,
    // Residual echo suppression (windowed STFT of the error, hop = BLOCK).
    window: Vec<f32>,
    err_prev: Vec<f32>,
    echo_prev: Vec<f32>,
    ola: Vec<f32>,
    echo_ps: Vec<f32>,
    err_ps: Vec<f32>,
    nlp_gain: Vec<f32>,
    // Delay estimator over block log-energies.
    env_far: VecDeque<f32>,
    env_near: VecDeque<f32>,
    est_countdown: u32,
    est_candidate: Option<i32>,
    erle_db: f32,
}

const ENV_HISTORY: usize = 300;
const MAX_LAG_BLOCKS: i32 = (MAX_STREAM_DELAY_MS / BLOCK_MS) as i32;
const NLP_FLOOR: f32 = 0.03;

impl Aec {
    fn new(partitions: usize) -> Self {
        let mut planner = RealFftPlanner::<f32>::new();
        let fwd = planner.plan_fft_forward(FFT_SIZE);
        let inv = planner.plan_fft_inverse(FFT_SIZE);
        let scratch = fwd.get_scratch_len().max(inv.get_scratch_len());
        let window = (0..FFT_SIZE)
            .map(|n| 0.5 - 0.5 * (2.0 * std::f32::consts::PI * n as f32 / FFT_SIZE as f32).cos())
            .collect();
        Self {
            fwd,
            inv,
            partitions,
            far_spec: vec![vec![Complex::default(); BINS]; partitions],
            head: 0,
            far_prev: vec![0.0; BLOCK],
            far_power: vec![0.0; BINS],
            w: vec![vec![Complex::default(); BINS]; partitions],
            constrain_next: 0,
            time: vec![0.0; FFT_SIZE],
            spec_y: vec![Complex::default(); BINS],
            spec_e: vec![Complex::default(); BINS],
            fft_scratch: vec![Complex::default(); scratch],
            see: 0.0,
            sdd: 0.0,
            syy: 0.0,
            pey: 0.0,
            pyy: 0.0,
            far_blocks: 0,
            window,
            err_prev: vec![0.0; BLOCK],
            echo_prev: vec![0.0; BLOCK],
            ola: vec![0.0; BLOCK],
            echo_ps: vec![0.0; BINS],
            err_ps: vec![0.0; BINS],
            nlp_gain: vec![1.0; BINS],
            env_far: VecDeque::with_capacity(ENV_HISTORY),
            env_near: VecDeque::with_capacity(ENV_HISTORY),
            est_countdown: 100,
            est_candidate: None,
            erle_db: 0.0,
        }
    }

    fn reset_filter(&mut self) {
        for p in &mut self.w {
            p.fill(Complex::default());
        }
        for p in &mut self.far_spec {
            p.fill(Complex::default());
        }
        self.far_power.fill(0.0);
        self.far_prev.fill(0.0);
        self.see = 0.0;
        self.sdd = 0.0;
        self.syy = 0.0;
        self.pey = 0.0;
        self.pyy = 0.0;
        self.far_blocks = 0;
        self.env_far.clear();
        self.env_near.clear();
        self.est_candidate = None;
        self.erle_db = 0.0;
    }

    fn converged(&self) -> bool {
        self.far_blocks > 100
    }

    fn forward(&mut self, out: &mut [Complex<f32>]) {
        let _ = self
            .fwd
            .process_with_scratch(&mut self.time, out, &mut self.fft_scratch);
    }

    fn inverse(&mut self, spec: &mut [Complex<f32>]) {
        spec[0].im = 0.0;
        spec[BINS - 1].im = 0.0;
        let _ = self
            .inv
            .process_with_scratch(spec, &mut self.time, &mut self.fft_scratch);
        let scale = 1.0 / FFT_SIZE as f32;
        for t in self.time.iter_mut() {
            *t *= scale;
        }
    }

    /// Cancel the echo of `far` (the aligned reference block) from `near` in place. Output is
    /// delayed by one block relative to the input.
    fn process(&mut self, near: &mut [f32], far: &[f32], far_present: bool) {
        debug_assert_eq!(near.len(), BLOCK);
        let far_energy: f32 = far.iter().map(|s| s * s).sum();
        let near_energy: f32 = near.iter().map(|s| s * s).sum();
        let far_active = far_present && far_energy > 1e-7 * BLOCK as f32;

        // 1. Newest far-end partition.
        self.head = (self.head + self.partitions - 1) % self.partitions;
        let oldest = std::mem::take(&mut self.far_spec[self.head]);
        for (b, o) in self.far_power.iter_mut().zip(oldest.iter()) {
            *b = (*b - o.norm_sqr()).max(0.0);
        }
        self.time[..BLOCK].copy_from_slice(&self.far_prev);
        self.time[BLOCK..].copy_from_slice(far);
        self.far_prev.copy_from_slice(far);
        let mut newest = oldest;
        self.forward(&mut newest);
        for (b, n) in self.far_power.iter_mut().zip(newest.iter()) {
            *b += n.norm_sqr();
        }
        self.far_spec[self.head] = newest;

        // 2. Echo estimate y = Σ_p W_p · X_{k-p}.
        self.spec_y.fill(Complex::default());
        for p in 0..self.partitions {
            let idx = (self.head + p) % self.partitions;
            let x = &self.far_spec[idx];
            let w = &self.w[p];
            for b in 0..BINS {
                self.spec_y[b] += w[b] * x[b];
            }
        }
        let mut spec_y = std::mem::take(&mut self.spec_y);
        self.inverse(&mut spec_y);
        self.spec_y = spec_y;
        let mut echo = [0.0f32; BLOCK];
        echo.copy_from_slice(&self.time[BLOCK..]);
        let mut err = [0.0f32; BLOCK];
        for i in 0..BLOCK {
            err[i] = near[i] - echo[i];
        }

        // 3. Adaptation (NLMS in the frequency domain, leak-controlled step size).
        let see: f32 = err.iter().map(|s| s * s).sum();
        let syy: f32 = echo.iter().map(|s| s * s).sum();
        if far_active {
            self.far_blocks += 1;
            self.time[..BLOCK].fill(0.0);
            self.time[BLOCK..].copy_from_slice(&err);
            let mut spec_e = std::mem::take(&mut self.spec_e);
            self.forward(&mut spec_e);
            let mut pey = 0.0f32;
            let mut pyy = 0.0f32;
            for (y, e) in self.spec_y.iter().zip(spec_e.iter()) {
                pey += (y * e.conj()).re;
                pyy += y.norm_sqr();
            }
            let beta = 0.05f32;
            self.pey = (1.0 - beta) * self.pey + beta * pey.max(0.0);
            self.pyy = (1.0 - beta) * self.pyy + beta * pyy;
            self.see = (1.0 - beta) * self.see + beta * see;
            self.sdd = (1.0 - beta) * self.sdd + beta * near_energy;
            self.syy = (1.0 - beta) * self.syy + beta * syy;
            let leak = if self.pyy > 1e-12 {
                (self.pey / self.pyy).clamp(0.0, 1.0)
            } else {
                1.0
            };
            let rate = if self.converged() {
                (0.7 * leak * self.syy / (self.see + 1e-9)).clamp(0.0, 0.5)
            } else {
                0.25
            };
            let mean_power = self.far_power.iter().sum::<f32>() / BINS as f32;
            let reg = 1e-3 * mean_power + 1e-9;
            for p in 0..self.partitions {
                let idx = (self.head + p) % self.partitions;
                let x = &self.far_spec[idx];
                let w = &mut self.w[p];
                for b in 0..BINS {
                    let g = x[b].conj() * spec_e[b] * (rate / (self.far_power[b] + reg));
                    w[b] += g;
                }
            }
            self.spec_e = spec_e;
            // Gradient constraint on one partition per block (round-robin): the impulse
            // response of each partition must live in its first half.
            let p = self.constrain_next;
            self.constrain_next = (p + 1) % self.partitions;
            let mut wp = std::mem::take(&mut self.w[p]);
            self.inverse(&mut wp);
            self.time[BLOCK..].fill(0.0);
            self.forward(&mut wp);
            self.w[p] = wp;
            self.erle_db = if self.see > 1e-9 && self.sdd > 1e-9 {
                (10.0 * (self.sdd / self.see).log10()).clamp(0.0, 40.0)
            } else {
                0.0
            };
        }

        // 4. Residual echo suppression on a Hann-windowed STFT of the error.
        let (head, tail) = self.time.split_at_mut(BLOCK);
        for ((t, p), w) in head
            .iter_mut()
            .zip(&self.err_prev)
            .zip(&self.window[..BLOCK])
        {
            *t = p * w;
        }
        for ((t, e), w) in tail.iter_mut().zip(err.iter()).zip(&self.window[BLOCK..]) {
            *t = e * w;
        }
        let mut spec_e = std::mem::take(&mut self.spec_e);
        self.forward(&mut spec_e);
        let (head, tail) = self.time.split_at_mut(BLOCK);
        for ((t, p), w) in head
            .iter_mut()
            .zip(&self.echo_prev)
            .zip(&self.window[..BLOCK])
        {
            *t = p * w;
        }
        for ((t, e), w) in tail.iter_mut().zip(echo.iter()).zip(&self.window[BLOCK..]) {
            *t = e * w;
        }
        let mut spec_y = std::mem::take(&mut self.spec_y);
        self.forward(&mut spec_y);
        self.err_prev.copy_from_slice(&err);
        self.echo_prev.copy_from_slice(&echo);
        let residual = if far_active && self.converged() {
            let erle_lin = if self.see > 1e-9 {
                self.sdd / self.see
            } else {
                1.0
            };
            (2.0 / erle_lin.max(1.0)).clamp(0.05, 1.0)
        } else if far_active {
            1.0
        } else {
            0.0
        };
        let over = 2.0f32;
        for b in 0..BINS {
            self.echo_ps[b] = 0.7 * self.echo_ps[b] + 0.3 * spec_y[b].norm_sqr();
            self.err_ps[b] = 0.7 * self.err_ps[b] + 0.3 * spec_e[b].norm_sqr();
            let target = if residual > 0.0 {
                (1.0 - over * residual * self.echo_ps[b] / (self.err_ps[b] + 1e-12))
                    .clamp(NLP_FLOOR, 1.0)
            } else {
                1.0
            };
            self.nlp_gain[b] = 0.6 * self.nlp_gain[b] + 0.4 * target;
        }
        // Light smoothing across frequency avoids musical noise.
        let mut prev = self.nlp_gain[0];
        for b in 1..BINS - 1 {
            let cur = self.nlp_gain[b];
            let next = self.nlp_gain[b + 1];
            self.nlp_gain[b] = (prev + cur + next) / 3.0;
            prev = cur;
        }
        for (e, g) in spec_e.iter_mut().zip(&self.nlp_gain) {
            *e *= g;
        }
        self.inverse(&mut spec_e);
        self.spec_e = spec_e;
        self.spec_y = spec_y;
        for ((n, t), o) in near.iter_mut().zip(&self.time[..BLOCK]).zip(&self.ola) {
            *n = t + o;
        }
        self.ola.copy_from_slice(&self.time[BLOCK..]);

        // 5. Delay estimation from log-energy envelopes.
        self.push_envelope(far_energy, near_energy);
    }

    fn push_envelope(&mut self, far_energy: f32, near_energy: f32) {
        if self.env_far.len() == ENV_HISTORY {
            self.env_far.pop_front();
            self.env_near.pop_front();
        }
        self.env_far.push_back((far_energy + 1e-9).ln());
        self.env_near.push_back((near_energy + 1e-9).ln());
    }

    /// Every ~1 s, correlate the envelopes over candidate lags. Returns a delay adjustment (in
    /// blocks, may be negative) when two consecutive estimates agree on a lag that the filter
    /// tail does not comfortably cover.
    fn estimate_delay_shift(&mut self, current_delay_blocks: usize) -> Option<i32> {
        if self.est_countdown > 0 {
            self.est_countdown -= 1;
            return None;
        }
        self.est_countdown = 100;
        if self.env_far.len() < ENV_HISTORY / 2 {
            return None;
        }
        let far: Vec<f32> = self.env_far.iter().copied().collect();
        let near: Vec<f32> = self.env_near.iter().copied().collect();
        let n = far.len();
        let mean = |v: &[f32]| v.iter().sum::<f32>() / v.len() as f32;
        let far_mean = mean(&far);
        let far_var = far.iter().map(|v| (v - far_mean).powi(2)).sum::<f32>() / n as f32;
        // Needs real far-end activity changes (≈ 6 dB std-dev of block energy) to be usable.
        if far_var < 2.0 {
            return None;
        }
        let min_lag = -(current_delay_blocks.min(MAX_LAG_BLOCKS as usize) as i32);
        let max_lag = MAX_LAG_BLOCKS - current_delay_blocks.min(MAX_LAG_BLOCKS as usize) as i32;
        let mut best = (0i32, f32::MIN);
        let mut second = f32::MIN;
        for lag in min_lag..=max_lag {
            let (mut sxy, mut sxx, mut syy, mut cnt) = (0.0f32, 0.0f32, 0.0f32, 0usize);
            let (mut sx, mut sy) = (0.0f32, 0.0f32);
            for (t, &y) in near.iter().enumerate().take(n) {
                let ft = t as i32 - lag;
                if ft < 0 || ft >= n as i32 {
                    continue;
                }
                let x = far[ft as usize];
                sx += x;
                sy += y;
                sxy += x * y;
                sxx += x * x;
                syy += y * y;
                cnt += 1;
            }
            if cnt < n / 2 {
                continue;
            }
            let c = cnt as f32;
            let cov = sxy - sx * sy / c;
            let var = (sxx - sx * sx / c) * (syy - sy * sy / c);
            if var <= 1e-6 {
                continue;
            }
            let corr = cov / var.sqrt();
            if corr > best.1 {
                second = best.1;
                best = (lag, corr);
            } else if corr > second {
                second = corr;
            }
        }
        let (lag, corr) = best;
        if corr < 0.6 || (second > f32::MIN && corr < second * 1.2) {
            self.est_candidate = None;
            return None;
        }
        // Inside the first half of the tail the filter models the delay itself.
        let comfortable = lag >= 0 && (lag as usize) < self.partitions / 2;
        if comfortable {
            self.est_candidate = None;
            return None;
        }
        match self.est_candidate {
            Some(prev) if (prev - lag).abs() <= 2 => {
                self.est_candidate = None;
                // Leave a quarter of the tail ahead of the echo for the filter to model.
                Some(lag - (self.partitions / 4) as i32)
            }
            _ => {
                self.est_candidate = Some(lag);
                None
            }
        }
    }
}

struct NoiseSuppressor {
    state: Box<DenoiseState<'static>>,
    wet: f32,
    input: Vec<f32>,
    output: Vec<f32>,
}

impl NoiseSuppressor {
    fn new(level: NoiseSuppression) -> Self {
        Self {
            state: DenoiseState::new(),
            wet: level.wet(),
            input: vec![0.0; BLOCK],
            output: vec![0.0; BLOCK],
        }
    }

    /// Returns the network's speech probability for the block.
    fn process(&mut self, block: &mut [f32]) -> f32 {
        for (i, s) in self.input.iter_mut().zip(block.iter()) {
            *i = s * 32768.0;
        }
        let prob = self.state.process_frame(&mut self.output, &self.input);
        let wet = self.wet;
        let dry = 1.0 - wet;
        for (s, o) in block.iter_mut().zip(self.output.iter()) {
            let denoised = (o / 32768.0).clamp(-1.0, 1.0);
            *s = dry * *s + wet * denoised;
        }
        prob.clamp(0.0, 1.0)
    }
}

struct Agc {
    gain: f32,
    target_rms: f32,
    max_gain: f32,
}

const AGC_MIN_GAIN: f32 = 0.1;
const LIMITER_KNEE: f32 = 0.89;

impl Agc {
    fn new(target_dbfs: f32, max_gain_db: f32) -> Self {
        Self {
            gain: 1.0,
            target_rms: 10f32.powf(target_dbfs / 20.0),
            max_gain: 10f32.powf(max_gain_db / 20.0),
        }
    }

    fn process(&mut self, block: &mut [f32], speech_probability: f32) {
        let rms = (block.iter().map(|s| s * s).sum::<f32>() / block.len() as f32).sqrt();
        if speech_probability > 0.5 && rms > 1e-4 {
            let desired = (self.target_rms / rms).clamp(AGC_MIN_GAIN, self.max_gain);
            if desired < self.gain {
                self.gain += (desired - self.gain) * 0.3;
            } else {
                self.gain += (desired - self.gain) * 0.02;
            }
        } else if rms * self.gain > 0.9 {
            // Loud non-speech (a slam, a click): back off quickly rather than clip.
            self.gain = (self.gain * 0.7).max(AGC_MIN_GAIN);
        }
        for s in block.iter_mut() {
            let v = *s * self.gain;
            let a = v.abs();
            *s = if a > LIMITER_KNEE {
                let room = 1.0 - LIMITER_KNEE;
                v.signum() * (LIMITER_KNEE + room * ((a - LIMITER_KNEE) / room).tanh())
            } else {
                v
            };
        }
    }

    fn gain_db(&self) -> f32 {
        20.0 * self.gain.log10()
    }
}

/// Energy-based speech gate used for the AGC when the neural suppressor is off.
struct EnergyGate {
    floor: f32,
}

impl EnergyGate {
    fn probability(&mut self, block: &[f32]) -> f32 {
        let energy = block.iter().map(|s| s * s).sum::<f32>() / block.len() as f32;
        if energy < self.floor {
            self.floor = energy.max(1e-10);
        } else {
            self.floor += (energy - self.floor) * 0.005;
        }
        if energy > self.floor * 8.0 {
            1.0
        } else {
            0.0
        }
    }
}

/// The capture DSP chain. Not `Send`-hostile: it lives inside the capture encoder and is
/// driven from whichever thread pushes capture audio.
pub struct Dsp {
    cfg: DspConfig,
    high_pass: Option<HighPass>,
    aec: Option<Aec>,
    ns: Option<NoiseSuppressor>,
    agc: Option<Agc>,
    gate: EnergyGate,
    far: Arc<Mutex<FarEnd>>,
    far_block: Vec<f32>,
    stats: DspStats,
}

impl Dsp {
    pub fn new(cfg: DspConfig) -> Self {
        let cfg = cfg.clamped();
        let far = Arc::new(Mutex::new(FarEnd::new(
            cfg.echo_cancellation,
            cfg.delay_blocks(),
        )));
        let mut this = Self {
            cfg,
            high_pass: None,
            aec: None,
            ns: None,
            agc: None,
            gate: EnergyGate { floor: 1e-6 },
            far,
            far_block: vec![0.0; BLOCK],
            stats: DspStats::default(),
        };
        this.rebuild();
        this
    }

    fn rebuild(&mut self) {
        let cfg = self.cfg;
        self.high_pass = cfg.high_pass.then(|| HighPass::new(80.0));
        self.aec = cfg.echo_cancellation.then(|| Aec::new(cfg.partitions()));
        self.ns = (cfg.noise_suppression != NoiseSuppression::Off)
            .then(|| NoiseSuppressor::new(cfg.noise_suppression));
        self.agc = cfg
            .agc
            .then(|| Agc::new(cfg.agc_target_dbfs, cfg.agc_max_gain_db));
        let mut far = self.far.lock();
        far.enabled = cfg.echo_cancellation;
        far.delay_blocks = cfg.delay_blocks();
        far.prefill();
        far.underruns = 0;
        self.stats = DspStats {
            echo_delay_ms: cfg.stream_delay_ms,
            ..DspStats::default()
        };
    }

    pub fn config(&self) -> DspConfig {
        self.cfg
    }

    /// Replace the configuration. Stages whose parameters changed are rebuilt (the AEC forgets
    /// its filter when the tail or delay hint changes); unchanged stages keep their state.
    pub fn set_config(&mut self, cfg: DspConfig) {
        let cfg = cfg.clamped();
        if cfg == self.cfg {
            return;
        }
        let old = self.cfg;
        self.cfg = cfg;
        if cfg.high_pass != old.high_pass {
            self.high_pass = cfg.high_pass.then(|| HighPass::new(80.0));
        }
        if cfg.echo_cancellation != old.echo_cancellation
            || cfg.echo_tail_ms != old.echo_tail_ms
            || cfg.stream_delay_ms != old.stream_delay_ms
        {
            self.aec = cfg.echo_cancellation.then(|| Aec::new(cfg.partitions()));
            let mut far = self.far.lock();
            far.enabled = cfg.echo_cancellation;
            far.delay_blocks = cfg.delay_blocks();
            far.prefill();
            far.underruns = 0;
            self.stats.echo_delay_ms = cfg.stream_delay_ms;
            self.stats.erle_db = 0.0;
            self.stats.echo_converged = false;
        }
        if cfg.noise_suppression != old.noise_suppression {
            match (&mut self.ns, cfg.noise_suppression) {
                (_, NoiseSuppression::Off) => self.ns = None,
                (Some(ns), level) => ns.wet = level.wet(),
                (None, level) => self.ns = Some(NoiseSuppressor::new(level)),
            }
        }
        if cfg.agc != old.agc
            || cfg.agc_target_dbfs != old.agc_target_dbfs
            || cfg.agc_max_gain_db != old.agc_max_gain_db
        {
            self.agc = cfg
                .agc
                .then(|| Agc::new(cfg.agc_target_dbfs, cfg.agc_max_gain_db));
            if !cfg.agc {
                self.stats.agc_gain_db = 0.0;
            }
        }
    }

    pub fn stats(&self) -> DspStats {
        self.stats
    }

    pub fn far_end(&self) -> FarEndHandle {
        FarEndHandle(self.far.clone())
    }

    /// `true` when at least one stage is active (otherwise `process_frame` is a no-op).
    pub fn active(&self) -> bool {
        self.cfg.any_enabled()
    }

    /// Process mono 48 kHz capture audio in place — one 20 ms frame in the capture encoder,
    /// any whole number of 10 ms blocks for standalone users (a trailing partial block is
    /// left untouched).
    pub fn process_frame(&mut self, frame: &mut [f32]) {
        debug_assert_eq!(frame.len() % BLOCK, 0);
        if !self.cfg.any_enabled() {
            return;
        }
        for block in frame.as_chunks_mut::<BLOCK>().0 {
            self.process_block(block);
        }
    }

    fn process_block(&mut self, block: &mut [f32]) {
        if let Some(hp) = self.high_pass.as_mut() {
            hp.process(block);
        }
        if let Some(aec) = self.aec.as_mut() {
            let (present, fed, underruns, delay_blocks) = {
                let mut far = self.far.lock();
                let present = far.take_block(&mut self.far_block);
                (present, far.recently_fed(), far.underruns, far.delay_blocks)
            };
            aec.process(block, &self.far_block, present);
            self.stats.far_end_active = fed;
            self.stats.far_end_underruns = underruns;
            self.stats.echo_converged = aec.converged();
            self.stats.erle_db = aec.erle_db;
            self.stats.echo_delay_ms = delay_blocks as u32 * BLOCK_MS;
            if let Some(shift) = aec.estimate_delay_shift(delay_blocks) {
                let new_delay = (delay_blocks as i32 + shift).clamp(0, MAX_LAG_BLOCKS) as usize;
                if new_delay != delay_blocks {
                    self.far.lock().set_delay(new_delay);
                    aec.reset_filter();
                    self.stats.echo_delay_ms = new_delay as u32 * BLOCK_MS;
                }
            }
        }
        let speech = match self.ns.as_mut() {
            Some(ns) => ns.process(block),
            None => self.gate.probability(block),
        };
        self.stats.speech_probability = speech;
        if let Some(agc) = self.agc.as_mut() {
            agc.process(block, speech);
            self.stats.agc_gain_db = agc.gain_db();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rms(v: &[f32]) -> f32 {
        (v.iter().map(|s| s * s).sum::<f32>() / v.len() as f32).sqrt()
    }

    fn tone(len: usize, hz: f32, amp: f32, phase: &mut f32) -> Vec<f32> {
        (0..len)
            .map(|_| {
                let s = amp * phase.sin();
                *phase += 2.0 * std::f32::consts::PI * hz / SAMPLE_RATE as f32;
                s
            })
            .collect()
    }

    /// Deterministic pseudo-random noise in `[-amp, amp]`.
    struct Lcg(u64);
    impl Lcg {
        fn next(&mut self) -> f32 {
            self.0 = self
                .0
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            ((self.0 >> 40) as f32 / (1u64 << 24) as f32) * 2.0 - 1.0
        }
        fn block(&mut self, len: usize, amp: f32) -> Vec<f32> {
            (0..len).map(|_| self.next() * amp).collect()
        }
    }

    #[test]
    fn config_is_clamped_and_rounded_to_blocks() {
        let c = DspConfig {
            echo_tail_ms: 999,
            stream_delay_ms: 5000,
            agc_target_dbfs: 3.0,
            agc_max_gain_db: -5.0,
            ..DspConfig::default()
        }
        .clamped();
        assert_eq!(c.echo_tail_ms, MAX_ECHO_TAIL_MS);
        assert_eq!(c.stream_delay_ms, MAX_STREAM_DELAY_MS);
        assert_eq!(c.agc_target_dbfs, -6.0);
        assert_eq!(c.agc_max_gain_db, 0.0);
        let c = DspConfig {
            echo_tail_ms: 5,
            ..DspConfig::default()
        }
        .clamped();
        assert_eq!(c.echo_tail_ms, MIN_ECHO_TAIL_MS);
        let c = DspConfig {
            echo_tail_ms: 125,
            ..DspConfig::default()
        }
        .clamped();
        assert_eq!(c.echo_tail_ms, 130);
        assert_eq!(c.partitions(), 13);
        assert!(!DspConfig::BYPASS.any_enabled());
    }

    #[test]
    fn bypass_leaves_audio_untouched() {
        let mut dsp = Dsp::new(DspConfig::BYPASS);
        let mut phase = 0.0;
        let original = tone(FRAME_SAMPLES, 440.0, 0.5, &mut phase);
        let mut frame = original.clone();
        dsp.process_frame(&mut frame);
        assert_eq!(frame, original);
        assert!(!dsp.active());
    }

    #[test]
    fn high_pass_removes_dc_and_keeps_speech_band() {
        let cfg = DspConfig {
            high_pass: true,
            ..DspConfig::BYPASS
        };
        let mut dsp = Dsp::new(cfg);
        let mut phase = 0.0;
        let mut last = Vec::new();
        for _ in 0..25 {
            let mut frame: Vec<f32> = tone(FRAME_SAMPLES, 1000.0, 0.3, &mut phase)
                .into_iter()
                .map(|s| s + 0.4)
                .collect();
            dsp.process_frame(&mut frame);
            last = frame;
        }
        let mean = last.iter().sum::<f32>() / last.len() as f32;
        assert!(mean.abs() < 0.01, "dc left: {mean}");
        let r = rms(&last);
        assert!((r - 0.3 / 2f32.sqrt()).abs() < 0.02, "tone changed: {r}");
    }

    #[test]
    fn noise_suppression_reduces_stationary_noise() {
        let cfg = DspConfig {
            noise_suppression: NoiseSuppression::High,
            ..DspConfig::BYPASS
        };
        let mut dsp = Dsp::new(cfg);
        let mut rng = Lcg(7);
        let mut in_rms = 0.0;
        let mut out_rms = 0.0;
        for i in 0..400 {
            let mut frame = rng.block(FRAME_SAMPLES, 0.02);
            let before = rms(&frame);
            dsp.process_frame(&mut frame);
            if i >= 300 {
                in_rms += before;
                out_rms += rms(&frame);
            }
        }
        assert!(
            out_rms < in_rms * 0.1,
            "noise not suppressed: in {in_rms} out {out_rms}"
        );
        assert!(dsp.stats().speech_probability < 0.5);
    }

    #[test]
    fn agc_levels_speech_like_signal_to_target() {
        let cfg = DspConfig {
            agc: true,
            agc_target_dbfs: -18.0,
            agc_max_gain_db: 30.0,
            ..DspConfig::BYPASS
        };
        let mut dsp = Dsp::new(cfg);
        // A quiet "voiced" signal (tone bursts) that the energy gate classifies as speech
        // because it alternates with silence.
        let mut phase = 0.0;
        let mut last = 0.0;
        for i in 0..400 {
            let mut frame = if i % 10 < 6 {
                tone(FRAME_SAMPLES, 300.0, 0.01, &mut phase)
            } else {
                vec![0.0; FRAME_SAMPLES]
            };
            dsp.process_frame(&mut frame);
            if i % 10 == 5 {
                last = rms(&frame);
            }
        }
        let target = 10f32.powf(-18.0 / 20.0);
        assert!(
            (last / target) > 0.6 && (last / target) < 1.4,
            "agc output rms {last}, target {target}"
        );
        assert!(dsp.stats().agc_gain_db > 10.0);
        // Loud input is turned down (and never clips).
        for _ in 0..200 {
            let mut frame = tone(FRAME_SAMPLES, 300.0, 0.9, &mut phase);
            dsp.process_frame(&mut frame);
            assert!(frame.iter().all(|s| s.abs() <= 1.0));
            last = rms(&frame);
        }
        assert!(last < 0.3, "loud input not attenuated: {last}");
    }

    #[test]
    fn aec_removes_echo_of_rendered_audio() {
        let cfg = DspConfig {
            echo_cancellation: true,
            echo_tail_ms: 200,
            ..DspConfig::BYPASS
        };
        let mut dsp = Dsp::new(cfg);
        let far_handle = dsp.far_end();
        let mut rng = Lcg(42);
        // Echo path: 3 taps within the tail (2 ms, 30 ms, 61 ms), attenuated.
        let taps: [(usize, f32); 3] = [(96, 0.6), (1440, -0.3), (2928, 0.15)];
        let mut far_hist: VecDeque<f32> = VecDeque::from(vec![0.0; 4000]);
        let mut in_energy = 0.0f32;
        let mut out_energy = 0.0f32;
        for i in 0..600 {
            let far = rng.block(FRAME_SAMPLES, 0.3);
            far_handle.push(&far, 1);
            let mut near = vec![0.0f32; FRAME_SAMPLES];
            for (n, f) in far.iter().enumerate() {
                far_hist.push_back(*f);
                far_hist.pop_front();
                let len = far_hist.len();
                let mut s = 0.0;
                for (d, g) in taps {
                    s += g * far_hist[len - 1 - d];
                }
                near[n] = s;
            }
            let before = rms(&near);
            dsp.process_frame(&mut near);
            if i >= 400 {
                in_energy += before;
                out_energy += rms(&near);
            }
        }
        let erle = 20.0 * (in_energy / out_energy.max(1e-9)).log10();
        assert!(erle > 15.0, "echo not cancelled: {erle} dB");
        let stats = dsp.stats();
        assert!(stats.echo_converged);
        assert!(stats.far_end_active);
        assert!(stats.erle_db > 6.0, "linear erle {}", stats.erle_db);
    }

    #[test]
    fn aec_preserves_near_end_speech_without_far_end() {
        let cfg = DspConfig {
            echo_cancellation: true,
            ..DspConfig::BYPASS
        };
        let mut dsp = Dsp::new(cfg);
        let mut phase = 0.0;
        let mut last = Vec::new();
        for _ in 0..50 {
            let mut frame = tone(FRAME_SAMPLES, 500.0, 0.3, &mut phase);
            dsp.process_frame(&mut frame);
            last = frame;
        }
        let r = rms(&last);
        assert!(
            (r - 0.3 / 2f32.sqrt()).abs() < 0.02,
            "near end altered: {r}"
        );
        assert!(!dsp.stats().far_end_active);
    }

    #[test]
    fn aec_keeps_near_end_during_double_talk() {
        let cfg = DspConfig {
            echo_cancellation: true,
            echo_tail_ms: 100,
            ..DspConfig::BYPASS
        };
        let mut dsp = Dsp::new(cfg);
        let far_handle = dsp.far_end();
        let mut rng = Lcg(3);
        let mut phase = 0.0;
        let mut hist: VecDeque<f32> = VecDeque::from(vec![0.0; 1000]);
        let mut talk_rms = 0.0;
        let mut n = 0;
        for i in 0..800 {
            let far = rng.block(FRAME_SAMPLES, 0.3);
            far_handle.push(&far, 1);
            let speech = if i >= 500 {
                tone(FRAME_SAMPLES, 700.0, 0.3, &mut phase)
            } else {
                vec![0.0; FRAME_SAMPLES]
            };
            let mut near = vec![0.0f32; FRAME_SAMPLES];
            for (k, f) in far.iter().enumerate() {
                hist.push_back(*f);
                hist.pop_front();
                near[k] = 0.5 * hist[hist.len() - 1 - 240] + speech[k];
            }
            dsp.process_frame(&mut near);
            if i >= 600 {
                talk_rms += rms(&near);
                n += 1;
            }
        }
        let talk_rms = talk_rms / n as f32;
        let expected = 0.3 / 2f32.sqrt();
        assert!(
            talk_rms > expected * 0.5,
            "near-end speech suppressed during double talk: {talk_rms} vs {expected}"
        );
    }

    #[test]
    fn far_end_queue_holds_the_delay_and_realigns_on_drift() {
        let mut far = FarEnd::new(true, 3);
        assert_eq!(far.q.len(), 3 * BLOCK);
        for _ in 0..BLOCK {
            far.push_sample(1.0);
        }
        let mut out = vec![0.0; BLOCK];
        // The first three reads are the zero prefill (the delay), the fourth is the audio.
        for _ in 0..3 {
            assert!(far.take_block(&mut out));
            assert!(out.iter().all(|s| *s == 0.0));
        }
        assert!(far.take_block(&mut out));
        assert!(out.iter().all(|s| *s == 1.0));
        assert!(!far.take_block(&mut out));
        // Playback runs ahead: excess beyond the slack is dropped so alignment is kept.
        for _ in 0..20 * BLOCK {
            far.push_sample(0.5);
        }
        far.take_block(&mut out);
        assert!(far.q.len() <= (3 + 1) * BLOCK);
        far.set_delay(6);
        assert_eq!(far.delay_blocks, 6);
        far.set_delay(1);
        assert_eq!(far.delay_blocks, 1);
    }

    #[test]
    fn set_config_keeps_untouched_stages() {
        let mut dsp = Dsp::new(DspConfig::default());
        let cfg = DspConfig {
            noise_suppression: NoiseSuppression::Low,
            ..DspConfig::default()
        };
        dsp.set_config(cfg);
        assert_eq!(dsp.config().noise_suppression, NoiseSuppression::Low);
        assert!(dsp.aec.is_some() && dsp.agc.is_some() && dsp.high_pass.is_some());
        dsp.set_config(DspConfig::BYPASS);
        assert!(dsp.aec.is_none() && dsp.ns.is_none() && dsp.agc.is_none());
        assert!(!dsp.far.lock().enabled);
        assert_eq!(dsp.stats().agc_gain_db, 0.0);
    }
}
