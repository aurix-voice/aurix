//! Audio building blocks shared by every native integration: level metering + VAD, capture
//! framing (any PCM format → 20 ms mono 48 kHz Opus frames, or 8 kHz G.711 μ-law when the
//! session negotiated the PCMU fallback) and the receive side (per-sender jitter buffer +
//! decoder, mixed into one interleaved output with per-participant gain and constant-power
//! panning). Mirrors `sdk/unity/Runtime/Audio` so all clients sound the same.

use aurix_common::g711::{Law, PCMU_FRAME_SAMPLES, PCMU_FRAME_SIZES, PCMU_SAMPLE_RATE};
use aurix_common::protocol::{
    decode_audio_level, encode_audio_level, opus_packet_is_stereo, AUDIO_LEVEL_SILENCE,
};
use aurix_common::types::{AudioCodec, AudioPolicy, Direction, OpusBandwidth, OpusSignal};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::time::{Duration, Instant};

use crate::dsp::{Dsp, DspConfig};
use crate::effects::EffectChain;
use crate::visemes::{VisemeAnalyzer, VisemeFrame};

/// Opus on the wire runs at 48 kHz; the mixer and the capture path work at this rate and
/// PCMU frames are resampled to/from it at the edge.
pub const SAMPLE_RATE: u32 = 48_000;
/// One packet carries 20 ms of audio.
pub const FRAME_SAMPLES: usize = 960;
pub const FRAME_DURATION: Duration = Duration::from_millis(20);
/// 48 kHz → 8 kHz decimation factor of the PCMU path.
const PCMU_DECIMATION: usize = (SAMPLE_RATE / PCMU_SAMPLE_RATE) as usize;
/// Largest software input gain (+12 dB).
pub const MAX_INPUT_GAIN: f32 = 4.0;
/// Largest master output volume (+6 dB).
pub const MAX_OUTPUT_VOLUME: f32 = 2.0;
/// Decoders accept frames up to 60 ms.
const MAX_DECODE_SAMPLES: usize = FRAME_SAMPLES * 3;
const STREAM_IDLE_TIMEOUT: Duration = Duration::from_secs(30);
/// A sequence jump this far from the expected one (in either direction) means the sender's
/// numbering restarted — e.g. the session moved to another node — rather than jitter.
const JITTER_RESYNC_GAP: u32 = 500;

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

/// Low-pass FIR (windowed sinc) run at 48 kHz on both sides of the PCMU path: before decimating
/// captured audio to 8 kHz (anti-aliasing) and after zero-stuffing decoded μ-law back to 48 kHz
/// (anti-imaging). Cut-off is the telephone band, so it costs nothing audible on μ-law.
#[derive(Debug, Clone)]
struct NarrowbandFir {
    taps: Vec<f32>,
    history: Vec<f32>,
    pos: usize,
}

impl NarrowbandFir {
    const TAPS: usize = 63;
    const CUTOFF_HZ: f32 = 3_600.0;

    fn new(gain: f32) -> Self {
        let fc = Self::CUTOFF_HZ / SAMPLE_RATE as f32;
        let m = (Self::TAPS - 1) as f32;
        let mut taps: Vec<f32> = (0..Self::TAPS)
            .map(|n| {
                let x = n as f32 - m / 2.0;
                let sinc = if x == 0.0 {
                    2.0 * fc
                } else {
                    (2.0 * std::f32::consts::PI * fc * x).sin() / (std::f32::consts::PI * x)
                };
                let hamming = 0.54 - 0.46 * (2.0 * std::f32::consts::PI * n as f32 / m).cos();
                sinc * hamming
            })
            .collect();
        let sum: f32 = taps.iter().sum();
        for t in taps.iter_mut() {
            *t *= gain / sum;
        }
        Self {
            taps,
            history: vec![0.0; Self::TAPS],
            pos: 0,
        }
    }

    fn process(&mut self, sample: f32) -> f32 {
        self.history[self.pos] = sample;
        let mut acc = 0.0;
        let mut idx = self.pos;
        for &t in &self.taps {
            acc += t * self.history[idx];
            idx = if idx == 0 { Self::TAPS - 1 } else { idx - 1 };
        }
        self.pos = (self.pos + 1) % Self::TAPS;
        acc
    }

    fn reset(&mut self) {
        self.history.iter_mut().for_each(|h| *h = 0.0);
        self.pos = 0;
    }
}

/// 48 kHz mono → G.711 (μ-law or A-law) at 8 kHz, one 20 ms frame at a time.
#[derive(Debug, Clone)]
pub struct G711Encoder {
    fir: NarrowbandFir,
}

impl Default for G711Encoder {
    fn default() -> Self {
        Self::new()
    }
}

impl G711Encoder {
    pub fn new() -> Self {
        Self {
            fir: NarrowbandFir::new(1.0),
        }
    }

    /// `pcm` is 48 kHz mono; every 6th low-passed sample becomes one `law` byte in `out`.
    pub fn encode(&mut self, law: Law, pcm: &[f32], out: &mut Vec<u8>) {
        for (i, &s) in pcm.iter().enumerate() {
            let y = self.fir.process(s);
            if i % PCMU_DECIMATION == PCMU_DECIMATION - 1 {
                out.push(law.encode_sample((y.clamp(-1.0, 1.0) * 32767.0) as i16));
            }
        }
    }

    pub fn reset(&mut self) {
        self.fir.reset();
    }
}

/// G.711 (μ-law or A-law) at 8 kHz → 48 kHz mono, with hold-last-sample concealment for
/// lost frames.
#[derive(Debug, Clone)]
pub struct G711Decoder {
    fir: NarrowbandFir,
    last: f32,
}

impl Default for G711Decoder {
    fn default() -> Self {
        Self::new()
    }
}

impl G711Decoder {
    pub fn new() -> Self {
        Self {
            fir: NarrowbandFir::new(PCMU_DECIMATION as f32),
            last: 0.0,
        }
    }

    /// Decodes one `law` frame (10/20/40/60 ms) into `out` at 48 kHz. Returns the number of
    /// samples written, or `None` for a frame of unsupported size.
    pub fn decode(&mut self, law: Law, coded: &[u8], out: &mut [f32]) -> Option<usize> {
        if !PCMU_FRAME_SIZES.contains(&coded.len()) || out.len() < coded.len() * PCMU_DECIMATION {
            return None;
        }
        let mut n = 0;
        for &b in coded {
            let s = law.decode_sample(b) as f32 / 32768.0;
            self.last = s;
            for k in 0..PCMU_DECIMATION {
                out[n] = self.fir.process(if k == 0 { s } else { 0.0 });
                n += 1;
            }
        }
        Some(n)
    }

    /// Fills one 20 ms frame for a lost packet: a fast fade from the last sample to silence.
    pub fn conceal(&mut self, out: &mut [f32]) -> usize {
        let n = FRAME_SAMPLES.min(out.len());
        for (i, o) in out[..n].iter_mut().enumerate() {
            let fade = 1.0 - i as f32 / n as f32;
            *o = self.last * fade;
        }
        self.last = 0.0;
        n
    }

    pub fn reset(&mut self) {
        self.fir.reset();
        self.last = 0.0;
    }
}

/// One encoded uplink frame.
#[derive(Debug, Clone)]
pub struct EncodedFrame {
    /// Encoded audio: Opus, or 160 bytes of μ-law when `codec` is [`AudioCodec::Pcmu`].
    pub payload: Vec<u8>,
    pub codec: AudioCodec,
    /// Wire audio level of the frame before encoding.
    pub level: u8,
    /// RMS of the frame, `0..=1`.
    pub energy: f32,
    /// The VAD considers this frame speech.
    pub speech: bool,
}

/// Everything the uplink Opus encoder can be tuned with. Values outside libopus' ranges are
/// clamped by [`EncoderSettings::clamped`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EncoderSettings {
    /// `6_000..=300_000` bit/s for mono, `..=510_000` for stereo (libopus' ceilings).
    pub bitrate_bps: u32,
    /// `0..=10`; lower is cheaper on the CPU, higher sounds better at the same bitrate.
    pub complexity: u8,
    /// Widest audio band the encoder may code (`OPUS_SET_MAX_BANDWIDTH`).
    pub max_bandwidth: OpusBandwidth,
    /// Content hint (`OPUS_SET_SIGNAL`).
    pub signal: OpusSignal,
    /// Variable bitrate; off is hard CBR at `bitrate_bps`.
    pub vbr: bool,
    /// Constrained VBR keeps every frame within the bitrate's byte budget.
    pub constrained_vbr: bool,
    /// In-band forward error correction (costs bitrate, recovers single lost frames).
    pub fec: bool,
    /// Loss the FEC is tuned for, `0..=100` %.
    pub expected_loss_percent: u8,
    /// Discontinuous transmission: near-empty frames during silence.
    pub dtx: bool,
    /// Channels the uplink is encoded with: `1` (voice, the default) or `2` (stereo music /
    /// broadcast). A channel policy without `stereo` forces `1`; PCMU is always mono.
    pub channels: u8,
    /// Deep REDundancy (Opus 1.5 DRED): every packet carries a neural low-rate summary of up
    /// to this much preceding audio (`0` = off, `..=1040` ms in 10 ms steps), so a receiver
    /// rebuilds bursts of several lost frames instead of concealing them. Costs a share of
    /// `bitrate_bps` (nothing below ~28 kbit/s with FEC on); the adaptive loss profile
    /// turns it on under heavy loss.
    pub dred_duration_ms: u16,
}

impl EncoderSettings {
    pub const MIN_BITRATE: u32 = 6_000;
    /// libopus clamps a mono stream's bitrate to 300 kbit/s internally.
    pub const MAX_BITRATE: u32 = 300_000;
    /// libopus' ceiling for a stereo stream.
    pub const MAX_STEREO_BITRATE: u32 = 510_000;
    /// libopus' own default.
    pub const DEFAULT_COMPLEXITY: u8 = 9;
    /// Longest DRED history libopus can code (104 × 10 ms).
    pub const MAX_DRED_DURATION_MS: u16 = 1_040;

    pub fn clamped(mut self) -> Self {
        self.channels = if self.channels == 2 { 2 } else { 1 };
        let max = if self.channels == 2 {
            Self::MAX_STEREO_BITRATE
        } else {
            Self::MAX_BITRATE
        };
        self.bitrate_bps = self.bitrate_bps.clamp(Self::MIN_BITRATE, max);
        self.complexity = self.complexity.min(10);
        self.expected_loss_percent = self.expected_loss_percent.min(100);
        self.dred_duration_ms = self.dred_duration_ms.min(Self::MAX_DRED_DURATION_MS) / 10 * 10;
        self
    }

    /// The channel's policy laid over these settings. `complexity` is the app's own choice
    /// (`None`: follow the policy's hint, or keep the current value if it has none); VBR mode
    /// and the channel count stay local, except that a policy without `stereo` forces mono.
    pub fn with_policy(mut self, policy: &AudioPolicy, local_complexity: Option<u8>) -> Self {
        self.bitrate_bps = policy.bitrate_bps;
        self.fec = policy.fec;
        self.dtx = policy.dtx;
        self.max_bandwidth = policy.max_bandwidth;
        self.signal = policy.signal;
        if let Some(c) = local_complexity.or(policy.complexity) {
            self.complexity = c;
        }
        if !policy.stereo {
            self.channels = 1;
        }
        self.clamped()
    }

    /// `Application::Audio` for music, `Voip` otherwise; fixed at encoder creation.
    fn application(&self) -> opus::Application {
        match self.signal {
            OpusSignal::Music => opus::Application::Audio,
            _ => opus::Application::Voip,
        }
    }
}

impl Default for EncoderSettings {
    fn default() -> Self {
        Self {
            bitrate_bps: 32_000,
            complexity: Self::DEFAULT_COMPLEXITY,
            max_bandwidth: OpusBandwidth::Fullband,
            signal: OpusSignal::Voice,
            vbr: true,
            constrained_vbr: true,
            fec: true,
            expected_loss_percent: 5,
            dtx: false,
            channels: 1,
            dred_duration_ms: 0,
        }
    }
}

fn opus_bandwidth(bw: OpusBandwidth) -> opus::Bandwidth {
    match bw {
        OpusBandwidth::Narrowband => opus::Bandwidth::Narrowband,
        OpusBandwidth::Mediumband => opus::Bandwidth::Mediumband,
        OpusBandwidth::Wideband => opus::Bandwidth::Wideband,
        OpusBandwidth::Superwideband => opus::Bandwidth::Superwideband,
        OpusBandwidth::Fullband => opus::Bandwidth::Fullband,
    }
}

fn opus_signal(signal: OpusSignal) -> opus::Signal {
    match signal {
        OpusSignal::Auto => opus::Signal::Auto,
        OpusSignal::Voice => opus::Signal::Voice,
        OpusSignal::Music => opus::Signal::Music,
    }
}

/// Pushes every setting into a libopus encoder; takes effect from the next frame. Returns
/// the clamped settings that were applied. On error the encoder keeps whatever libopus
/// accepted so far.
pub fn apply_encoder_settings(
    e: &mut opus::Encoder,
    settings: EncoderSettings,
) -> Result<EncoderSettings, opus::Error> {
    let s = settings.clamped();
    e.set_signal(opus_signal(s.signal))?;
    e.set_max_bandwidth(opus_bandwidth(s.max_bandwidth))?;
    e.set_complexity(i32::from(s.complexity))?;
    e.set_vbr(s.vbr)?;
    e.set_vbr_constraint(s.constrained_vbr)?;
    e.set_inband_fec(s.fec)?;
    e.set_packet_loss_perc(i32::from(s.expected_loss_percent))?;
    e.set_dtx(s.dtx)?;
    e.set_bitrate(opus::Bitrate::Bits(s.bitrate_bps as i32))?;
    let dred_duration_ms = match e.set_dred_duration(u32::from(s.dred_duration_ms / 10)) {
        Ok(()) => s.dred_duration_ms,
        // A libopus without DRED: plain FEC/PLC, the setting reads back as off.
        Err(err) if err.code() == opus::ErrorCode::Unimplemented => 0,
        Err(err) => return Err(err),
    };
    Ok(EncoderSettings {
        dred_duration_ms,
        ..s
    })
}

/// Whether this libopus build codes DRED (`false`: [`EncoderSettings::dred_duration_ms`] is
/// accepted but stays off and receivers fall back to FEC/PLC).
pub fn dred_supported() -> bool {
    opus::Encoder::new(SAMPLE_RATE, opus::Channels::Mono, opus::Application::Voip)
        .and_then(|mut e| e.set_dred_duration(1))
        .is_ok()
}

/// What libopus reports back, for tests and diagnostics.
pub fn probe_encoder_settings(e: &mut opus::Encoder) -> Result<EncoderSettings, opus::Error> {
    let bitrate_bps = match e.get_bitrate()? {
        opus::Bitrate::Bits(b) => b.max(0) as u32,
        _ => 0,
    };
    let max_bandwidth = match e.get_max_bandwidth()? {
        opus::Bandwidth::Narrowband => OpusBandwidth::Narrowband,
        opus::Bandwidth::Mediumband => OpusBandwidth::Mediumband,
        opus::Bandwidth::Wideband => OpusBandwidth::Wideband,
        opus::Bandwidth::Superwideband => OpusBandwidth::Superwideband,
        opus::Bandwidth::Fullband | opus::Bandwidth::Auto => OpusBandwidth::Fullband,
    };
    let signal = match e.get_signal()? {
        opus::Signal::Voice => OpusSignal::Voice,
        opus::Signal::Music => OpusSignal::Music,
        opus::Signal::Auto => OpusSignal::Auto,
    };
    Ok(EncoderSettings {
        bitrate_bps,
        complexity: e.get_complexity()?.clamp(0, 10) as u8,
        max_bandwidth,
        signal,
        vbr: e.get_vbr()?,
        constrained_vbr: e.get_vbr_constraint()?,
        fec: e.get_inband_fec()?,
        expected_loss_percent: e.get_packet_loss_perc()?.clamp(0, 100) as u8,
        dtx: e.get_dtx()?,
        channels: 1,
        dred_duration_ms: e
            .get_dred_duration()
            .map(|frames| {
                (frames * 10).min(u32::from(EncoderSettings::MAX_DRED_DURATION_MS)) as u16
            })
            .unwrap_or(0),
    })
}

/// Errors from the bare [`OpusEncoder`] / [`OpusDecoder`] wrappers.
#[derive(Debug)]
pub enum CodecError {
    /// Only mono and stereo streams are supported.
    BadChannels(u8),
    /// The PCM slice is not a whole number of interleaved frames.
    BadFrame,
    Opus(opus::Error),
}

impl std::fmt::Display for CodecError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::BadChannels(c) => write!(f, "unsupported channel count {c} (1 or 2)"),
            Self::BadFrame => f.write_str("pcm length is not a multiple of the channel count"),
            Self::Opus(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for CodecError {}

impl From<opus::Error> for CodecError {
    fn from(e: opus::Error) -> Self {
        Self::Opus(e)
    }
}

fn opus_channels(channels: u8) -> Result<opus::Channels, CodecError> {
    match channels {
        1 => Ok(opus::Channels::Mono),
        2 => Ok(opus::Channels::Stereo),
        other => Err(CodecError::BadChannels(other)),
    }
}

fn opus_width(stereo: bool) -> opus::Channels {
    if stereo {
        opus::Channels::Stereo
    } else {
        opus::Channels::Mono
    }
}

/// A bare Opus encoder (any supported rate, mono or stereo) with the same settings model as
/// the capture path; backs the C ABI `aurix_opus_encoder_*` family used by the Unity SDK's
/// native codec.
pub struct OpusEncoder {
    encoder: opus::Encoder,
    channels: usize,
    settings: EncoderSettings,
}

impl OpusEncoder {
    pub fn new(
        sample_rate_hz: u32,
        channels: u8,
        settings: EncoderSettings,
    ) -> Result<Self, CodecError> {
        let ch = opus_channels(channels)?;
        let settings = EncoderSettings {
            channels,
            ..settings
        };
        let mut encoder = opus::Encoder::new(sample_rate_hz, ch, settings.application())?;
        let settings = apply_encoder_settings(&mut encoder, settings)?;
        Ok(Self {
            encoder,
            channels: usize::from(channels),
            settings,
        })
    }

    pub fn settings(&self) -> EncoderSettings {
        self.settings
    }

    pub fn channels(&self) -> usize {
        self.channels
    }

    /// Applies new settings; the channel count is fixed at construction and ignored here.
    pub fn apply(&mut self, settings: EncoderSettings) -> Result<(), opus::Error> {
        let settings = EncoderSettings {
            channels: self.channels as u8,
            ..settings
        };
        self.settings = apply_encoder_settings(&mut self.encoder, settings)?;
        Ok(())
    }

    pub fn probe(&mut self) -> Result<EncoderSettings, opus::Error> {
        Ok(EncoderSettings {
            channels: self.channels as u8,
            ..probe_encoder_settings(&mut self.encoder)?
        })
    }

    /// Encodes one interleaved frame (a valid Opus frame size for the encoder's rate:
    /// 2.5/5/10/20/40/60 ms). Returns the packet length.
    pub fn encode_f32(&mut self, pcm: &[f32], out: &mut [u8]) -> Result<usize, CodecError> {
        if pcm.is_empty() || !pcm.len().is_multiple_of(self.channels) {
            return Err(CodecError::BadFrame);
        }
        Ok(self.encoder.encode_float(pcm, out)?)
    }

    pub fn encode_i16(&mut self, pcm: &[i16], out: &mut [u8]) -> Result<usize, CodecError> {
        if pcm.is_empty() || !pcm.len().is_multiple_of(self.channels) {
            return Err(CodecError::BadFrame);
        }
        Ok(self.encoder.encode(pcm, out)?)
    }
}

/// A bare Opus decoder with packet-loss concealment and FEC recovery; backs the C ABI
/// `aurix_opus_decoder_*` family.
pub struct OpusDecoder {
    decoder: opus::Decoder,
    channels: usize,
    sample_rate: u32,
    /// DRED state, created on the first `dred_decode_*` call; `None` when the build has no DRED.
    dred: Option<Box<DecoderDred>>,
}

struct DecoderDred {
    decoder: opus::DredDecoder,
    data: opus::Dred,
    /// FNV-1a of the packet the redundancy was parsed from, and the history it can rebuild.
    parsed: Option<(u64, usize)>,
}

fn fnv1a(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325u64, |h, &b| {
        (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3)
    })
}

impl OpusDecoder {
    pub fn new(sample_rate_hz: u32, channels: u8) -> Result<Self, CodecError> {
        let ch = opus_channels(channels)?;
        Ok(Self {
            decoder: opus::Decoder::new(sample_rate_hz, ch)?,
            channels: usize::from(channels),
            sample_rate: sample_rate_hz,
            dred: None,
        })
    }

    pub fn channels(&self) -> usize {
        self.channels
    }

    /// Retune the decoder: complexity `>= 5` conceals lost frames with the neural PLC, `>= 6`
    /// enhances decoded speech (OSCE); `osce_bwe` extends 16 kHz speech to full band.
    pub fn apply(&mut self, settings: DecoderSettings) -> Result<(), CodecError> {
        Ok(settings.clamped().apply(&mut self.decoder)?)
    }

    pub fn settings(&mut self) -> DecoderSettings {
        DecoderSettings {
            complexity: self.decoder.get_complexity().unwrap_or(0).clamp(0, 10) as u8,
            osce_bwe: self.decoder.get_osce_bwe().unwrap_or(false),
        }
    }

    /// Rebuild the frame `frames_before` frames before `packet` (1 = the one right before it)
    /// from the packet's Deep REDundancy into one frame of `pcm.len() / channels` samples.
    /// Returns samples per channel, `Ok(0)` when the packet's DRED does not reach that far (or
    /// this build has no DRED) — run PLC then.
    pub fn dred_decode_f32(
        &mut self,
        packet: &[u8],
        frames_before: usize,
        pcm: &mut [f32],
    ) -> Result<usize, CodecError> {
        if pcm.is_empty() || !pcm.len().is_multiple_of(self.channels) || frames_before == 0 {
            return Err(CodecError::BadFrame);
        }
        let frame = pcm.len() / self.channels;
        let offset = frames_before * frame;
        if self.dred.is_none() {
            self.dred = opus::DredDecoder::new()
                .and_then(|decoder| {
                    Ok(Box::new(DecoderDred {
                        decoder,
                        data: opus::Dred::new()?,
                        parsed: None,
                    }))
                })
                .ok();
        }
        let Some(st) = self.dred.as_deref_mut() else {
            return Ok(0);
        };
        let key = fnv1a(packet);
        let available = match st.parsed {
            Some((k, n)) if k == key => n,
            _ => {
                let max = (offset + 4 * frame).min(self.sample_rate as usize);
                let n = st
                    .decoder
                    .parse(&mut st.data, packet, max, self.sample_rate)
                    .unwrap_or(0);
                st.parsed = Some((key, n));
                n
            }
        };
        if available < offset {
            return Ok(0);
        }
        Ok(self.decoder.dred_decode_float(&st.data, offset, pcm)?)
    }

    /// [`Self::dred_decode_f32`] with 16-bit output.
    pub fn dred_decode_i16(
        &mut self,
        packet: &[u8],
        frames_before: usize,
        pcm: &mut [i16],
    ) -> Result<usize, CodecError> {
        if pcm.is_empty() || !pcm.len().is_multiple_of(self.channels) || frames_before == 0 {
            return Err(CodecError::BadFrame);
        }
        let mut tmp = vec![0f32; pcm.len()];
        let n = self.dred_decode_f32(packet, frames_before, &mut tmp)?;
        for (o, s) in pcm.iter_mut().zip(&tmp[..n * self.channels]) {
            *o = (s.clamp(-1.0, 1.0) * 32767.0) as i16;
        }
        Ok(n)
    }

    /// Decodes `packet` into interleaved f32 PCM; an empty packet runs PLC for one frame of
    /// `pcm.len() / channels` samples. `fec` asks for the in-band FEC of the *next* packet
    /// (pass the following packet to recover the lost one). Returns samples per channel.
    pub fn decode_f32(
        &mut self,
        packet: &[u8],
        pcm: &mut [f32],
        fec: bool,
    ) -> Result<usize, CodecError> {
        if pcm.is_empty() || !pcm.len().is_multiple_of(self.channels) {
            return Err(CodecError::BadFrame);
        }
        Ok(self.decoder.decode_float(packet, pcm, fec)?)
    }

    pub fn decode_i16(
        &mut self,
        packet: &[u8],
        pcm: &mut [i16],
        fec: bool,
    ) -> Result<usize, CodecError> {
        if pcm.is_empty() || !pcm.len().is_multiple_of(self.channels) {
            return Err(CodecError::BadFrame);
        }
        Ok(self.decoder.decode(packet, pcm, fec)?)
    }
}

/// Turns arbitrary captured PCM (any rate, 1..=8 interleaved channels, i16 or f32) into
/// 20 ms frames — 48 kHz Opus (mono, or stereo when [`EncoderSettings::channels`] is 2), or
/// 8 kHz mono μ-law when the session codec is PCMU — with input gain and VAD metering applied.
///
/// Mono encoding averages every input channel. Stereo encoding keeps the first two input
/// channels as L/R (a mono source is duplicated) and meters VAD / energy on their average;
/// the capture DSP (AEC / NS / AGC) is voice-only and is bypassed for stereo frames.
pub struct CaptureEncoder {
    encoder: opus::Encoder,
    /// Channels the current `encoder` was created with.
    encoder_channels: u8,
    music: bool,
    g711: G711Encoder,
    codec: AudioCodec,
    /// One resampler per encoded channel.
    resamplers: Vec<Resampler>,
    /// De-interleaved input per encoded channel (scratch).
    split: Vec<Vec<f32>>,
    /// Resampled 48 kHz samples per encoded channel waiting for a whole frame.
    pending: Vec<Vec<f32>>,
    /// Interleaved stereo frame scratch.
    interleaved: Vec<f32>,
    /// Downmix of a stereo frame for VAD / energy metering.
    meter: Vec<f32>,
    gain: f32,
    pub vad: VoiceActivityDetector,
    /// Capture DSP (high-pass / AEC / NS / AGC), bypassed until configured.
    pub dsp: Dsp,
    /// Voice effects applied after the DSP and gain, before VAD metering and encoding.
    pub effects: EffectChain,
    /// Lip-sync of our own voice as sent (after DSP, gain and effects), when enabled.
    visemes: Option<Box<VisemeAnalyzer>>,
    settings: EncoderSettings,
    out: [u8; 1275],
    g711_out: Vec<u8>,
}

impl CaptureEncoder {
    pub fn new(settings: EncoderSettings) -> Result<Self, opus::Error> {
        let settings = settings.clamped();
        let encoder = Self::make_encoder(&settings)?;
        let mut this = Self {
            encoder,
            encoder_channels: settings.channels,
            music: settings.signal == OpusSignal::Music,
            g711: G711Encoder::new(),
            codec: AudioCodec::Opus,
            resamplers: Vec::new(),
            split: Vec::new(),
            pending: Vec::new(),
            interleaved: vec![0.0; FRAME_SAMPLES * 2],
            meter: vec![0.0; FRAME_SAMPLES],
            gain: 1.0,
            vad: VoiceActivityDetector::default(),
            dsp: Dsp::new(DspConfig::BYPASS),
            effects: EffectChain::default(),
            visemes: None,
            settings,
            out: [0u8; 1275],
            g711_out: Vec::with_capacity(PCMU_FRAME_SAMPLES),
        };
        this.apply(this.settings)?;
        Ok(this)
    }

    fn make_encoder(settings: &EncoderSettings) -> Result<opus::Encoder, opus::Error> {
        let ch = if settings.channels == 2 {
            opus::Channels::Stereo
        } else {
            opus::Channels::Mono
        };
        opus::Encoder::new(SAMPLE_RATE, ch, settings.application())
    }

    /// Which codec the produced frames use. Switching resets both codecs' state so the first
    /// frame after the switch does not carry the previous one's history.
    pub fn set_codec(&mut self, codec: AudioCodec) {
        if self.codec != codec {
            self.codec = codec;
            self.g711.reset();
            let _ = self.encoder.reset_state();
        }
    }

    pub fn codec(&self) -> AudioCodec {
        self.codec
    }

    /// Channels of the frames currently produced: `settings.channels` for Opus, always 1
    /// for G.711.
    pub fn channels(&self) -> u8 {
        match self.codec {
            AudioCodec::Opus => self.settings.channels,
            AudioCodec::Pcmu | AudioCodec::Pcma => 1,
        }
    }

    pub fn bitrate(&self) -> u32 {
        self.settings.bitrate_bps
    }

    pub fn set_bitrate(&mut self, bitrate_bps: u32) -> Result<(), opus::Error> {
        self.apply(EncoderSettings {
            bitrate_bps,
            ..self.settings
        })
    }

    pub fn settings(&self) -> EncoderSettings {
        self.settings
    }

    /// Pushes every setting into libopus; takes effect from the next frame. A change of the
    /// channel count or of the music/voice application recreates the encoder (the receiver's
    /// decoder follows the packets, so nothing else has to be renegotiated). On error the
    /// encoder keeps whatever libopus accepted so far and `settings()` is unchanged.
    pub fn apply(&mut self, settings: EncoderSettings) -> Result<(), opus::Error> {
        let settings = settings.clamped();
        let music = settings.signal == OpusSignal::Music;
        if settings.channels != self.encoder_channels || music != self.music {
            let mut encoder = Self::make_encoder(&settings)?;
            let applied = apply_encoder_settings(&mut encoder, settings)?;
            self.encoder = encoder;
            self.encoder_channels = settings.channels;
            self.music = music;
            self.settings = applied;
            return Ok(());
        }
        self.settings = apply_encoder_settings(&mut self.encoder, settings)?;
        Ok(())
    }

    /// What libopus reports back, for tests and diagnostics.
    pub fn probe(&mut self) -> Result<EncoderSettings, opus::Error> {
        Ok(EncoderSettings {
            channels: self.encoder_channels,
            ..probe_encoder_settings(&mut self.encoder)?
        })
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
        let enc = usize::from(self.channels());
        // A change of encoded width or input rate restarts the resamplers (a few ms of
        // capture are dropped, no clicks: every frame handed on is still whole).
        if self.pending.len() != enc
            || self
                .resamplers
                .first()
                .is_some_and(|r| r.from != sample_rate)
        {
            self.resamplers = (0..enc)
                .map(|_| Resampler::new(sample_rate, SAMPLE_RATE))
                .collect();
            self.split = (0..enc).map(|_| Vec::with_capacity(pcm.len())).collect();
            self.pending = (0..enc)
                .map(|_| Vec::with_capacity(FRAME_SAMPLES * 4))
                .collect();
        }
        for s in &mut self.split {
            s.clear();
        }
        for frame in pcm.chunks_exact(channels) {
            if enc == 1 {
                let sum: f32 = frame.iter().sum();
                self.split[0].push(sum / channels as f32);
            } else if channels == 1 {
                self.split[0].push(frame[0]);
                self.split[1].push(frame[0]);
            } else {
                self.split[0].push(frame[0]);
                self.split[1].push(frame[1]);
            }
        }
        for ((r, input), pending) in self
            .resamplers
            .iter_mut()
            .zip(&self.split)
            .zip(&mut self.pending)
        {
            r.push(input, pending);
        }
        let ready = self.pending.iter().map(Vec::len).min().unwrap_or(0) / FRAME_SAMPLES;
        for i in 0..ready {
            let offset = i * FRAME_SAMPLES;
            if enc == 1 {
                let frame = &mut self.pending[0][offset..offset + FRAME_SAMPLES];
                self.dsp.process_frame(frame);
                if self.gain != 1.0 {
                    for s in frame.iter_mut() {
                        *s = (*s * self.gain).clamp(-1.0, 1.0);
                    }
                }
                self.effects.process(frame, 1);
                if let Some(v) = &mut self.visemes {
                    v.push(frame, 1);
                }
                self.vad.process(frame);
                let energy = rms(frame);
                match self.codec {
                    AudioCodec::Opus => match self.encoder.encode_float(frame, &mut self.out) {
                        Ok(n) if n > 0 => sink(EncodedFrame {
                            payload: self.out[..n].to_vec(),
                            codec: AudioCodec::Opus,
                            level: self.vad.level(),
                            energy,
                            speech: self.vad.speaking(),
                        }),
                        _ => {}
                    },
                    codec @ (AudioCodec::Pcmu | AudioCodec::Pcma) => {
                        let law = codec.g711_law().unwrap_or(Law::Mu);
                        self.g711_out.clear();
                        self.g711.encode(law, frame, &mut self.g711_out);
                        sink(EncodedFrame {
                            payload: self.g711_out.clone(),
                            codec,
                            level: self.vad.level(),
                            energy,
                            speech: self.vad.speaking(),
                        });
                    }
                }
            } else {
                let left = &self.pending[0][offset..offset + FRAME_SAMPLES];
                let right = &self.pending[1][offset..offset + FRAME_SAMPLES];
                for (n, (l, r)) in left.iter().zip(right).enumerate() {
                    let (l, r) = if self.gain != 1.0 {
                        (
                            (l * self.gain).clamp(-1.0, 1.0),
                            (r * self.gain).clamp(-1.0, 1.0),
                        )
                    } else {
                        (*l, *r)
                    };
                    self.interleaved[n * 2] = l;
                    self.interleaved[n * 2 + 1] = r;
                }
                self.effects.process(&mut self.interleaved, 2);
                if let Some(v) = &mut self.visemes {
                    v.push(&self.interleaved, 2);
                }
                for (n, [l, r]) in self.interleaved.as_chunks::<2>().0.iter().enumerate() {
                    self.meter[n] = 0.5 * (l + r);
                }
                self.vad.process(&self.meter);
                let energy = rms(&self.meter);
                if let Ok(n) = self.encoder.encode_float(&self.interleaved, &mut self.out) {
                    if n > 0 {
                        sink(EncodedFrame {
                            payload: self.out[..n].to_vec(),
                            codec: AudioCodec::Opus,
                            level: self.vad.level(),
                            energy,
                            speech: self.vad.speaking(),
                        });
                    }
                }
            }
        }
        for pending in &mut self.pending {
            pending.drain(..ready * FRAME_SAMPLES);
        }
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

    /// Analyse (or stop analysing) the outgoing voice for lip-sync; read [`Self::visemes`].
    pub fn set_visemes(&mut self, enabled: bool) {
        match (&self.visemes, enabled) {
            (None, true) => self.visemes = Some(Box::default()),
            (Some(_), false) => self.visemes = None,
            _ => {}
        }
    }

    /// Mouth state of the last encoded frame, `None` unless [`Self::set_visemes`] is on.
    pub fn visemes(&self) -> Option<VisemeFrame> {
        self.visemes.as_ref().map(|v| v.frame())
    }

    /// Drop buffered samples (device switch, unmute after a long pause).
    pub fn reset(&mut self) {
        self.pending.clear();
        self.resamplers.clear();
        self.vad.reset();
        self.effects.reset();
        if let Some(v) = &mut self.visemes {
            v.reset();
        }
        let _ = self.encoder.reset_state();
        self.g711.reset();
    }
}

/// Per-sender jitter buffer: reorders by sequence, absorbs jitter with a small target depth and
/// hands frames (or loss markers for PLC) to the decoder at a steady 20 ms cadence.
#[derive(Debug)]
pub struct JitterBuffer<F = Vec<u8>> {
    frames: BTreeMap<u32, F>,
    target_depth: usize,
    max_depth: usize,
    next_seq: u32,
    started: bool,
    pub lost: u64,
    pub late: u64,
}

#[derive(Debug, PartialEq)]
pub enum JitterSlot<F = Vec<u8>> {
    /// Nothing to play yet (still filling or starved).
    Wait,
    Frame(F),
    /// The packet for this slot is missing; run PLC.
    Lost,
}

impl<F> JitterBuffer<F> {
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

    /// Sequence of the slot the next [`Self::pop`] serves (the slot just returned is one
    /// below).
    pub fn next_seq(&self) -> u32 {
        self.next_seq
    }

    /// Oldest buffered frame and its sequence — after a [`JitterSlot::Lost`], the packet
    /// whose in-band FEC / DRED may rebuild the missing slot.
    pub fn peek(&self) -> Option<(u32, &F)> {
        self.frames.iter().next().map(|(&seq, f)| (seq, f))
    }

    fn seq_before(a: u32, b: u32) -> bool {
        (a.wrapping_sub(b) as i32) < 0
    }

    pub fn push(&mut self, seq: u32, frame: F) {
        if self.started {
            if (seq.wrapping_sub(self.next_seq) as i32).unsigned_abs() > JITTER_RESYNC_GAP {
                self.reset();
            } else if Self::seq_before(seq, self.next_seq) {
                self.late += 1;
                return;
            }
        }
        self.frames.insert(seq, frame);
        if self.frames.len() > self.max_depth {
            while self.frames.len() > self.target_depth {
                self.frames.pop_first();
            }
            if let Some(&first) = self.frames.keys().next() {
                self.next_seq = first;
            }
        }
    }

    pub fn pop(&mut self) -> JitterSlot<F> {
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

/// One downlink frame waiting in a jitter buffer.
#[derive(Debug, PartialEq)]
struct WireFrame {
    codec: AudioCodec,
    data: Vec<u8>,
}

struct Stream {
    jitter: JitterBuffer<WireFrame>,
    decoder: opus::Decoder,
    g711: G711Decoder,
    /// DRED parsed from the packet that closes the current gap, reused for each lost frame
    /// of that gap.
    dred: Option<Box<StreamDred>>,
    fec_recovered: u64,
    dred_recovered: u64,
    /// Codec of the last frame decoded; the concealment path follows it.
    codec: AudioCodec,
    /// The Opus decoder is stereo and `frame` holds interleaved L/R: a server-mixed stream,
    /// or a sender whose packets carry two channels (a stream upgrades on its first stereo
    /// packet and stays stereo — libopus upmixes any later mono packet).
    stereo: bool,
    /// Server-mixed stream: never panned, restarts its decoder if the flag flips.
    mixed: bool,
    /// Has a direction: a mono frame is panned, a stereo frame is downmixed and then panned.
    panned: bool,
    volume: f32,
    left: f32,
    right: f32,
    frame: Vec<f32>,
    pos: usize,
    len: usize,
    last_activity: Instant,
    starved: bool,
    starved_at: Instant,
    /// Lip-sync of this stream's decoded audio, when the mixer analyses visemes.
    visemes: Option<Box<VisemeAnalyzer>>,
    /// Last time the analyser was fed; gaps feed it silence once per frame period at most.
    viseme_fed_at: Instant,
}

struct StreamDred {
    data: opus::Dred,
    /// Packet the redundancy was parsed from.
    packet_seq: u32,
    /// Samples before that packet the redundancy can rebuild.
    samples: usize,
}

impl StreamDred {
    /// The redundancy of `packet` (sequence `seq`), parsed once per packet for up to
    /// `max_samples` of history; `None` when this build has no DRED state.
    fn parse_once<'a>(
        slot: &'a mut Option<Box<StreamDred>>,
        dec: &mut opus::DredDecoder,
        seq: u32,
        packet: &[u8],
        max_samples: usize,
    ) -> Option<&'a mut StreamDred> {
        if slot.is_none() {
            *slot = opus::Dred::new().ok().map(|data| {
                Box::new(StreamDred {
                    data,
                    packet_seq: seq.wrapping_sub(1),
                    samples: 0,
                })
            });
        }
        let st = slot.as_deref_mut()?;
        if st.packet_seq != seq {
            st.samples = dec
                .parse(&mut st.data, packet, max_samples, SAMPLE_RATE)
                .unwrap_or(0);
            st.packet_seq = seq;
        }
        Some(st)
    }
}

/// Longest gap DRED is asked to bridge, in 20 ms frames (libopus codes up to ~1 s).
const MAX_DRED_GAP_FRAMES: usize = 52;

/// History to request from a packet's DRED for a gap of `gap` frames: the redundancy is
/// coded with an encoder-side offset of a few frames, so asking for exactly the gap decodes
/// too few latents; asking for a little more costs only decoder time.
fn dred_history_samples(gap: usize) -> usize {
    ((gap + 4) * FRAME_SAMPLES).min(SAMPLE_RATE as usize)
}

/// Tuning of every remote stream's Opus decoder (libopus 1.5+ neural paths).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DecoderSettings {
    /// `0..=10`. `>= 5` conceals lost frames with the neural PLC instead of the classic
    /// one; `>= 6` also runs OSCE LACE speech enhancement on SILK frames, `>= 7` NoLACE
    /// (best quality, several times the CPU of LACE).
    pub complexity: u8,
    /// OSCE bandwidth extension: wideband SILK speech is widened to fullband (needs
    /// `complexity >= 4`).
    pub osce_bwe: bool,
}

impl DecoderSettings {
    /// Neural PLC without OSCE — the default balance for many concurrent streams.
    pub const DEFAULT_COMPLEXITY: u8 = 5;

    pub fn clamped(mut self) -> Self {
        self.complexity = self.complexity.min(10);
        self
    }

    /// Configures one libopus decoder; a build without OSCE keeps `osce_bwe` off.
    fn apply(self, d: &mut opus::Decoder) -> Result<(), opus::Error> {
        d.set_complexity(i32::from(self.complexity))?;
        match d.set_osce_bwe(self.osce_bwe) {
            Ok(()) => Ok(()),
            Err(err) if err.code() == opus::ErrorCode::Unimplemented => Ok(()),
            Err(err) => Err(err),
        }
    }
}

impl Default for DecoderSettings {
    fn default() -> Self {
        Self {
            complexity: Self::DEFAULT_COMPLEXITY,
            osce_bwe: false,
        }
    }
}

/// Statistics of one decoded sender stream.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct StreamStats {
    pub ssrc: u32,
    pub buffered_frames: usize,
    pub lost: u64,
    pub late: u64,
    /// Lost frames rebuilt from the following packet's in-band FEC / DRED (subsets of
    /// `lost`); the rest were concealed.
    pub fec_recovered: u64,
    pub dred_recovered: u64,
    /// Decoded two-wide (a stereo uplink or a server mix).
    pub stereo: bool,
    /// Server-mixed channel stream (`PacketFlags::Mixed`), never a single participant.
    pub mixed: bool,
    /// Ran dry: nothing buffered and nothing arriving (between talk spurts or gone).
    pub starved: bool,
}

/// Downlink counters accumulated over every stream the mixer has ever seen.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct MixerTotals {
    /// Frames the jitter buffers declared lost (rebuilt from FEC / DRED or concealed).
    pub lost: u64,
    /// Frames that arrived after their play-out time and were discarded.
    pub late: u64,
    /// Times a stream ran dry mid-talk-spurt (a packet followed within
    /// [`UNDERRUN_RESUME_WINDOW`]); the natural end of a spurt is not counted.
    pub underruns: u64,
    /// Lost frames rebuilt from the next packet's in-band FEC (subset of `lost`).
    pub fec_recovered: u64,
    /// Lost frames rebuilt from Deep REDundancy (subset of `lost`).
    pub dred_recovered: u64,
}

impl MixerTotals {
    fn absorb(&mut self, s: &Stream) {
        self.lost += s.jitter.lost;
        self.late += s.jitter.late;
        self.fec_recovered += s.fec_recovered;
        self.dred_recovered += s.dred_recovered;
    }
}

/// A stream that starves and receives its next frame within this window ran dry because
/// of the network, not because the speaker paused (VAD hangover is longer than this).
pub const UNDERRUN_RESUME_WINDOW: Duration = Duration::from_millis(250);

/// Decodes and mixes every remote sender into one interleaved float buffer. Each sender has
/// its own decoders because decoder state is per-stream; Opus and PCMU frames of the same
/// sender (around a codec switch) are decoded by whichever codec each frame is flagged with.
/// Safe to drive from the audio thread: pushes only touch a `BTreeMap` behind the caller's
/// lock.
pub struct RemoteMixer {
    streams: HashMap<u32, Stream>,
    /// Streams an engine plays itself through [`Self::pull`]; [`Self::mix`] skips them.
    claimed: HashSet<u32>,
    output_volume: f32,
    output_muted: bool,
    target_depth: usize,
    max_depth: usize,
    scratch: Vec<f32>,
    /// Lost/late of streams already dropped, so totals never go backwards.
    retired: MixerTotals,
    underruns: u64,
    /// Every stream gets a [`VisemeAnalyzer`] fed from its decoded frames.
    visemes: bool,
    decoder: DecoderSettings,
    /// Shared DRED extractor; `None` when this libopus has no DRED (gaps get FEC/PLC only).
    dred: Option<opus::DredDecoder>,
}

impl RemoteMixer {
    pub fn new(target_depth: usize, max_depth: usize) -> Self {
        Self {
            streams: HashMap::new(),
            claimed: HashSet::new(),
            output_volume: 1.0,
            output_muted: false,
            target_depth,
            max_depth,
            scratch: Vec::new(),
            retired: MixerTotals::default(),
            underruns: 0,
            visemes: false,
            decoder: DecoderSettings::default(),
            dred: opus::DredDecoder::new().ok(),
        }
    }

    /// Retune every stream's decoder (existing and future ones).
    pub fn set_decoder_settings(&mut self, settings: DecoderSettings) -> Result<(), opus::Error> {
        self.decoder = settings.clamped();
        for s in self.streams.values_mut() {
            self.decoder.apply(&mut s.decoder)?;
        }
        Ok(())
    }

    pub fn decoder_settings(&self) -> DecoderSettings {
        self.decoder
    }

    /// Whether lost frames can be rebuilt from DRED with this libopus build.
    pub fn dred_supported(&self) -> bool {
        self.dred.is_some()
    }

    fn new_decoder(&self, stereo: bool) -> Result<opus::Decoder, opus::Error> {
        let mut d = opus::Decoder::new(SAMPLE_RATE, opus_width(stereo))?;
        self.decoder.apply(&mut d)?;
        Ok(d)
    }

    /// Analyse every stream's decoded audio for lip-sync (see [`Self::visemes`]). Streams
    /// are analysed as they are rendered by [`Self::mix`] / [`Self::pull`]; a stream nobody
    /// plays keeps its last state.
    pub fn set_visemes(&mut self, enabled: bool) {
        self.visemes = enabled;
        for s in self.streams.values_mut() {
            match (&s.visemes, enabled) {
                (None, true) => s.visemes = Some(Box::default()),
                (Some(_), false) => s.visemes = None,
                _ => {}
            }
        }
    }

    pub fn visemes_enabled(&self) -> bool {
        self.visemes
    }

    /// Mouth state of `ssrc`: `None` for an unknown stream or with analysis off.
    pub fn visemes(&self, ssrc: u32) -> Option<VisemeFrame> {
        self.streams
            .get(&ssrc)
            .and_then(|s| s.visemes.as_ref())
            .map(|v| v.frame())
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

    /// Replace the set of SSRCs [`Self::mix`] leaves alone because they are rendered through
    /// [`Self::pull`] (a spatialized per-participant source). Frames of claimed streams stay in
    /// their jitter buffers until pulled.
    pub fn set_claimed(&mut self, ssrcs: impl IntoIterator<Item = u32>) {
        self.claimed.clear();
        self.claimed.extend(ssrcs);
    }

    pub fn claimed(&self) -> impl Iterator<Item = u32> + '_ {
        self.claimed.iter().copied()
    }

    /// Queue one verified downlink Opus frame. `volume` is the server-applied gain byte and
    /// `direction` the speaker's bearing relative to this listener (`None` = centred).
    pub fn push(
        &mut self,
        ssrc: u32,
        seq: u32,
        volume: f32,
        direction: Option<Direction>,
        opus: Vec<u8>,
    ) -> Result<(), opus::Error> {
        self.push_frame(ssrc, seq, volume, direction, AudioCodec::Opus, opus)
    }

    /// [`Self::push`] for a frame in either codec.
    pub fn push_frame(
        &mut self,
        ssrc: u32,
        seq: u32,
        volume: f32,
        direction: Option<Direction>,
        codec: AudioCodec,
        data: Vec<u8>,
    ) -> Result<(), opus::Error> {
        self.push_wire_frame(ssrc, seq, volume, direction, codec, false, data)
    }

    /// [`Self::push_frame`] with `mixed` set for a server-mixed stream (`PacketFlags::Mixed`
    /// with Opus): the payload is decoded as interleaved stereo and played without panning.
    /// A stream that flips the flag restarts its decoder. Any other Opus stream whose packet
    /// TOC says stereo (a stereo uplink) switches to a stereo decoder as well.
    #[allow(clippy::too_many_arguments)]
    pub fn push_wire_frame(
        &mut self,
        ssrc: u32,
        seq: u32,
        volume: f32,
        direction: Option<Direction>,
        codec: AudioCodec,
        mixed: bool,
        data: Vec<u8>,
    ) -> Result<(), opus::Error> {
        let stereo_packet = codec == AudioCodec::Opus && opus_packet_is_stereo(&data);
        let stereo = mixed || stereo_packet;
        if let Some(s) = self.streams.get_mut(&ssrc) {
            let want_stereo = if s.mixed != mixed {
                stereo
            } else {
                s.stereo || stereo
            };
            if s.mixed != mixed || s.stereo != want_stereo {
                let mut decoder = opus::Decoder::new(SAMPLE_RATE, opus_width(want_stereo))?;
                self.decoder.apply(&mut decoder)?;
                s.decoder = decoder;
                s.stereo = want_stereo;
                s.mixed = mixed;
                s.pos = 0;
                s.len = 0;
            }
        }
        let visemes = self.visemes;
        let stream = match self.streams.get_mut(&ssrc) {
            Some(s) => s,
            None => {
                let decoder = self.new_decoder(stereo)?;
                self.streams.entry(ssrc).or_insert(Stream {
                    jitter: JitterBuffer::new(self.target_depth, self.max_depth),
                    decoder,
                    g711: G711Decoder::new(),
                    dred: None,
                    fec_recovered: 0,
                    dred_recovered: 0,
                    codec,
                    stereo,
                    mixed,
                    panned: false,
                    volume: 1.0,
                    left: 1.0,
                    right: 1.0,
                    frame: vec![0.0; MAX_DECODE_SAMPLES * 2],
                    pos: 0,
                    len: 0,
                    last_activity: Instant::now(),
                    starved: false,
                    starved_at: Instant::now(),
                    visemes: visemes.then(Box::default),
                    viseme_fed_at: Instant::now() - FRAME_DURATION,
                })
            }
        };
        stream.volume = volume;
        match direction {
            Some(d) if !mixed => {
                (stream.left, stream.right) = d.stereo_gains();
                stream.panned = true;
            }
            _ => {
                (stream.left, stream.right) = (1.0, 1.0);
                stream.panned = false;
            }
        }
        let now = Instant::now();
        stream.last_activity = now;
        if stream.starved {
            let resumed = now.duration_since(stream.starved_at) < UNDERRUN_RESUME_WINDOW;
            let bridged = resumed
                && codec == AudioCodec::Opus
                && stream.codec == AudioCodec::Opus
                && Self::packet_bridges_gap(stream, self.dred.as_mut(), seq, &data);
            if !bridged {
                // A talk spurt after silence: refill the target depth before playing again.
                stream.jitter.reset();
            }
            stream.starved = false;
            if let Some(v) = &mut stream.visemes {
                v.reset();
            }
            if resumed {
                self.underruns += 1;
            }
        }
        stream.jitter.push(seq, WireFrame { codec, data });
        Ok(())
    }

    /// After the buffer ran dry mid-spurt, whether the packet that ends the gap can rebuild
    /// the frames missed meanwhile (in-band FEC for a single one, DRED for a burst). Then the
    /// stream keeps its sequence — the gap plays late, rebuilt, until the next pause re-syncs
    /// it — instead of restarting and dropping the frames for good. A packet that cannot
    /// (no redundancy, gap too long or out of order) restarts the stream as before.
    fn packet_bridges_gap(
        s: &mut Stream,
        dred_dec: Option<&mut opus::DredDecoder>,
        seq: u32,
        packet: &[u8],
    ) -> bool {
        if !s.jitter.is_empty() {
            return true;
        }
        let gap = seq.wrapping_sub(s.jitter.next_seq()) as usize;
        if gap == 0 || gap > MAX_DRED_GAP_FRAMES {
            return false;
        }
        if gap == 1 && opus::packet::has_lbrr(packet).unwrap_or(false) {
            return true;
        }
        let Some(dd) = dred_dec else {
            return false;
        };
        StreamDred::parse_once(&mut s.dred, dd, seq, packet, dred_history_samples(gap))
            .is_some_and(|st| st.samples >= FRAME_SAMPLES)
    }

    pub fn remove(&mut self, ssrc: u32) {
        if let Some(s) = self.streams.remove(&ssrc) {
            self.retire(&s);
        }
    }

    pub fn clear(&mut self) {
        for (_, s) in self.streams.drain() {
            self.retired.absorb(&s);
        }
        self.claimed.clear();
    }

    /// Keep the streams (decoders, gains) but drop buffered frames and sequence expectations:
    /// after a move to another node every downlink stream restarts its numbering.
    pub fn resync(&mut self) {
        for s in self.streams.values_mut() {
            s.jitter.reset();
            s.pos = 0;
            s.len = 0;
        }
    }

    fn retire(&mut self, s: &Stream) {
        self.retired.absorb(s);
    }

    pub fn stream_stats(&self) -> Vec<StreamStats> {
        self.streams
            .iter()
            .map(|(&ssrc, s)| StreamStats {
                ssrc,
                buffered_frames: s.jitter.len(),
                lost: s.jitter.lost,
                late: s.jitter.late,
                fec_recovered: s.fec_recovered,
                dred_recovered: s.dred_recovered,
                stereo: s.stereo,
                mixed: s.mixed,
                starved: s.starved,
            })
            .collect()
    }

    /// Lifetime downlink totals: live streams plus everything already retired.
    pub fn totals(&self) -> MixerTotals {
        let mut t = self.retired;
        for s in self.streams.values() {
            t.absorb(s);
        }
        t.underruns = self.underruns;
        t
    }

    fn master_gain(&self) -> f32 {
        if self.output_muted {
            0.0
        } else {
            self.output_volume
        }
    }

    fn expire_idle(&mut self, now: Instant) {
        let mut retired = self.retired;
        self.streams.retain(|_, s| {
            let keep = now.duration_since(s.last_activity) < STREAM_IDLE_TIMEOUT;
            if !keep {
                retired.absorb(s);
            }
            keep
        });
        self.retired = retired;
    }

    /// Fill a lost Opus slot: the packet closing the gap rebuilds it from in-band FEC (the
    /// slot right before that packet) or DRED (any slot within the redundancy it carries);
    /// otherwise the decoder conceals. Returns samples per channel.
    fn conceal_opus(
        s: &mut Stream,
        dred_dec: Option<&mut opus::DredDecoder>,
    ) -> Result<usize, opus::Error> {
        let width = if s.stereo { 2 } else { 1 };
        let out = &mut s.frame[..FRAME_SAMPLES * width];
        let lost_seq = s.jitter.next_seq().wrapping_sub(1);
        if let Some((next_seq, next)) = s.jitter.peek() {
            let gap = next_seq.wrapping_sub(lost_seq) as usize;
            if next.codec == AudioCodec::Opus && gap >= 1 {
                if gap == 1 && opus::packet::has_lbrr(&next.data).unwrap_or(false) {
                    let n = s.decoder.decode_float(&next.data, out, true)?;
                    s.fec_recovered += 1;
                    return Ok(n);
                }
                if let Some(dd) = dred_dec.filter(|_| gap <= MAX_DRED_GAP_FRAMES) {
                    let offset = gap * FRAME_SAMPLES;
                    if let Some(st) = StreamDred::parse_once(
                        &mut s.dred,
                        dd,
                        next_seq,
                        &next.data,
                        dred_history_samples(gap),
                    )
                    .filter(|st| st.samples >= offset)
                    {
                        let n = s.decoder.dred_decode_float(&st.data, offset, out)?;
                        s.dred_recovered += 1;
                        return Ok(n);
                    }
                }
            }
        }
        s.decoder.decode_float(&[], out, false)
    }

    /// Render one stream **into** `output` (added). With `pan` a directional stream is panned
    /// between the first two channels; without it every stream is delivered centred (the
    /// caller spatializes). Returns `(frames written, contributed audible samples)`.
    fn render_stream(
        s: &mut Stream,
        dred: Option<&mut opus::DredDecoder>,
        output: &mut [f32],
        channels: usize,
        master: f32,
        pan_allowed: bool,
        now: Instant,
    ) -> (usize, bool) {
        let frames_needed = output.len() / channels;
        let mut written = 0;
        let mut contributed = false;
        let mut dred = dred;
        while written < frames_needed {
            if s.pos >= s.len {
                let slot = s.jitter.pop();
                let n = match slot {
                    JitterSlot::Wait => {
                        if s.jitter.is_empty() && !s.starved {
                            s.starved = true;
                            s.starved_at = now;
                        }
                        break;
                    }
                    JitterSlot::Frame(WireFrame { codec, data }) => {
                        s.codec = codec;
                        match codec {
                            AudioCodec::Opus => s.decoder.decode_float(&data, &mut s.frame, false),
                            AudioCodec::Pcmu | AudioCodec::Pcma => {
                                let law = codec.g711_law().unwrap_or(Law::Mu);
                                Ok(s.g711.decode(law, &data, &mut s.frame).unwrap_or(0))
                            }
                        }
                    }
                    JitterSlot::Lost => match s.codec {
                        AudioCodec::Opus => Self::conceal_opus(s, dred.as_deref_mut()),
                        AudioCodec::Pcmu | AudioCodec::Pcma => Ok(s.g711.conceal(&mut s.frame)),
                    },
                };
                // G.711 is always mono, even on a stream that also carried stereo Opus.
                s.len = n.unwrap_or(0);
                s.pos = 0;
                if s.len == 0 {
                    break;
                }
                if let Some(v) = &mut s.visemes {
                    let width = if s.stereo && s.codec == AudioCodec::Opus {
                        2
                    } else {
                        1
                    };
                    v.push(&s.frame[..s.len * width], width as u8);
                    s.viseme_fed_at = now;
                }
            }
            let take = (s.len - s.pos).min(frames_needed - written);
            let gain = s.volume * master;
            if gain != 0.0 {
                contributed = true;
                let stereo_frame = s.stereo && s.codec == AudioCodec::Opus;
                let pan = pan_allowed && channels >= 2 && s.panned;
                for f in 0..take {
                    let base = (written + f) * channels;
                    if stereo_frame && !pan {
                        let l = s.frame[(s.pos + f) * 2];
                        let r = s.frame[(s.pos + f) * 2 + 1];
                        for c in 0..channels {
                            let sample = match (channels, c) {
                                (1, _) => 0.5 * (l + r),
                                (_, 0) => l,
                                (_, 1) => r,
                                _ => 0.5 * (l + r),
                            };
                            output[base + c] += sample * gain;
                        }
                        continue;
                    }
                    let sample = if stereo_frame {
                        0.5 * (s.frame[(s.pos + f) * 2] + s.frame[(s.pos + f) * 2 + 1])
                    } else {
                        s.frame[s.pos + f]
                    };
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
        if written < frames_needed && now.duration_since(s.viseme_fed_at) >= FRAME_DURATION {
            // Nothing (more) to play: the mouth relaxes towards closed.
            if let Some(v) = &mut s.visemes {
                v.push(&[], 1);
                s.viseme_fed_at = now;
            }
        }
        (written, contributed)
    }

    /// Mix into `output` (interleaved, `channels` wide; **adds** to its contents). Streams
    /// with a direction are panned between the first two channels (a stereo sender is
    /// downmixed first); stereo streams without one keep their L/R image, or are downmixed
    /// for a mono output. Returns the number of streams that contributed audio.
    pub fn mix(&mut self, output: &mut [f32], channels: u8) -> usize {
        let channels = channels.clamp(1, 8) as usize;
        let master = self.master_gain();
        let now = Instant::now();
        self.expire_idle(now);
        let mut active = 0;
        let claimed = &self.claimed;
        let mut dred = self.dred.as_mut();
        for (ssrc, s) in self.streams.iter_mut() {
            if claimed.contains(ssrc) {
                continue;
            }
            if Self::render_stream(s, dred.as_deref_mut(), output, channels, master, true, now).1 {
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

    /// Per-participant playout: **overwrite** `output` (interleaved, `channels` wide) with
    /// the sum of the listed streams only — a participant's microphone and its synthesized
    /// TTS voice, typically. No local panning is applied (the engine positions the source
    /// itself); a stereo sender keeps its L/R image on a stereo output and is downmixed for a
    /// mono one. Per-participant volume, the server's gain byte and the master volume / mute
    /// still apply. Frames past what the jitter buffers hold are silence. Returns the number
    /// of frames that carried decoded audio (`0` while the participant is silent or unknown).
    /// Streams not pulled by anyone keep their newest frames and drop the oldest, so pulling
    /// some participants and mixing the rest with [`Self::mix`] does not double-play anyone.
    pub fn pull(&mut self, ssrcs: &[u32], output: &mut [f32], channels: u8) -> usize {
        output.fill(0.0);
        let channels = channels.clamp(1, 8) as usize;
        let master = self.master_gain();
        let now = Instant::now();
        self.expire_idle(now);
        let mut frames = 0;
        let mut dred = self.dred.as_mut();
        for ssrc in ssrcs {
            if let Some(s) = self.streams.get_mut(ssrc) {
                let (written, _) = Self::render_stream(
                    s,
                    dred.as_deref_mut(),
                    output,
                    channels,
                    master,
                    false,
                    now,
                );
                frames = frames.max(written);
            }
        }
        if frames > 0 {
            for v in output.iter_mut() {
                *v = v.clamp(-1.0, 1.0);
            }
        }
        frames
    }

    /// [`Self::pull`] into an i16 buffer.
    pub fn pull_i16(&mut self, ssrcs: &[u32], output: &mut [i16], channels: u8) -> usize {
        self.scratch.clear();
        self.scratch.resize(output.len(), 0.0);
        let mut scratch = std::mem::take(&mut self.scratch);
        let frames = self.pull(ssrcs, &mut scratch, channels);
        for (o, s) in output.iter_mut().zip(scratch.iter()) {
            *o = (s * 32767.0).round().clamp(-32768.0, 32767.0) as i16;
        }
        self.scratch = scratch;
        frames
    }

    /// SSRCs of the streams currently held.
    pub fn stream_ids(&self) -> Vec<u32> {
        self.streams.keys().copied().collect()
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
        let mut enc = CaptureEncoder::new(EncoderSettings::default()).unwrap();
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
            mixer
                .push(7, i as u32, 1.0, None, f.payload.clone())
                .unwrap();
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
    fn g711_encoder_decimates_and_decoder_restores_tone() {
        for law in [Law::Mu, Law::A] {
            let mut enc = G711Encoder::new();
            let mut dec = G711Decoder::new();
            let pcm = sine(FRAME_SAMPLES * 20, 48_000, 440.0, 0.5);
            let mut ulaw = Vec::new();
            let mut out = Vec::new();
            let mut frame = vec![0f32; FRAME_SAMPLES];
            for chunk in pcm.chunks(FRAME_SAMPLES) {
                ulaw.clear();
                enc.encode(law, chunk, &mut ulaw);
                assert_eq!(ulaw.len(), PCMU_FRAME_SAMPLES);
                assert_eq!(dec.decode(law, &ulaw, &mut frame), Some(FRAME_SAMPLES));
                out.extend_from_slice(&frame);
            }
            assert_eq!(out.len(), pcm.len());
            // Skip the FIR group delay of both filters, then the tone must come back at level.
            let e = rms(&out[FRAME_SAMPLES * 2..]);
            assert!((e - 0.3535).abs() < 0.03, "{law:?}: {e}");
            // Unsupported frame sizes are refused rather than misinterpreted.
            assert_eq!(dec.decode(law, &[0xff; 100], &mut frame), None);
            assert_eq!(
                dec.decode(law, &[0xff; 80], &mut frame),
                Some(80 * PCMU_DECIMATION)
            );
            // Concealment fades to silence and never exceeds one frame.
            assert_eq!(dec.conceal(&mut frame), FRAME_SAMPLES);
            assert!(frame[FRAME_SAMPLES - 1].abs() < 1e-3);
        }
    }

    #[test]
    fn capture_encoder_switches_codec_and_mixer_plays_mixed_streams() {
        let mut enc = CaptureEncoder::new(EncoderSettings::default()).unwrap();
        assert_eq!(enc.codec(), AudioCodec::Opus);
        let pcm = sine(FRAME_SAMPLES * 12, 48_000, 440.0, 0.5);
        let mut opus_frames = Vec::new();
        enc.push_f32(&pcm, 48_000, 1, |f| opus_frames.push(f));
        assert!(opus_frames.iter().all(|f| f.codec == AudioCodec::Opus));

        enc.set_codec(AudioCodec::Pcmu);
        assert_eq!(enc.codec(), AudioCodec::Pcmu);
        let mut pcmu_frames = Vec::new();
        enc.push_f32(&pcm, 48_000, 1, |f| pcmu_frames.push(f));
        assert_eq!(pcmu_frames.len(), 12);
        assert!(pcmu_frames
            .iter()
            .all(|f| f.codec == AudioCodec::Pcmu && f.payload.len() == PCMU_FRAME_SAMPLES));
        // Level/VAD metadata is computed before encoding, so it is codec independent.
        assert!(pcmu_frames.iter().all(|f| f.speech && f.level < 20));

        enc.set_codec(AudioCodec::Opus);
        let mut back = Vec::new();
        enc.push_f32(&pcm[..FRAME_SAMPLES], 48_000, 1, |f| back.push(f));
        assert_eq!(back.len(), 1);
        assert_eq!(back[0].codec, AudioCodec::Opus);

        // One Opus and one PCMU participant on the same mixer, each at -6 dB volume.
        let mut mixer = RemoteMixer::new(1, 12);
        for (i, (o, p)) in opus_frames.iter().zip(&pcmu_frames).enumerate() {
            mixer
                .push_frame(1, i as u32, 0.5, None, AudioCodec::Opus, o.payload.clone())
                .unwrap();
            mixer
                .push_frame(2, i as u32, 0.5, None, AudioCodec::Pcmu, p.payload.clone())
                .unwrap();
        }
        let mut out = vec![0f32; FRAME_SAMPLES * 12];
        mixer.mix(&mut out, 1);
        // Both streams are audible: one alone would sit at 0.177 (half volume), two tones
        // with different codec delays sum somewhere between incoherent and coherent.
        let e = rms(&out[FRAME_SAMPLES * 3..]);
        assert!(e > 0.21 && e < 0.38, "{e}");

        // A stream that flips codec mid-way keeps decoding (server re-negotiation).
        let mut mixer = RemoteMixer::new(1, 12);
        for (i, o) in opus_frames.iter().enumerate().take(6) {
            mixer
                .push_frame(3, i as u32, 1.0, None, AudioCodec::Opus, o.payload.clone())
                .unwrap();
        }
        for (i, p) in pcmu_frames.iter().enumerate().skip(6) {
            mixer
                .push_frame(3, i as u32, 1.0, None, AudioCodec::Pcmu, p.payload.clone())
                .unwrap();
        }
        let mut out = vec![0f32; FRAME_SAMPLES * 12];
        mixer.mix(&mut out, 1);
        let e = rms(&out[FRAME_SAMPLES * 8..]);
        assert!((e - 0.3535).abs() < 0.08, "{e}");
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
    fn jitter_buffer_resyncs_after_a_sequence_restart() {
        let mut jb = JitterBuffer::new(2, 8);
        jb.push(1000, vec![0]);
        jb.push(1001, vec![1]);
        assert_eq!(jb.pop(), JitterSlot::Frame(vec![0]));
        assert_eq!(jb.pop(), JitterSlot::Frame(vec![1]));
        // The sender restarted far below: not "late", the buffer re-primes on the new numbering.
        jb.push(7, vec![7]);
        jb.push(8, vec![8]);
        assert_eq!(jb.pop(), JitterSlot::Frame(vec![7]));
        assert_eq!(jb.pop(), JitterSlot::Frame(vec![8]));
        // …and far above: no endless run of loss markers.
        jb.push(900_000, vec![9]);
        jb.push(900_001, vec![10]);
        assert_eq!(jb.pop(), JitterSlot::Frame(vec![9]));
        assert_eq!(jb.late, 0);
        assert_eq!(jb.lost, 0);
    }

    #[test]
    fn encoder_settings_reach_libopus() {
        let wanted = EncoderSettings {
            bitrate_bps: 24_000,
            complexity: 3,
            max_bandwidth: OpusBandwidth::Wideband,
            signal: OpusSignal::Music,
            vbr: false,
            constrained_vbr: false,
            fec: false,
            expected_loss_percent: 20,
            dtx: true,
            channels: 1,
            dred_duration_ms: 200,
        };
        let mut enc = CaptureEncoder::new(wanted).unwrap();
        assert_eq!(enc.settings(), wanted);
        assert_eq!(enc.probe().unwrap(), wanted);

        // Out-of-range values are clamped rather than rejected.
        enc.apply(EncoderSettings {
            bitrate_bps: 1_000_000,
            complexity: 99,
            expected_loss_percent: 255,
            ..wanted
        })
        .unwrap();
        let s = enc.settings();
        assert_eq!(
            (s.bitrate_bps, s.complexity, s.expected_loss_percent),
            (EncoderSettings::MAX_BITRATE, 10, 100)
        );
        assert_eq!(enc.probe().unwrap(), s);
        enc.set_bitrate(1).unwrap();
        assert_eq!(enc.bitrate(), EncoderSettings::MIN_BITRATE);
        assert_eq!(enc.probe().unwrap().complexity, 10, "other settings kept");
    }

    #[test]
    fn bandwidth_cap_bounds_the_frame_size() {
        // A narrowband, low-complexity, CBR encoder produces smaller packets than a fullband
        // VBR one for the same wideband-rich input.
        fn bytes_for(settings: EncoderSettings) -> usize {
            let mut enc = CaptureEncoder::new(settings).unwrap();
            let mut pcm = sine(SAMPLE_RATE as usize, SAMPLE_RATE, 440.0, 0.3);
            let hi = sine(SAMPLE_RATE as usize, SAMPLE_RATE, 9_000.0, 0.3);
            for (a, b) in pcm.iter_mut().zip(hi) {
                *a += b;
            }
            let mut total = 0;
            enc.push_f32(&pcm, SAMPLE_RATE, 1, |f| total += f.payload.len());
            total
        }
        let full = bytes_for(EncoderSettings {
            bitrate_bps: 64_000,
            ..EncoderSettings::default()
        });
        let narrow = bytes_for(EncoderSettings {
            bitrate_bps: 64_000,
            max_bandwidth: OpusBandwidth::Narrowband,
            complexity: 0,
            ..EncoderSettings::default()
        });
        assert!(narrow < full, "narrow={narrow} full={full}");
        let cbr = bytes_for(EncoderSettings {
            bitrate_bps: 16_000,
            vbr: false,
            ..EncoderSettings::default()
        });
        // 50 frames × 20 ms at 16 kbit/s = 40 bytes each, CBR is exact.
        assert!((1_900..=2_100).contains(&cbr), "{cbr}");
    }

    #[test]
    fn channel_policy_overrides_wire_settings_but_not_local_complexity() {
        let policy = AudioPolicy {
            bitrate_bps: 96_000,
            min_bitrate_bps: 32_000,
            fec: false,
            dtx: true,
            max_bandwidth: OpusBandwidth::Superwideband,
            complexity: Some(4),
            signal: OpusSignal::Music,
            stereo: false,
            e2ee: false,
        };
        let base = EncoderSettings {
            vbr: false,
            ..EncoderSettings::default()
        };
        let s = base.with_policy(&policy, None);
        assert_eq!(
            (
                s.bitrate_bps,
                s.fec,
                s.dtx,
                s.max_bandwidth,
                s.signal,
                s.complexity,
                s.vbr
            ),
            (
                96_000,
                false,
                true,
                OpusBandwidth::Superwideband,
                OpusSignal::Music,
                4,
                false
            )
        );
        assert_eq!(base.with_policy(&policy, Some(8)).complexity, 8);
        let no_hint = AudioPolicy {
            complexity: None,
            ..policy
        };
        assert_eq!(base.with_policy(&no_hint, None).complexity, base.complexity);
    }

    #[test]
    fn mixer_pans_directional_streams() {
        let mut enc = CaptureEncoder::new(EncoderSettings::default()).unwrap();
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
                .push(1, i as u32, 0.5, Some(right), f.payload.clone())
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

    /// Left-only 44.1 kHz stereo capture, chunked like a device callback.
    fn left_only_capture(enc: &mut CaptureEncoder) -> Vec<EncodedFrame> {
        let pcm = sine(44_100, 44_100, 440.0, 0.5);
        let stereo: Vec<f32> = pcm.iter().flat_map(|&s| [s, 0.0]).collect();
        let mut frames = Vec::new();
        for chunk in stereo.chunks(2 * 441) {
            enc.push_f32(chunk, 44_100, 2, |f| frames.push(f));
        }
        frames
    }

    /// Paced like a real downlink: one frame in, one 20 ms mix out.
    fn play(
        mixer: &mut RemoteMixer,
        ssrc: u32,
        seq0: u32,
        frames: &[EncodedFrame],
        direction: Option<Direction>,
        channels: usize,
    ) -> Vec<f32> {
        let mut out = vec![0f32; FRAME_SAMPLES * channels * frames.len()];
        for (i, f) in frames.iter().enumerate() {
            mixer
                .push(ssrc, seq0 + i as u32, 1.0, direction, f.payload.clone())
                .unwrap();
            let slice = &mut out[i * FRAME_SAMPLES * channels..(i + 1) * FRAME_SAMPLES * channels];
            mixer.mix(slice, channels as u8);
        }
        out
    }

    fn split_lr(out: &[f32], skip_frames: usize) -> (Vec<f32>, Vec<f32>) {
        let l = out
            .iter()
            .step_by(2)
            .skip(FRAME_SAMPLES * skip_frames)
            .copied()
            .collect();
        let r = out
            .iter()
            .skip(1)
            .step_by(2)
            .skip(FRAME_SAMPLES * skip_frames)
            .copied()
            .collect();
        (l, r)
    }

    #[test]
    fn stereo_capture_keeps_the_image_and_the_mixer_follows_the_packets() {
        let stereo = EncoderSettings {
            channels: 2,
            signal: OpusSignal::Music,
            bitrate_bps: 96_000,
            ..EncoderSettings::default()
        };
        let mut enc = CaptureEncoder::new(stereo).unwrap();
        assert_eq!(enc.channels(), 2);
        assert_eq!(enc.probe().unwrap().channels, 2);
        let frames = left_only_capture(&mut enc);
        assert!((frames.len() as i64 - 50).abs() <= 1, "{}", frames.len());
        assert!(frames.iter().all(|f| opus_packet_is_stereo(&f.payload)));
        // VAD / level are metered on the downmix, so a left-only source still counts.
        assert!(frames.iter().all(|f| f.speech));

        // Plain receiver (no direction): the image survives.
        let mut mixer = RemoteMixer::new(1, 12);
        let out = play(&mut mixer, 1, 0, &frames, None, 2);
        let (l, r) = split_lr(&out, 4);
        assert!((rms(&l) - 0.3535).abs() < 0.05, "{}", rms(&l));
        assert!(rms(&r) < 0.02, "{}", rms(&r));

        // Mono output: downmixed, not dropped.
        let mut mono = RemoteMixer::new(1, 12);
        let out1 = play(&mut mono, 2, 0, &frames, None, 1);
        let e = rms(&out1[FRAME_SAMPLES * 4..]);
        assert!((e - 0.177).abs() < 0.03, "{e}");

        // Positional receiver: the sender's own image is replaced by the direction.
        let right = Direction {
            azimuth: std::f32::consts::FRAC_PI_2,
            elevation: 0.0,
        };
        let mut positional = RemoteMixer::new(1, 12);
        let out = play(&mut positional, 3, 0, &frames, Some(right), 2);
        let (l, r) = split_lr(&out, 4);
        assert!(rms(&l) < 0.02, "{}", rms(&l));
        // Downmix (−6 dB) then the constant-power hard-right gain (+3 dB).
        assert!((rms(&r) - 0.25).abs() < 0.03, "{}", rms(&r));

        // A mono-policy channel and PCMU both force a mono uplink from the same source.
        let policy = AudioPolicy {
            bitrate_bps: 64_000,
            min_bitrate_bps: 16_000,
            fec: true,
            dtx: false,
            max_bandwidth: OpusBandwidth::Fullband,
            complexity: None,
            signal: OpusSignal::Music,
            stereo: false,
            e2ee: false,
        };
        assert_eq!(stereo.with_policy(&policy, None).channels, 1);
        assert_eq!(
            stereo
                .with_policy(
                    &AudioPolicy {
                        stereo: true,
                        ..policy
                    },
                    None
                )
                .channels,
            2
        );
        enc.apply(stereo.with_policy(&policy, None)).unwrap();
        assert_eq!(enc.channels(), 1);
        let mono_frames = left_only_capture(&mut enc);
        assert!(mono_frames.len() >= 45, "{}", mono_frames.len());
        assert!(mono_frames
            .iter()
            .all(|f| !opus_packet_is_stereo(&f.payload)));
        enc.apply(stereo).unwrap();
        enc.set_codec(AudioCodec::Pcmu);
        assert_eq!(enc.channels(), 1);
        let ulaw = left_only_capture(&mut enc);
        assert!(ulaw
            .iter()
            .all(|f| f.codec == AudioCodec::Pcmu && f.payload.len() == PCMU_FRAME_SAMPLES));
    }

    #[test]
    fn voice_effects_run_after_gain_and_before_vad_and_encoding() {
        use crate::effects::CallbackEffect;
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;

        let seen_channels = Arc::new(AtomicUsize::new(0));
        let frames_seen = Arc::new(AtomicUsize::new(0));
        let capture = |enc: &mut CaptureEncoder| {
            let pcm = sine(48_000, 48_000, 440.0, 0.5);
            let mut frames = Vec::new();
            for chunk in pcm.chunks(480) {
                enc.push_f32(chunk, 48_000, 1, |f| frames.push(f));
            }
            frames
        };

        let mut enc = CaptureEncoder::new(EncoderSettings::default()).unwrap();
        let loud = capture(&mut enc);
        assert!(loud.iter().all(|f| f.speech));

        // A stage that silences the frame is seen by the VAD / level meter and the encoder.
        let mut chain = EffectChain::default();
        let (sc, fs) = (seen_channels.clone(), frames_seen.clone());
        chain.push(Box::new(CallbackEffect::new(
            move |frame: &mut [f32], ch| {
                sc.store(ch as usize, Ordering::Relaxed);
                fs.fetch_add(1, Ordering::Relaxed);
                frame.fill(0.0);
            },
        )));
        enc.effects = chain;
        let muted = capture(&mut enc);
        assert_eq!(seen_channels.load(Ordering::Relaxed), 1);
        assert_eq!(frames_seen.load(Ordering::Relaxed), 50);
        assert!(muted.iter().all(|f| f.level == AUDIO_LEVEL_SILENCE));
        // VAD hangover / energy smoothing from the loud run decay, then it is plain silence.
        assert!(muted[20..].iter().all(|f| !f.speech && f.energy < 1e-4));

        // Empty chain is a bypass again.
        enc.effects = EffectChain::default();
        assert!(capture(&mut enc).iter().all(|f| f.speech));

        // Stereo frames arrive interleaved with `channels == 2`, and the effect output is
        // what gets metered: silencing the right half only still leaves speech.
        let mut stereo = CaptureEncoder::new(EncoderSettings {
            channels: 2,
            signal: OpusSignal::Music,
            bitrate_bps: 96_000,
            ..EncoderSettings::default()
        })
        .unwrap();
        let mut chain = EffectChain::default();
        let sc = seen_channels.clone();
        chain.push(Box::new(CallbackEffect::new(
            move |frame: &mut [f32], ch| {
                sc.store(ch as usize, Ordering::Relaxed);
                for [l, r] in frame.as_chunks_mut::<2>().0 {
                    *r = *l;
                    *l = 0.0;
                }
            },
        )));
        stereo.effects = chain;
        let frames = left_only_capture(&mut stereo);
        assert_eq!(seen_channels.load(Ordering::Relaxed), 2);
        assert!(frames
            .iter()
            .all(|f| f.speech && opus_packet_is_stereo(&f.payload)));
        let mut mixer = RemoteMixer::new(1, 12);
        let out = play(&mut mixer, 1, 0, &frames, None, 2);
        let (l, r) = split_lr(&out, 4);
        assert!(rms(&l) < 0.02, "{}", rms(&l));
        assert!((rms(&r) - 0.3535).abs() < 0.05, "{}", rms(&r));

        // The PCMU edge gets the same processed frame: a built-in pitch shift up an octave is
        // audible in the μ-law payload, and `reset()` clears the effect state too.
        use crate::effects::PitchShift;
        let mut pcmu = CaptureEncoder::new(EncoderSettings::default()).unwrap();
        pcmu.set_codec(AudioCodec::Pcmu);
        pcmu.effects = EffectChain::new(vec![Box::new(PitchShift::new(12.0))]);
        let frames = capture(&mut pcmu);
        assert!(frames
            .iter()
            .all(|f| f.codec == AudioCodec::Pcmu && f.payload.len() == PCMU_FRAME_SAMPLES));
        let mut decoded = Vec::new();
        for f in &frames[10..] {
            aurix_common::g711::decode_f32(&f.payload, &mut decoded);
        }
        let crossings = decoded
            .windows(2)
            .filter(|w| (w[0] < 0.0) != (w[1] < 0.0))
            .count() as f32;
        let hz = crossings / 2.0 / (decoded.len() as f32 / PCMU_SAMPLE_RATE as f32);
        assert!((hz - 880.0).abs() < 60.0, "pitched μ-law at {hz} Hz");
        assert!(frames[10..].iter().all(|f| f.speech));
        pcmu.reset();
        assert_eq!(pcmu.effects.len(), 1);
    }

    #[test]
    fn mixer_upgrades_a_stream_to_stereo_on_its_first_stereo_packet() {
        let mut mono_enc = CaptureEncoder::new(EncoderSettings::default()).unwrap();
        let pcm = sine(FRAME_SAMPLES * 10, 48_000, 440.0, 0.5);
        let mut mono_frames = Vec::new();
        mono_enc.push_f32(&pcm, 48_000, 1, |f| mono_frames.push(f));
        let mut stereo_enc = CaptureEncoder::new(EncoderSettings {
            channels: 2,
            bitrate_bps: 96_000,
            ..EncoderSettings::default()
        })
        .unwrap();
        let stereo_frames = left_only_capture(&mut stereo_enc);

        let mut mixer = RemoteMixer::new(1, 12);
        let mut seq = 0u32;
        let out = play(&mut mixer, 9, seq, &mono_frames, None, 2);
        seq += mono_frames.len() as u32;
        let (l, r) = split_lr(&out, 4);
        assert!((rms(&l) - rms(&r)).abs() < 0.01);
        assert!(rms(&r) > 0.3, "{}", rms(&r));

        // Same SSRC starts sending stereo: decoder upgrades, image appears.
        let out = play(&mut mixer, 9, seq, &stereo_frames[..20], None, 2);
        seq += 20;
        let (l, r) = split_lr(&out, 4);
        assert!(rms(&l) > 0.3, "{}", rms(&l));
        assert!(rms(&r) < 0.02, "{}", rms(&r));

        // …and back to mono packets on the (now stereo) decoder: libopus upmixes, no restart.
        let out = play(&mut mixer, 9, seq, &mono_frames, None, 2);
        let (l, r) = split_lr(&out, 4);
        assert!((rms(&l) - rms(&r)).abs() < 0.01);
        assert!(rms(&r) > 0.3, "{}", rms(&r));
    }

    #[test]
    fn mixer_totals_count_loss_late_underruns_and_survive_stream_removal() {
        let mut enc = CaptureEncoder::new(EncoderSettings::default()).unwrap();
        let pcm = sine(FRAME_SAMPLES * 12, 48_000, 440.0, 0.5);
        let mut frames = Vec::new();
        enc.push_f32(&pcm, 48_000, 1, |f| frames.push(f));
        let mut mixer = RemoteMixer::new(1, 12);
        let mut out = vec![0f32; FRAME_SAMPLES];

        // Frames 0,1,3 (2 lost), then an ancient frame that is discarded as late.
        for seq in [0u32, 1, 3] {
            mixer
                .push(9, seq, 1.0, None, frames[seq as usize].payload.clone())
                .unwrap();
        }
        for _ in 0..4 {
            mixer.mix(&mut out, 1);
        }
        mixer
            .push(9, 0, 1.0, None, frames[0].payload.clone())
            .unwrap();
        let t = mixer.totals();
        assert_eq!((t.lost, t.late), (1, 1), "{t:?}");

        // Buffer runs dry, next frame arrives within the window → one underrun.
        for _ in 0..3 {
            mixer.mix(&mut out, 1);
        }
        mixer
            .push(9, 4, 1.0, None, frames[4].payload.clone())
            .unwrap();
        assert_eq!(mixer.totals().underruns, 1);
        // Starving again without a follow-up frame is a natural pause, not an underrun.
        for _ in 0..3 {
            mixer.mix(&mut out, 1);
        }
        assert_eq!(mixer.totals().underruns, 1);

        // Removing the stream keeps its counters in the totals.
        mixer.remove(9);
        assert!(mixer.stream_stats().is_empty());
        let t = mixer.totals();
        assert_eq!((t.lost, t.late, t.underruns), (1, 1, 1), "{t:?}");
        mixer.clear();
        assert_eq!(mixer.totals(), t);
    }

    #[test]
    fn pull_isolates_one_participant_and_leaves_the_rest_to_the_mix() {
        // Two mono senders: 440 Hz at 0.5 (A) and 880 Hz at 0.25 (B). A stereo sender C.
        let mut enc = CaptureEncoder::new(EncoderSettings::default()).unwrap();
        let mut a = Vec::new();
        enc.push_f32(
            &sine(FRAME_SAMPLES * 20, 48_000, 440.0, 0.5),
            48_000,
            1,
            |f| a.push(f),
        );
        let mut enc_b = CaptureEncoder::new(EncoderSettings::default()).unwrap();
        let mut b = Vec::new();
        enc_b.push_f32(
            &sine(FRAME_SAMPLES * 20, 48_000, 880.0, 0.25),
            48_000,
            1,
            |f| b.push(f),
        );
        let mut enc_c = CaptureEncoder::new(EncoderSettings {
            channels: 2,
            bitrate_bps: 96_000,
            ..EncoderSettings::default()
        })
        .unwrap();
        let c = left_only_capture(&mut enc_c);

        let right = Direction {
            azimuth: std::f32::consts::FRAC_PI_2,
            elevation: 0.0,
        };
        let mut mixer = RemoteMixer::new(1, 12);
        let n = 20;
        let mut pulled_a = vec![0f32; FRAME_SAMPLES * 2 * n];
        let mut pulled_c = vec![0f32; FRAME_SAMPLES * 2 * n];
        let mut mixed = vec![0f32; FRAME_SAMPLES * 2 * n];
        let mut frames_a = 0;
        for i in 0..n {
            // A arrives with a server direction (hard right) — pull ignores it.
            mixer
                .push(1, i as u32, 1.0, Some(right), a[i].payload.clone())
                .unwrap();
            mixer
                .push(2, i as u32, 1.0, None, b[i].payload.clone())
                .unwrap();
            mixer
                .push(3, i as u32, 1.0, None, c[i].payload.clone())
                .unwrap();
            let range = i * FRAME_SAMPLES * 2..(i + 1) * FRAME_SAMPLES * 2;
            // Unknown SSRC in the list is ignored, output is overwritten (not added).
            pulled_a[range.clone()].fill(0.7);
            frames_a += mixer.pull(&[1, 0x8000_0001], &mut pulled_a[range.clone()], 2);
            mixer.pull(&[3], &mut pulled_c[range.clone()], 2);
            mixer.mix(&mut mixed[range], 2);
        }
        assert!(
            frames_a >= FRAME_SAMPLES * (n - 1) && frames_a <= FRAME_SAMPLES * n,
            "{frames_a}"
        );
        let skip = FRAME_SAMPLES * 2 * 4;
        // A: centred (no panning) at its own level on both channels.
        let (l, r) = split_lr(&pulled_a, 4);
        assert!((rms(&l) - 0.3535).abs() < 0.03, "{}", rms(&l));
        assert!((rms(&r) - 0.3535).abs() < 0.03, "{}", rms(&r));
        // C: stereo image preserved.
        let (l, r) = split_lr(&pulled_c, 4);
        assert!((rms(&l) - 0.3535).abs() < 0.05, "{}", rms(&l));
        assert!(rms(&r) < 0.02, "{}", rms(&r));
        // The aggregate mix now holds only B: neither A's 440 Hz nor C leaked into it.
        let (l, r) = split_lr(&mixed, 4);
        assert!((rms(&l) - 0.177).abs() < 0.03, "{}", rms(&l));
        assert!((rms(&r) - 0.177).abs() < 0.03, "{}", rms(&r));
        let period = 48_000.0 / 880.0;
        let zero_crossings = mixed[skip..]
            .chunks(2)
            .map(|s| s[0])
            .collect::<Vec<_>>()
            .windows(2)
            .filter(|w| (w[0] < 0.0) != (w[1] < 0.0))
            .count() as f32;
        let expected = (FRAME_SAMPLES * (n - 4)) as f32 / period * 2.0;
        assert!(
            (zero_crossings - expected).abs() / expected < 0.1,
            "{zero_crossings} vs {expected}"
        );

        // Mono pull of the stereo sender downmixes; master mute silences pulls too.
        let mut mono = vec![0f32; FRAME_SAMPLES];
        mixer
            .push(3, n as u32, 1.0, None, c[n].payload.clone())
            .unwrap();
        mixer.pull(&[3], &mut mono, 1);
        assert!(rms(&mono) > 0.1, "{}", rms(&mono));
        mixer.set_output_muted(true);
        mixer
            .push(3, n as u32 + 1, 1.0, None, c[n + 1].payload.clone())
            .unwrap();
        assert_eq!(mixer.pull(&[3], &mut mono, 1), FRAME_SAMPLES);
        assert_eq!(rms(&mono), 0.0);

        let stats = mixer.stream_stats();
        assert_eq!(stats.len(), 3);
        assert!(stats.iter().find(|s| s.ssrc == 3).unwrap().stereo);
        assert!(!stats.iter().find(|s| s.ssrc == 1).unwrap().stereo);
    }

    #[test]
    fn claimed_streams_survive_a_mix_that_runs_before_their_pull() {
        let mut enc = CaptureEncoder::new(EncoderSettings::default()).unwrap();
        let mut a = Vec::new();
        enc.push_f32(
            &sine(FRAME_SAMPLES * 12, 48_000, 440.0, 0.5),
            48_000,
            1,
            |f| a.push(f),
        );
        let mut mixer = RemoteMixer::new(1, 12);
        let mut mixed = vec![0f32; FRAME_SAMPLES];
        let mut pulled = vec![0f32; FRAME_SAMPLES];

        // Unclaimed: whoever renders first (here the mix) consumes the frame.
        for i in 0..4 {
            mixer
                .push(1, i, 1.0, None, a[i as usize].payload.clone())
                .unwrap();
            mixed.fill(0.0);
            mixer.mix(&mut mixed, 1);
        }
        assert!(rms(&mixed) > 0.2, "{}", rms(&mixed));
        assert_eq!(mixer.pull(&[1], &mut pulled, 1), 0);

        // Claimed: the mix skips it, the pull that follows gets the audio.
        mixer.set_claimed([1, 1 | crate::SYNTH_SSRC_FLAG]);
        assert_eq!(mixer.claimed().count(), 2);
        let mut pulled_frames = 0;
        for i in 4..10 {
            mixer
                .push(1, i, 1.0, None, a[i as usize].payload.clone())
                .unwrap();
            mixed.fill(0.0);
            assert_eq!(mixer.mix(&mut mixed, 1), 0);
            assert_eq!(rms(&mixed), 0.0);
            pulled_frames += mixer.pull(&[1], &mut pulled, 1);
        }
        assert!(pulled_frames >= FRAME_SAMPLES * 5, "{pulled_frames}");
        assert!(rms(&pulled) > 0.2, "{}", rms(&pulled));

        // Releasing the claim hands the stream back to the mix.
        mixer.set_claimed([]);
        mixer.push(1, 10, 1.0, None, a[10].payload.clone()).unwrap();
        mixed.fill(0.0);
        assert_eq!(mixer.mix(&mut mixed, 1), 1);
        assert!(rms(&mixed) > 0.2, "{}", rms(&mixed));
    }

    /// A harmonic-rich "vowel" (fundamental plus two formant partials), loud enough to open
    /// the analysed mouth; a bare sine reads as a closed-lip hum.
    fn vowel(len: usize) -> Vec<f32> {
        let mut pcm = sine(len, 48_000, 150.0, 0.25);
        for (i, s) in pcm.iter_mut().enumerate() {
            let t = i as f32 / 48_000.0;
            *s += 0.2 * (2.0 * std::f32::consts::PI * 750.0 * t).sin()
                + 0.15 * (2.0 * std::f32::consts::PI * 1250.0 * t).sin();
        }
        pcm
    }

    #[test]
    fn mixer_tracks_visemes_per_stream_and_relaxes_in_gaps() {
        let mut enc = CaptureEncoder::new(EncoderSettings::default()).unwrap();
        let mut voiced = Vec::new();
        enc.push_f32(&vowel(FRAME_SAMPLES * 10), 48_000, 1, |f| voiced.push(f));

        let mut mixer = RemoteMixer::new(1, 12);
        assert!(!mixer.visemes_enabled());
        mixer
            .push(7, 0, 1.0, None, voiced[0].payload.clone())
            .unwrap();
        let mut out = vec![0f32; FRAME_SAMPLES];
        mixer.mix(&mut out, 1);
        // Off: no analysis is kept even for a stream that is playing.
        assert_eq!(mixer.visemes(7), None);

        mixer.set_visemes(true);
        assert!(mixer.visemes_enabled());
        // Turning it on retrofits existing streams and applies to new ones.
        assert_eq!(mixer.visemes(7).unwrap().sequence, 0);
        for i in 1..8 {
            mixer
                .push(7, i, 1.0, None, voiced[i as usize].payload.clone())
                .unwrap();
            out.fill(0.0);
            mixer.mix(&mut out, 1);
        }
        let talking = mixer.visemes(7).unwrap();
        assert!(talking.sequence >= 6, "{talking:?}");
        assert!(talking.mouth_open > 0.3, "{talking:?}");
        assert_ne!(talking.dominant, crate::visemes::Viseme::Silence);
        assert_eq!(mixer.visemes(8), None, "unknown stream");

        // The stream stops: rendering gaps feed silence so the mouth closes, one push per
        // frame period at most no matter how small the host buffers are.
        std::thread::sleep(FRAME_DURATION);
        for _ in 0..4 {
            mixer.mix(&mut out[..FRAME_SAMPLES / 4], 1);
        }
        let after_gap = mixer.visemes(7).unwrap().sequence;
        assert!(after_gap > talking.sequence);
        for _ in 0..4 {
            mixer.mix(&mut out[..FRAME_SAMPLES / 4], 1);
        }
        assert_eq!(mixer.visemes(7).unwrap().sequence, after_gap);
        for _ in 0..40 {
            std::thread::sleep(FRAME_DURATION);
            mixer.mix(&mut out, 1);
            if mixer.visemes(7).unwrap().mouth_open < 0.05 {
                break;
            }
        }
        let quiet = mixer.visemes(7).unwrap();
        assert!(quiet.mouth_open < 0.05, "{quiet:?}");
        assert_eq!(quiet.dominant, crate::visemes::Viseme::Silence);

        mixer.set_visemes(false);
        assert_eq!(mixer.visemes(7), None);
    }

    #[test]
    fn encoder_tracks_visemes_of_the_voice_as_sent() {
        let mut enc = CaptureEncoder::new(EncoderSettings::default()).unwrap();
        assert_eq!(enc.visemes(), None);
        enc.set_visemes(true);
        let mut frames = 0;
        enc.push_f32(&vowel(FRAME_SAMPLES * 6), 48_000, 1, |_| frames += 1);
        let mouth = enc.visemes().unwrap();
        assert_eq!(mouth.sequence, frames as u64);
        assert!(mouth.mouth_open > 0.3, "{mouth:?}");

        // Stereo capture is downmixed for the analysis like everything else.
        let mut stereo = CaptureEncoder::new(EncoderSettings {
            channels: 2,
            ..EncoderSettings::default()
        })
        .unwrap();
        stereo.set_visemes(true);
        let mono = vowel(FRAME_SAMPLES * 6);
        let interleaved: Vec<f32> = mono.iter().flat_map(|&s| [s, s]).collect();
        stereo.push_f32(&interleaved, 48_000, 2, |_| {});
        assert!(stereo.visemes().unwrap().mouth_open > 0.3);

        enc.reset();
        let cleared = enc.visemes().unwrap();
        assert_eq!(
            cleared.mouth_open, 0.0,
            "reset closes the mouth: {cleared:?}"
        );
        assert_eq!(
            cleared.sequence, mouth.sequence,
            "but the counter stays monotonic"
        );
        enc.set_visemes(false);
        assert_eq!(enc.visemes(), None);
    }

    /// Syllable-like bursts (glottal-pulse harmonics with moving formant emphasis) separated
    /// by short pauses; SILK keeps classifying it as speech, so LBRR and DRED are produced.
    fn speech_like(frames: usize) -> Vec<f32> {
        let mut seed = 0x1234_5678u32;
        (0..frames * FRAME_SAMPLES)
            .map(|i| {
                let t = i as f32 / SAMPLE_RATE as f32;
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
                (s * 0.4 * env + noise) * 0.27
            })
            .collect()
    }

    fn rms(pcm: &[f32]) -> f32 {
        (pcm.iter().map(|s| s * s).sum::<f32>() / pcm.len().max(1) as f32).sqrt()
    }

    /// Best normalized cross-correlation of `a` against `b` over lags `0..400`.
    fn similarity(a: &[f32], b: &[f32]) -> f32 {
        let n = a.len().min(b.len()) - 400;
        (0..400)
            .map(|lag| {
                let (mut xy, mut xx, mut yy) = (0.0f64, 0.0f64, 0.0f64);
                for i in 0..n {
                    let (x, y) = (a[i] as f64, b[i + lag] as f64);
                    xy += x * y;
                    xx += x * x;
                    yy += y * y;
                }
                (xy / (xx * yy).sqrt().max(1e-12)) as f32
            })
            .fold(f32::MIN, f32::max)
    }

    fn resilient_encoder(channels: u8) -> CaptureEncoder {
        CaptureEncoder::new(EncoderSettings {
            bitrate_bps: if channels == 2 { 64_000 } else { 40_000 },
            fec: true,
            expected_loss_percent: 30,
            dtx: false,
            channels,
            dred_duration_ms: 400,
            ..EncoderSettings::default()
        })
        .unwrap()
    }

    /// Reference decode of every packet, and the mixer's output for `frames` with `lost`
    /// dropped (all packets are buffered up front so the gap is declared lost, not starved).
    fn mix_with_loss(
        frames: &[EncodedFrame],
        lost: &[u32],
        channels: usize,
        settings: DecoderSettings,
    ) -> (RemoteMixer, Vec<f32>) {
        let mut mixer = RemoteMixer::new(1, frames.len() + 4);
        mixer.set_decoder_settings(settings).unwrap();
        for (seq, f) in frames.iter().enumerate() {
            if !lost.contains(&(seq as u32)) {
                mixer
                    .push(7, seq as u32, 1.0, None, f.payload.clone())
                    .unwrap();
            }
        }
        let mut out = Vec::with_capacity(frames.len() * FRAME_SAMPLES * channels);
        let mut frame = vec![0f32; FRAME_SAMPLES * channels];
        for _ in 0..frames.len() {
            frame.fill(0.0);
            mixer.mix(&mut frame, channels as u8);
            out.extend_from_slice(&frame);
        }
        (mixer, out)
    }

    #[test]
    fn mixer_rebuilds_one_lost_frame_from_fec_and_conceals_without_it() {
        let mut enc = resilient_encoder(1);
        let pcm = speech_like(60);
        let mut frames = Vec::new();
        enc.push_f32(&pcm, SAMPLE_RATE, 1, |f| frames.push(f));
        assert_eq!(frames.len(), 60);
        // A lost frame whose successor carries LBRR (SILK omits it from some packets).
        let lost = (20..50u32)
            .find(|&s| opus::packet::has_lbrr(&frames[s as usize + 1].payload).unwrap())
            .expect("steady-state speech packets carry LBRR");
        let (_, reference) = mix_with_loss(&frames, &[], 1, DecoderSettings::default());
        let (mixer, fec) = mix_with_loss(&frames, &[lost], 1, DecoderSettings::default());
        let t = mixer.totals();
        assert_eq!(
            (t.lost, t.fec_recovered, t.dred_recovered),
            (1, 1, 0),
            "{t:?}"
        );

        // Without in-band FEC the same gap is concealed (deep PLC still fills it).
        let mut plain = CaptureEncoder::new(EncoderSettings {
            fec: false,
            dtx: false,
            dred_duration_ms: 0,
            ..EncoderSettings::default()
        })
        .unwrap();
        let mut plain_frames = Vec::new();
        plain.push_f32(&pcm, SAMPLE_RATE, 1, |f| plain_frames.push(f));
        let (mixer, plc) = mix_with_loss(&plain_frames, &[lost], 1, DecoderSettings::default());
        let t = mixer.totals();
        assert_eq!(
            (t.lost, t.fec_recovered, t.dred_recovered),
            (1, 0, 0),
            "{t:?}"
        );

        // Jitter target 1 delays play-out by one frame: slot `lost + 1` of the output.
        let win = (lost as usize + 1) * FRAME_SAMPLES..(lost as usize + 3) * FRAME_SAMPLES;
        let (fec_sim, plc_sim) = (
            similarity(&fec[win.clone()], &reference[win.clone()]),
            similarity(&plc[win.clone()], &reference[win.clone()]),
        );
        assert!(fec_sim > 0.9, "FEC similarity {fec_sim}");
        assert!(rms(&plc[win.clone()]) > 0.01, "PLC went silent");
        assert!(fec_sim > plc_sim, "FEC {fec_sim} should beat PLC {plc_sim}");
    }

    #[test]
    fn mixer_rebuilds_a_burst_from_dred_and_falls_back_to_plc_without_it() {
        let mut enc = resilient_encoder(1);
        let pcm = speech_like(60);
        let mut frames = Vec::new();
        enc.push_f32(&pcm, SAMPLE_RATE, 1, |f| frames.push(f));
        // 40 kb/s with 30 % expected loss buys ~135 ms of DRED history per packet (the
        // encoder trades duration for bits): a 120 ms burst is fully covered, a 160 ms one
        // has its oldest frames concealed instead.
        let lost: Vec<u32> = (30..36).collect();
        let (_, reference) = mix_with_loss(&frames, &[], 1, DecoderSettings::default());
        let (mixer, dred) = mix_with_loss(&frames, &lost, 1, DecoderSettings::default());
        let t = mixer.totals();
        assert_eq!(t.lost, 6, "{t:?}");
        assert_eq!(t.fec_recovered + t.dred_recovered, 6, "{t:?}");
        assert!(t.dred_recovered >= 5, "{t:?}");
        let win = 31 * FRAME_SAMPLES..37 * FRAME_SAMPLES;
        let (r_ref, r_dred) = (rms(&reference[win.clone()]), rms(&dred[win.clone()]));
        assert!(
            r_dred > r_ref * 0.3 && r_dred < r_ref * 3.0,
            "DRED rms {r_dred} vs {r_ref}"
        );
        let longer: Vec<u32> = (30..38).collect();
        let (mixer, _) = mix_with_loss(&frames, &longer, 1, DecoderSettings::default());
        let t = mixer.totals();
        assert_eq!(t.lost, 8, "{t:?}");
        assert!(t.fec_recovered + t.dred_recovered >= 5, "{t:?}");
        assert!(t.fec_recovered + t.dred_recovered < 8, "{t:?}");

        // A mixer without a DRED decoder conceals the whole burst.
        let mut mixer = RemoteMixer::new(1, 64);
        mixer.dred = None;
        for (seq, f) in frames.iter().enumerate() {
            if !lost.contains(&(seq as u32)) {
                mixer
                    .push(7, seq as u32, 1.0, None, f.payload.clone())
                    .unwrap();
            }
        }
        let mut frame = vec![0f32; FRAME_SAMPLES];
        for _ in 0..frames.len() {
            mixer.mix(&mut frame, 1);
        }
        let t = mixer.totals();
        assert_eq!(t.lost, 6, "{t:?}");
        assert_eq!(t.dred_recovered, 0, "{t:?}");
        assert!(t.fec_recovered <= 1, "{t:?}");

        // Packets without DRED: the burst is concealed, nothing is mis-attributed.
        let mut plain = CaptureEncoder::new(EncoderSettings {
            fec: false,
            dtx: false,
            dred_duration_ms: 0,
            ..EncoderSettings::default()
        })
        .unwrap();
        let mut plain_frames = Vec::new();
        plain.push_f32(&pcm, SAMPLE_RATE, 1, |f| plain_frames.push(f));
        let (mixer, _) = mix_with_loss(&plain_frames, &lost, 1, DecoderSettings::default());
        let t = mixer.totals();
        assert_eq!(
            (t.lost, t.fec_recovered, t.dred_recovered),
            (6, 0, 0),
            "{t:?}"
        );
    }

    #[test]
    fn bare_decoder_rebuilds_a_gap_from_dred_and_reports_zero_beyond_it() {
        let mut enc = resilient_encoder(1);
        let pcm = speech_like(50);
        let mut frames = Vec::new();
        enc.push_f32(&pcm, SAMPLE_RATE, 1, |f| frames.push(f));
        let mut dec = OpusDecoder::new(SAMPLE_RATE, 1).unwrap();
        dec.apply(DecoderSettings {
            complexity: 6,
            osce_bwe: false,
        })
        .unwrap();
        assert_eq!(dec.settings().complexity, 6);
        let mut out = vec![0f32; FRAME_SAMPLES];
        for f in &frames[..30] {
            dec.decode_f32(&f.payload, &mut out, false).unwrap();
        }
        // Frames 30..34 lost; packet 34 rebuilds 33 (FEC) and 30..32 (DRED, 2..4 frames back).
        let next = &frames[34].payload;
        let mut rebuilt = Vec::new();
        for back in (2..=4).rev() {
            let n = dec.dred_decode_f32(next, back, &mut out).unwrap();
            assert_eq!(n, FRAME_SAMPLES, "{back} frames back");
            rebuilt.extend_from_slice(&out);
        }
        let reference = &pcm[30 * FRAME_SAMPLES..33 * FRAME_SAMPLES];
        let (r_ref, r_dred) = (rms(reference), rms(&rebuilt));
        assert!(
            r_dred > r_ref * 0.3 && r_dred < r_ref * 3.0,
            "DRED rms {r_dred} vs {r_ref}"
        );
        // Beyond what the packet carries (~1 s cap, and nothing at all for a DRED-less packet).
        assert_eq!(dec.dred_decode_f32(next, 60, &mut out).unwrap(), 0);
        let mut plain = CaptureEncoder::new(EncoderSettings {
            dred_duration_ms: 0,
            dtx: false,
            ..EncoderSettings::default()
        })
        .unwrap();
        let mut plain_frames = Vec::new();
        plain.push_f32(&pcm, SAMPLE_RATE, 1, |f| plain_frames.push(f));
        assert_eq!(
            dec.dred_decode_f32(&plain_frames[34].payload, 2, &mut out)
                .unwrap(),
            0
        );
        assert!(matches!(
            dec.dred_decode_f32(next, 0, &mut out),
            Err(CodecError::BadFrame)
        ));
    }

    #[test]
    fn mixer_recovers_a_stereo_burst_and_survives_reordering() {
        let mut enc = resilient_encoder(2);
        let mono = speech_like(60);
        let pcm: Vec<f32> = mono.iter().flat_map(|&s| [s, s * 0.5]).collect();
        let mut frames = Vec::new();
        enc.push_f32(&pcm, SAMPLE_RATE, 2, |f| frames.push(f));
        assert_eq!(frames.len(), 60);
        let lost: Vec<u32> = (30..35).collect();
        let (mixer, out) = mix_with_loss(&frames, &lost, 2, DecoderSettings::default());
        let t = mixer.totals();
        assert_eq!(t.lost, 5, "{t:?}");
        assert_eq!(t.fec_recovered + t.dred_recovered, 5, "{t:?}");
        assert!(mixer.stream_stats()[0].stereo);
        let win = 31 * FRAME_SAMPLES * 2..36 * FRAME_SAMPLES * 2;
        let (l, r): (Vec<f32>, Vec<f32>) = out[win].chunks(2).map(|c| (c[0], c[1])).unzip();
        let (rl, rr) = (rms(&l), rms(&r));
        assert!(rl > 0.01 && rr > 0.5 * rl && rr < 0.8 * rl, "L {rl} R {rr}");

        // Reordered arrival within the buffer is plain decoding — nothing is "recovered".
        let mut mixer = RemoteMixer::new(4, 64);
        let order = [0u32, 1, 2, 3, 5, 4, 6, 8, 7, 9, 10, 11];
        for &seq in &order {
            mixer
                .push(7, seq, 1.0, None, frames[seq as usize].payload.clone())
                .unwrap();
        }
        let mut frame = vec![0f32; FRAME_SAMPLES * 2];
        for _ in 0..12 {
            mixer.mix(&mut frame, 2);
        }
        let t = mixer.totals();
        assert_eq!(
            (t.lost, t.late, t.fec_recovered, t.dred_recovered),
            (0, 0, 0, 0),
            "{t:?}"
        );
    }

    #[test]
    fn starved_stream_keeps_its_sequence_when_the_next_packet_can_rebuild_the_gap() {
        let mut enc = resilient_encoder(1);
        let pcm = speech_like(60);
        let mut frames = Vec::new();
        enc.push_f32(&pcm, SAMPLE_RATE, 1, |f| frames.push(f));
        let mut mixer = RemoteMixer::new(2, 12);
        let mut out = vec![0f32; FRAME_SAMPLES];
        let feed = |m: &mut RemoteMixer, seq: u32| {
            m.push(7, seq, 1.0, None, frames[seq as usize].payload.clone())
                .unwrap()
        };
        // Real-time cadence: one packet in, one frame out; frames 30..=35 never arrive.
        for seq in 0..30 {
            feed(&mut mixer, seq);
            mixer.mix(&mut out, 1);
        }
        for _ in 30..36 {
            mixer.mix(&mut out, 1);
        }
        assert!(mixer.stream_stats()[0].starved);
        for seq in 36..60 {
            feed(&mut mixer, seq);
            mixer.mix(&mut out, 1);
        }
        let t = mixer.totals();
        assert_eq!((t.underruns, t.lost), (1, 6), "{t:?}");
        assert_eq!(t.fec_recovered + t.dred_recovered, 6, "{t:?}");
        assert!(t.dred_recovered >= 5, "{t:?}");

        // Without redundancy the same starvation restarts the stream: no phantom losses.
        let mut plain = CaptureEncoder::new(EncoderSettings {
            fec: false,
            dtx: false,
            dred_duration_ms: 0,
            ..EncoderSettings::default()
        })
        .unwrap();
        let mut plain_frames = Vec::new();
        plain.push_f32(&pcm, SAMPLE_RATE, 1, |f| plain_frames.push(f));
        let mut mixer = RemoteMixer::new(2, 12);
        for (seq, frame) in plain_frames.iter().enumerate().take(30) {
            mixer
                .push(7, seq as u32, 1.0, None, frame.payload.clone())
                .unwrap();
            mixer.mix(&mut out, 1);
        }
        for _ in 30..36 {
            mixer.mix(&mut out, 1);
        }
        for (seq, frame) in plain_frames.iter().enumerate().take(60).skip(36) {
            mixer
                .push(7, seq as u32, 1.0, None, frame.payload.clone())
                .unwrap();
            mixer.mix(&mut out, 1);
        }
        let t = mixer.totals();
        assert_eq!(
            (t.underruns, t.lost, t.fec_recovered, t.dred_recovered),
            (1, 0, 0, 0),
            "{t:?}"
        );
    }

    #[test]
    fn pcmu_streams_ignore_the_opus_recovery_path() {
        let mut enc = CaptureEncoder::new(EncoderSettings::default()).unwrap();
        enc.set_codec(AudioCodec::Pcmu);
        let pcm = sine(FRAME_SAMPLES * 12, SAMPLE_RATE, 440.0, 0.5);
        let mut frames = Vec::new();
        enc.push_f32(&pcm, SAMPLE_RATE, 1, |f| frames.push(f));
        assert!(frames.iter().all(|f| f.codec == AudioCodec::Pcmu));
        let mut mixer = RemoteMixer::new(1, 16);
        for (seq, f) in frames.iter().enumerate() {
            if seq != 5 {
                mixer
                    .push_frame(7, seq as u32, 1.0, None, f.codec, f.payload.clone())
                    .unwrap();
            }
        }
        let mut out = vec![0f32; FRAME_SAMPLES];
        for _ in 0..12 {
            mixer.mix(&mut out, 1);
        }
        let t = mixer.totals();
        assert_eq!(
            (t.lost, t.fec_recovered, t.dred_recovered),
            (1, 0, 0),
            "{t:?}"
        );
    }
}
