//! Audio building blocks shared by every native integration: level metering + VAD, capture
//! framing (any PCM format → 20 ms mono 48 kHz Opus frames, or 8 kHz G.711 μ-law when the
//! session negotiated the PCMU fallback) and the receive side (per-sender jitter buffer +
//! decoder, mixed into one interleaved output with per-participant gain and constant-power
//! panning). Mirrors `sdk/unity/Runtime/Audio` so all clients sound the same.

use aurix_common::g711::{self, PCMU_FRAME_SAMPLES, PCMU_FRAME_SIZES, PCMU_SAMPLE_RATE};
use aurix_common::protocol::{decode_audio_level, encode_audio_level, AUDIO_LEVEL_SILENCE};
use aurix_common::types::{AudioCodec, AudioPolicy, Direction, OpusBandwidth, OpusSignal};
use std::collections::{BTreeMap, HashMap};
use std::time::{Duration, Instant};

use crate::dsp::{Dsp, DspConfig};

/// Opus on the wire runs at 48 kHz; the mixer and the capture path work at this rate and
/// PCMU frames are resampled to/from it at the edge.
pub const SAMPLE_RATE: u32 = 48_000;
/// One packet carries 20 ms of audio.
pub const FRAME_SAMPLES: usize = 960;
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

/// 48 kHz mono → G.711 μ-law at 8 kHz, one 20 ms frame at a time.
#[derive(Debug, Clone)]
pub struct PcmuEncoder {
    fir: NarrowbandFir,
}

impl Default for PcmuEncoder {
    fn default() -> Self {
        Self::new()
    }
}

impl PcmuEncoder {
    pub fn new() -> Self {
        Self {
            fir: NarrowbandFir::new(1.0),
        }
    }

    /// `pcm` is 48 kHz mono; every 6th low-passed sample becomes one μ-law byte in `out`.
    pub fn encode(&mut self, pcm: &[f32], out: &mut Vec<u8>) {
        for (i, &s) in pcm.iter().enumerate() {
            let y = self.fir.process(s);
            if i % PCMU_DECIMATION == PCMU_DECIMATION - 1 {
                out.push(g711::ulaw_encode((y.clamp(-1.0, 1.0) * 32767.0) as i16));
            }
        }
    }

    pub fn reset(&mut self) {
        self.fir.reset();
    }
}

/// G.711 μ-law at 8 kHz → 48 kHz mono, with hold-last-sample concealment for lost frames.
#[derive(Debug, Clone)]
pub struct PcmuDecoder {
    fir: NarrowbandFir,
    last: f32,
}

impl Default for PcmuDecoder {
    fn default() -> Self {
        Self::new()
    }
}

impl PcmuDecoder {
    pub fn new() -> Self {
        Self {
            fir: NarrowbandFir::new(PCMU_DECIMATION as f32),
            last: 0.0,
        }
    }

    /// Decodes one μ-law frame (10/20/40/60 ms) into `out` at 48 kHz. Returns the number of
    /// samples written, or `None` for a frame of unsupported size.
    pub fn decode(&mut self, ulaw: &[u8], out: &mut [f32]) -> Option<usize> {
        if !PCMU_FRAME_SIZES.contains(&ulaw.len()) || out.len() < ulaw.len() * PCMU_DECIMATION {
            return None;
        }
        let mut n = 0;
        for &b in ulaw {
            let s = g711::ulaw_decode(b) as f32 / 32768.0;
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
    /// `6_000..=300_000` bit/s (libopus' ceiling for a mono stream).
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
}

impl EncoderSettings {
    pub const MIN_BITRATE: u32 = 6_000;
    /// libopus clamps a mono stream's bitrate to 300 kbit/s internally.
    pub const MAX_BITRATE: u32 = 300_000;
    /// libopus' own default.
    pub const DEFAULT_COMPLEXITY: u8 = 9;

    pub fn clamped(mut self) -> Self {
        self.bitrate_bps = self.bitrate_bps.clamp(Self::MIN_BITRATE, Self::MAX_BITRATE);
        self.complexity = self.complexity.min(10);
        self.expected_loss_percent = self.expected_loss_percent.min(100);
        self
    }

    /// The channel's policy laid over these settings. `complexity` is the app's own choice
    /// (`None`: follow the policy's hint, or keep the current value if it has none); VBR mode
    /// stays local.
    pub fn with_policy(mut self, policy: &AudioPolicy, local_complexity: Option<u8>) -> Self {
        self.bitrate_bps = policy.bitrate_bps;
        self.fec = policy.fec;
        self.dtx = policy.dtx;
        self.max_bandwidth = policy.max_bandwidth;
        self.signal = policy.signal;
        if let Some(c) = local_complexity.or(policy.complexity) {
            self.complexity = c;
        }
        self.clamped()
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
    Ok(s)
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
        let application = match settings.signal {
            OpusSignal::Music => opus::Application::Audio,
            _ => opus::Application::Voip,
        };
        let mut encoder = opus::Encoder::new(sample_rate_hz, ch, application)?;
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

    pub fn apply(&mut self, settings: EncoderSettings) -> Result<(), opus::Error> {
        self.settings = apply_encoder_settings(&mut self.encoder, settings)?;
        Ok(())
    }

    pub fn probe(&mut self) -> Result<EncoderSettings, opus::Error> {
        probe_encoder_settings(&mut self.encoder)
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
}

impl OpusDecoder {
    pub fn new(sample_rate_hz: u32, channels: u8) -> Result<Self, CodecError> {
        let ch = opus_channels(channels)?;
        Ok(Self {
            decoder: opus::Decoder::new(sample_rate_hz, ch)?,
            channels: usize::from(channels),
        })
    }

    pub fn channels(&self) -> usize {
        self.channels
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
/// 20 ms mono frames — 48 kHz Opus, or 8 kHz μ-law when the session codec is PCMU — with
/// input gain and VAD metering applied.
pub struct CaptureEncoder {
    encoder: opus::Encoder,
    pcmu: PcmuEncoder,
    codec: AudioCodec,
    resampler: Option<Resampler>,
    mono: Vec<f32>,
    pending: Vec<f32>,
    gain: f32,
    pub vad: VoiceActivityDetector,
    /// Capture DSP (high-pass / AEC / NS / AGC), bypassed until configured.
    pub dsp: Dsp,
    settings: EncoderSettings,
    out: [u8; 1275],
    ulaw: Vec<u8>,
}

impl CaptureEncoder {
    pub fn new(settings: EncoderSettings) -> Result<Self, opus::Error> {
        let encoder =
            opus::Encoder::new(SAMPLE_RATE, opus::Channels::Mono, opus::Application::Voip)?;
        let mut this = Self {
            encoder,
            pcmu: PcmuEncoder::new(),
            codec: AudioCodec::Opus,
            resampler: None,
            mono: Vec::with_capacity(FRAME_SAMPLES * 4),
            pending: Vec::with_capacity(FRAME_SAMPLES * 4),
            gain: 1.0,
            vad: VoiceActivityDetector::default(),
            dsp: Dsp::new(DspConfig::BYPASS),
            settings: settings.clamped(),
            out: [0u8; 1275],
            ulaw: Vec::with_capacity(PCMU_FRAME_SAMPLES),
        };
        this.apply(this.settings)?;
        Ok(this)
    }

    /// Which codec the produced frames use. Switching resets both codecs' state so the first
    /// frame after the switch does not carry the previous one's history.
    pub fn set_codec(&mut self, codec: AudioCodec) {
        if self.codec != codec {
            self.codec = codec;
            self.pcmu.reset();
            let _ = self.encoder.reset_state();
        }
    }

    pub fn codec(&self) -> AudioCodec {
        self.codec
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

    /// Pushes every setting into libopus; takes effect from the next frame. On error the
    /// encoder keeps whatever libopus accepted so far and `settings()` is unchanged.
    pub fn apply(&mut self, settings: EncoderSettings) -> Result<(), opus::Error> {
        self.settings = apply_encoder_settings(&mut self.encoder, settings)?;
        Ok(())
    }

    /// What libopus reports back, for tests and diagnostics.
    pub fn probe(&mut self) -> Result<EncoderSettings, opus::Error> {
        probe_encoder_settings(&mut self.encoder)
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
            self.dsp.process_frame(frame);
            if self.gain != 1.0 {
                for s in frame.iter_mut() {
                    *s = (*s * self.gain).clamp(-1.0, 1.0);
                }
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
                AudioCodec::Pcmu => {
                    self.ulaw.clear();
                    self.pcmu.encode(frame, &mut self.ulaw);
                    sink(EncodedFrame {
                        payload: self.ulaw.clone(),
                        codec: AudioCodec::Pcmu,
                        level: self.vad.level(),
                        energy,
                        speech: self.vad.speaking(),
                    });
                }
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
        self.pcmu.reset();
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
    pcmu: PcmuDecoder,
    /// Codec of the last frame decoded; the concealment path follows it.
    codec: AudioCodec,
    /// Server-mixed stereo stream: `frame` holds interleaved L/R and is not panned.
    stereo: bool,
    volume: f32,
    left: f32,
    right: f32,
    frame: Vec<f32>,
    pos: usize,
    len: usize,
    last_activity: Instant,
    starved: bool,
    starved_at: Instant,
}

/// Statistics of one decoded sender stream.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct StreamStats {
    pub ssrc: u32,
    pub buffered_frames: usize,
    pub lost: u64,
    pub late: u64,
}

/// Downlink counters accumulated over every stream the mixer has ever seen.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct MixerTotals {
    /// Frames the jitter buffers declared lost and concealed with PLC.
    pub lost: u64,
    /// Frames that arrived after their play-out time and were discarded.
    pub late: u64,
    /// Times a stream ran dry mid-talk-spurt (a packet followed within
    /// [`UNDERRUN_RESUME_WINDOW`]); the natural end of a spurt is not counted.
    pub underruns: u64,
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
    output_volume: f32,
    output_muted: bool,
    target_depth: usize,
    max_depth: usize,
    scratch: Vec<f32>,
    /// Lost/late of streams already dropped, so totals never go backwards.
    retired: MixerTotals,
    underruns: u64,
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
            retired: MixerTotals::default(),
            underruns: 0,
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

    /// [`Self::push_frame`] with `stereo` set for a server-mixed stream (`PacketFlags::Mixed`
    /// with Opus): the payload is decoded as interleaved stereo and played without panning.
    /// A stream that flips between mono and stereo restarts its decoder.
    #[allow(clippy::too_many_arguments)]
    pub fn push_wire_frame(
        &mut self,
        ssrc: u32,
        seq: u32,
        volume: f32,
        direction: Option<Direction>,
        codec: AudioCodec,
        stereo: bool,
        data: Vec<u8>,
    ) -> Result<(), opus::Error> {
        let channels = if stereo {
            opus::Channels::Stereo
        } else {
            opus::Channels::Mono
        };
        if let Some(s) = self.streams.get_mut(&ssrc) {
            if s.stereo != stereo {
                s.decoder = opus::Decoder::new(SAMPLE_RATE, channels)?;
                s.stereo = stereo;
                s.pos = 0;
                s.len = 0;
            }
        }
        let stream = match self.streams.get_mut(&ssrc) {
            Some(s) => s,
            None => {
                let decoder = opus::Decoder::new(SAMPLE_RATE, channels)?;
                self.streams.entry(ssrc).or_insert(Stream {
                    jitter: JitterBuffer::new(self.target_depth, self.max_depth),
                    decoder,
                    pcmu: PcmuDecoder::new(),
                    codec,
                    stereo,
                    volume: 1.0,
                    left: 1.0,
                    right: 1.0,
                    frame: vec![0.0; MAX_DECODE_SAMPLES * 2],
                    pos: 0,
                    len: 0,
                    last_activity: Instant::now(),
                    starved: false,
                    starved_at: Instant::now(),
                })
            }
        };
        stream.volume = volume;
        match direction {
            Some(d) if !stereo => (stream.left, stream.right) = d.stereo_gains(),
            _ => (stream.left, stream.right) = (1.0, 1.0),
        }
        let now = Instant::now();
        stream.last_activity = now;
        if stream.starved {
            // A talk spurt after silence: refill the target depth before playing again.
            stream.jitter.reset();
            stream.starved = false;
            if now.duration_since(stream.starved_at) < UNDERRUN_RESUME_WINDOW {
                self.underruns += 1;
            }
        }
        stream.jitter.push(seq, WireFrame { codec, data });
        Ok(())
    }

    pub fn remove(&mut self, ssrc: u32) {
        if let Some(s) = self.streams.remove(&ssrc) {
            self.retire(&s);
        }
    }

    pub fn clear(&mut self) {
        for (_, s) in self.streams.drain() {
            self.retired.lost += s.jitter.lost;
            self.retired.late += s.jitter.late;
        }
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
        self.retired.lost += s.jitter.lost;
        self.retired.late += s.jitter.late;
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

    /// Lifetime downlink totals: live streams plus everything already retired.
    pub fn totals(&self) -> MixerTotals {
        let mut t = self.retired;
        for s in self.streams.values() {
            t.lost += s.jitter.lost;
            t.late += s.jitter.late;
        }
        t.underruns = self.underruns;
        t
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
        let mut retired = self.retired;
        self.streams.retain(|_, s| {
            let keep = now.duration_since(s.last_activity) < STREAM_IDLE_TIMEOUT;
            if !keep {
                retired.lost += s.jitter.lost;
                retired.late += s.jitter.late;
            }
            keep
        });
        self.retired = retired;
        for s in self.streams.values_mut() {
            let mut written = 0;
            let mut contributed = false;
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
                                AudioCodec::Opus => {
                                    s.decoder.decode_float(&data, &mut s.frame, false)
                                }
                                AudioCodec::Pcmu => {
                                    Ok(s.pcmu.decode(&data, &mut s.frame).unwrap_or(0))
                                }
                            }
                        }
                        JitterSlot::Lost => match s.codec {
                            AudioCodec::Opus => {
                                let width = if s.stereo { 2 } else { 1 };
                                s.decoder.decode_float(
                                    &[],
                                    &mut s.frame[..FRAME_SAMPLES * width],
                                    false,
                                )
                            }
                            AudioCodec::Pcmu => Ok(s.pcmu.conceal(&mut s.frame)),
                        },
                    };
                    // PCMU is always mono, even on a stream that also carried stereo Opus.
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
                    let stereo_frame = s.stereo && s.codec == AudioCodec::Opus;
                    let pan = channels >= 2 && !stereo_frame;
                    for f in 0..take {
                        let base = (written + f) * channels;
                        if stereo_frame {
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
                        let sample = s.frame[s.pos + f];
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
    fn pcmu_encoder_decimates_and_decoder_restores_tone() {
        let mut enc = PcmuEncoder::new();
        let mut dec = PcmuDecoder::new();
        let pcm = sine(FRAME_SAMPLES * 20, 48_000, 440.0, 0.5);
        let mut ulaw = Vec::new();
        let mut out = Vec::new();
        let mut frame = vec![0f32; FRAME_SAMPLES];
        for chunk in pcm.chunks(FRAME_SAMPLES) {
            ulaw.clear();
            enc.encode(chunk, &mut ulaw);
            assert_eq!(ulaw.len(), PCMU_FRAME_SAMPLES);
            assert_eq!(dec.decode(&ulaw, &mut frame), Some(FRAME_SAMPLES));
            out.extend_from_slice(&frame);
        }
        assert_eq!(out.len(), pcm.len());
        // Skip the FIR group delay of both filters, then the tone must come back at level.
        let e = rms(&out[FRAME_SAMPLES * 2..]);
        assert!((e - 0.3535).abs() < 0.03, "{e}");
        // Unsupported frame sizes are refused rather than misinterpreted.
        assert_eq!(dec.decode(&[0xff; 100], &mut frame), None);
        assert_eq!(
            dec.decode(&[0xff; 80], &mut frame),
            Some(80 * PCMU_DECIMATION)
        );
        // Concealment fades to silence and never exceeds one frame.
        assert_eq!(dec.conceal(&mut frame), FRAME_SAMPLES);
        assert!(frame[FRAME_SAMPLES - 1].abs() < 1e-3);
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
}
