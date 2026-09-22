//! Server-side noise suppression of uplinks (`media.noise_suppression`).
//!
//! The node runs an RNNoise-class recurrent denoiser (`nnnoiseless`, the same model the native
//! SDK uses on the client) over a session's audio before it enters the router — for clients
//! that run no capture DSP of their own or channels that require it
//! (`ChannelConfig::noise_suppression`). An Opus uplink is decoded to 48 kHz mono, cleaned and
//! re-encoded with the node's own encoder (same frame duration, node bitrate and in-band FEC);
//! a PCMU uplink is cleaned inside its transcode, where the μ-law samples are already PCM
//! (8 kHz, resampled to the model's 48 kHz and back). End-to-end encrypted frames are never
//! touched — the node cannot decode them — and neither are frames into stereo channels: the
//! model is mono speech.
//!
//! Cost: one Opus decode + one RNNoise pass (~1 % of a core per session) + one Opus encode per
//! frame of every cleaned Opus session; [`DenoisePool`] bounds the number of sessions cleaned
//! at once (`max_sessions`).

use aurix_common::config::{NoiseSuppressionConfig, NoiseSuppressionLevel};
use aurix_common::error::{AurixError, Result};
use aurix_common::g711::{PCMU_FRAME_SIZES, PCMU_SAMPLE_RATE};
use bytes::Bytes;
use nnnoiseless::DenoiseState;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

/// Sample rate the model runs at.
pub const SAMPLE_RATE: u32 = 48_000;
/// Samples per model block (10 ms at 48 kHz); every cleaned frame is a multiple of it.
pub const BLOCK: usize = DenoiseState::FRAME_SIZE;
/// Longest Opus frame the cleaner re-encodes (120 ms at 48 kHz).
const MAX_FRAME_SAMPLES: usize = 5_760;
/// Upsampling ratio between the PCMU rate and the model rate.
const NB_RATIO: usize = (SAMPLE_RATE / PCMU_SAMPLE_RATE) as usize;
/// libopus encoder complexity of the re-encode: well within transparent for speech at the
/// default bitrate, half the CPU of the default `9`.
const ENCODER_COMPLEXITY: i32 = 5;
/// Shortest Opus packet worth cleaning: a TOC-only (DTX / lost-frame) packet decodes to
/// silence or concealment and is forwarded as it came.
const MIN_OPUS_PACKET: usize = 3;

/// Node-wide budget of concurrently denoised sessions (`media.noise_suppression`).
pub struct DenoisePool {
    config: NoiseSuppressionConfig,
    active: AtomicUsize,
}

impl DenoisePool {
    pub fn new(config: NoiseSuppressionConfig) -> Arc<Self> {
        Arc::new(Self {
            config,
            active: AtomicUsize::new(0),
        })
    }

    /// Whether the node offers the denoiser at all (`SessionInitAck.noise_suppression`).
    pub fn enabled(&self) -> bool {
        self.config.enabled
    }

    pub fn config(&self) -> &NoiseSuppressionConfig {
        &self.config
    }

    /// Sessions holding a slot right now.
    pub fn active(&self) -> usize {
        self.active.load(Ordering::Relaxed)
    }

    /// Reserves a session slot; `None` when the denoiser is disabled or `max_sessions` are
    /// already cleaned. The slot is returned when the guard drops.
    pub fn acquire(self: &Arc<Self>) -> Option<DenoiseSlot> {
        if !self.config.enabled {
            return None;
        }
        let mut current = self.active.load(Ordering::Relaxed);
        loop {
            if current >= self.config.max_sessions {
                return None;
            }
            match self.active.compare_exchange_weak(
                current,
                current + 1,
                Ordering::AcqRel,
                Ordering::Relaxed,
            ) {
                Ok(_) => break,
                Err(actual) => current = actual,
            }
        }
        aurix_metrics::NOISE_SUPPRESSION_SESSIONS.inc();
        Some(DenoiseSlot {
            pool: Arc::clone(self),
        })
    }
}

/// One session's reservation in a [`DenoisePool`].
pub struct DenoiseSlot {
    pool: Arc<DenoisePool>,
}

impl Drop for DenoiseSlot {
    fn drop(&mut self) {
        self.pool.active.fetch_sub(1, Ordering::AcqRel);
        aurix_metrics::NOISE_SUPPRESSION_SESSIONS.dec();
    }
}

impl std::fmt::Debug for DenoiseSlot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("DenoiseSlot")
    }
}

/// One session's cleaning state: its pool slot plus the codec state built on first use for
/// whichever uplink path (Opus / PCMU) the session sends on.
pub struct UplinkDenoiser {
    _slot: DenoiseSlot,
    config: NoiseSuppressionConfig,
    opus: Option<OpusUplinkCleaner>,
    narrowband: Option<NarrowbandDenoiser>,
    /// The last Opus frame cleaned, `(timestamp, input, output)`: a native client in several
    /// channels sends the same frame once per channel and each copy must not go through the
    /// model (and the codec state) again.
    last: Option<(u32, Bytes, Option<Bytes>)>,
}

impl std::fmt::Debug for UplinkDenoiser {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UplinkDenoiser")
            .field("opus", &self.opus.is_some())
            .field("narrowband", &self.narrowband.is_some())
            .finish()
    }
}

impl UplinkDenoiser {
    /// Reserves a slot in `pool`; `None` when the node cannot clean another session.
    pub fn acquire(pool: &Arc<DenoisePool>) -> Option<Self> {
        let slot = pool.acquire()?;
        Some(Self {
            _slot: slot,
            config: pool.config.clone(),
            opus: None,
            narrowband: None,
            last: None,
        })
    }

    /// Cleans one Opus uplink packet (see [`OpusUplinkCleaner::clean`]); a repeat of the
    /// previous packet (same `timestamp` and bytes) gets the previous outcome.
    pub fn clean_opus(&mut self, timestamp: u32, packet: &[u8]) -> Result<Cleaned> {
        if let Some((ts, input, output)) = &self.last {
            if *ts == timestamp && input.as_ref() == packet {
                return Ok(Cleaned::Repeated(output.clone()));
            }
        }
        let cleaner = match self.opus.as_mut() {
            Some(c) => c,
            None => self.opus.insert(OpusUplinkCleaner::new(&self.config)?),
        };
        let cleaned = cleaner.clean(packet)?;
        let output = match &cleaned {
            Cleaned::Opus(bytes) => Some(bytes.clone()),
            Cleaned::Passthrough | Cleaned::Repeated(_) => None,
        };
        self.last = Some((timestamp, Bytes::copy_from_slice(packet), output));
        Ok(cleaned)
    }

    /// The 8 kHz denoiser for the session's PCMU transcode.
    pub fn narrowband(&mut self) -> &mut NarrowbandDenoiser {
        let level = self.config.level;
        self.narrowband
            .get_or_insert_with(|| NarrowbandDenoiser::new(level))
    }
}

/// The recurrent denoiser over 48 kHz mono PCM, in blocks of [`BLOCK`] samples.
pub struct Denoiser {
    state: Box<DenoiseState<'static>>,
    wet: f32,
    input: Vec<f32>,
    output: Vec<f32>,
}

impl Denoiser {
    pub fn new(level: NoiseSuppressionLevel) -> Self {
        Self {
            state: DenoiseState::new(),
            wet: level.wet(),
            input: vec![0.0; BLOCK],
            output: vec![0.0; BLOCK],
        }
    }

    /// Cleans `pcm` in place and returns the model's mean speech probability over its blocks.
    /// `pcm.len()` must be a positive multiple of [`BLOCK`].
    pub fn process(&mut self, pcm: &mut [i16]) -> Result<f32> {
        if pcm.is_empty() || !pcm.len().is_multiple_of(BLOCK) {
            return Err(AurixError::Codec(format!(
                "denoiser frame of {} samples (expected a multiple of {BLOCK})",
                pcm.len()
            )));
        }
        let dry = 1.0 - self.wet;
        let mut prob_sum = 0.0;
        for block in pcm.as_chunks_mut::<BLOCK>().0 {
            for (i, s) in self.input.iter_mut().zip(block.iter()) {
                *i = f32::from(*s);
            }
            prob_sum += self.state.process_frame(&mut self.output, &self.input);
            for (s, o) in block.iter_mut().zip(self.output.iter()) {
                let mixed = dry * f32::from(*s) + self.wet * o;
                *s = mixed.round().clamp(-32768.0, 32767.0) as i16;
            }
        }
        Ok((prob_sum / (pcm.len() / BLOCK) as f32).clamp(0.0, 1.0))
    }
}

/// Outcome of cleaning one uplink frame.
#[derive(Debug, PartialEq, Eq)]
pub enum Cleaned {
    /// The frame was denoised and re-encoded.
    Opus(Bytes),
    /// The frame is forwarded as it came: a TOC-only packet, or a frame shorter than the
    /// model block (2.5 / 5 ms).
    Passthrough,
    /// The same frame again (another channel's copy): the earlier outcome, `None` for a
    /// passthrough.
    Repeated(Option<Bytes>),
}

/// Decode → denoise → encode for one session's Opus uplink.
pub struct OpusUplinkCleaner {
    decoder: opus::Decoder,
    encoder: opus::Encoder,
    denoiser: Denoiser,
    pcm: Vec<i16>,
    out: Vec<u8>,
}

impl OpusUplinkCleaner {
    pub fn new(config: &NoiseSuppressionConfig) -> Result<Self> {
        let decoder = opus::Decoder::new(SAMPLE_RATE, opus::Channels::Mono)
            .map_err(|e| AurixError::Codec(format!("denoise decoder: {e}")))?;
        let mut encoder =
            opus::Encoder::new(SAMPLE_RATE, opus::Channels::Mono, opus::Application::Voip)
                .map_err(|e| AurixError::Codec(format!("denoise encoder: {e}")))?;
        encoder
            .set_bitrate(opus::Bitrate::Bits(config.bitrate as i32))
            .map_err(|e| AurixError::Codec(format!("denoise encoder bitrate: {e}")))?;
        let _ = encoder.set_complexity(ENCODER_COMPLEXITY);
        let _ = encoder.set_inband_fec(true);
        let _ = encoder.set_packet_loss_perc(10);
        Ok(Self {
            decoder,
            encoder,
            denoiser: Denoiser::new(config.level),
            pcm: vec![0i16; MAX_FRAME_SAMPLES],
            out: vec![0u8; 1275],
        })
    }

    /// Cleans one Opus packet. Stereo packets are downmixed to mono by the decoder (callers
    /// keep stereo channels away from the cleaner); a packet the decoder rejects is an error
    /// and the caller forwards the original.
    pub fn clean(&mut self, packet: &[u8]) -> Result<Cleaned> {
        if packet.len() < MIN_OPUS_PACKET {
            return Ok(Cleaned::Passthrough);
        }
        let samples = self
            .decoder
            .get_nb_samples(packet)
            .map_err(|e| AurixError::Codec(format!("denoise packet: {e}")))?;
        if samples == 0 || samples > MAX_FRAME_SAMPLES {
            return Err(AurixError::Codec(format!(
                "denoise packet of {samples} samples (expected 1..={MAX_FRAME_SAMPLES})"
            )));
        }
        if !samples.is_multiple_of(BLOCK) {
            return Ok(Cleaned::Passthrough);
        }
        let pcm = &mut self.pcm[..samples];
        let decoded = self
            .decoder
            .decode(packet, pcm, false)
            .map_err(|e| AurixError::Codec(format!("denoise decode: {e}")))?;
        if decoded != samples {
            return Err(AurixError::Codec(format!(
                "denoise decode returned {decoded} samples for a {samples}-sample packet"
            )));
        }
        self.denoiser.process(pcm)?;
        let n = self
            .encoder
            .encode(pcm, &mut self.out)
            .map_err(|e| AurixError::Codec(format!("denoise encode: {e}")))?;
        Ok(Cleaned::Opus(Bytes::copy_from_slice(&self.out[..n])))
    }
}

/// The denoiser for 8 kHz PCM (a PCMU uplink): linear upsampling to the model rate, one model
/// pass, box-filter decimation back. The one-sample interpolation delay (125 µs) is constant.
pub struct NarrowbandDenoiser {
    denoiser: Denoiser,
    wide: Vec<i16>,
    last: i16,
}

impl NarrowbandDenoiser {
    pub fn new(level: NoiseSuppressionLevel) -> Self {
        Self {
            denoiser: Denoiser::new(level),
            wide: vec![0i16; PCMU_FRAME_SIZES[PCMU_FRAME_SIZES.len() - 1] * NB_RATIO],
            last: 0,
        }
    }

    /// Cleans one 8 kHz frame in place; its length must be one of [`PCMU_FRAME_SIZES`].
    pub fn process(&mut self, pcm: &mut [i16]) -> Result<f32> {
        if !PCMU_FRAME_SIZES.contains(&pcm.len()) {
            return Err(AurixError::Codec(format!(
                "narrowband denoiser frame of {} samples (expected one of {:?})",
                pcm.len(),
                PCMU_FRAME_SIZES
            )));
        }
        let wide = &mut self.wide[..pcm.len() * NB_RATIO];
        let mut prev = f32::from(self.last);
        for (s, out) in pcm.iter().zip(wide.as_chunks_mut::<NB_RATIO>().0) {
            let next = f32::from(*s);
            for (k, o) in out.iter_mut().enumerate() {
                let t = (k + 1) as f32 / NB_RATIO as f32;
                *o = (prev + (next - prev) * t).round() as i16;
            }
            prev = next;
        }
        self.last = pcm[pcm.len() - 1];
        let prob = self.denoiser.process(wide)?;
        for (s, group) in pcm.iter_mut().zip(wide.as_chunks::<NB_RATIO>().0) {
            let sum: i32 = group.iter().map(|v| i32::from(*v)).sum();
            *s = (sum / NB_RATIO as i32) as i16;
        }
        Ok(prob)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> NoiseSuppressionConfig {
        NoiseSuppressionConfig {
            enabled: true,
            level: NoiseSuppressionLevel::High,
            max_sessions: 2,
            bitrate: 32_000,
        }
    }

    /// Deterministic fan-like rumble: white noise from a 64-bit LCG through a one-pole low-pass
    /// (the stationary, coloured noise the model was trained against; RNNoise does little to
    /// synthetic full-band white noise).
    struct Rumble {
        seed: u64,
        y: f32,
        amplitude: f32,
    }

    impl Rumble {
        fn new(seed: u64, amplitude: f32) -> Self {
            Self {
                seed,
                y: 0.0,
                amplitude,
            }
        }

        fn block(&mut self, len: usize) -> Vec<i16> {
            (0..len)
                .map(|_| {
                    self.seed = self
                        .seed
                        .wrapping_mul(6364136223846793005)
                        .wrapping_add(1442695040888963407);
                    let white = ((self.seed >> 40) as f32 / (1u64 << 24) as f32) * 2.0 - 1.0;
                    self.y = 0.95 * self.y + 0.05 * white;
                    (self.y * 6.0 * self.amplitude) as i16
                })
                .collect()
        }
    }

    fn rms(pcm: &[i16]) -> f64 {
        (pcm.iter().map(|s| f64::from(*s).powi(2)).sum::<f64>() / pcm.len().max(1) as f64).sqrt()
    }

    #[test]
    fn stationary_noise_is_attenuated() {
        let mut d = Denoiser::new(NoiseSuppressionLevel::High);
        let mut rumble = Rumble::new(0x9e37_79b9, 600.0);
        let mut before = 0.0;
        let mut after = 0.0;
        // The recurrent state needs a moment to settle; judge the last second of four.
        for i in 0..400 {
            let mut frame = rumble.block(BLOCK);
            let b = rms(&frame);
            d.process(&mut frame).unwrap();
            if i >= 300 {
                before += b;
                after += rms(&frame);
            }
        }
        assert!(
            after < before * 0.25,
            "noise floor should drop by more than 12 dB: {before:.0} -> {after:.0}"
        );
    }

    #[test]
    fn rejects_partial_blocks() {
        let mut d = Denoiser::new(NoiseSuppressionLevel::Moderate);
        assert!(d.process(&mut [0i16; 479]).is_err());
        assert!(d.process(&mut []).is_err());
        assert!(d.process(&mut [0i16; BLOCK * 2]).is_ok());
    }

    #[test]
    fn low_level_keeps_part_of_the_signal() {
        let raw = Rumble::new(7, 600.0).block(BLOCK * 100);
        let run = |level: NoiseSuppressionLevel| {
            let mut d = Denoiser::new(level);
            let mut pcm = raw.clone();
            for block in pcm.as_chunks_mut::<BLOCK>().0 {
                d.process(block).unwrap();
            }
            rms(&pcm[BLOCK * 50..])
        };
        let high = run(NoiseSuppressionLevel::High);
        let low = run(NoiseSuppressionLevel::Low);
        assert!(low > high, "low={low:.0} high={high:.0}");
        assert!(low < rms(&raw[BLOCK * 50..]));
    }

    #[test]
    fn pool_bounds_sessions_and_returns_slots() {
        let pool = DenoisePool::new(config());
        let a = pool.acquire().expect("first slot");
        let b = pool.acquire().expect("second slot");
        assert!(
            pool.acquire().is_none(),
            "third session exceeds max_sessions"
        );
        assert_eq!(pool.active(), 2);
        drop(a);
        assert_eq!(pool.active(), 1);
        let c = pool.acquire().expect("slot returned");
        drop((b, c));
        assert_eq!(pool.active(), 0);

        let disabled = DenoisePool::new(NoiseSuppressionConfig::default());
        assert!(!disabled.enabled());
        assert!(disabled.acquire().is_none());
    }

    #[test]
    fn opus_cleaner_keeps_frame_duration_and_passes_short_packets() {
        let mut enc =
            opus::Encoder::new(48_000, opus::Channels::Mono, opus::Application::Voip).unwrap();
        let mut cleaner = OpusUplinkCleaner::new(&config()).unwrap();
        let probe = opus::Decoder::new(48_000, opus::Channels::Mono).unwrap();
        let mut rumble = Rumble::new(42, 3000.0);
        let mut out = vec![0u8; 1275];
        for &samples in &[960usize, 480, 1920, 2880] {
            let pcm = rumble.block(samples);
            let n = enc.encode(&pcm, &mut out).unwrap();
            match cleaner.clean(&out[..n]).unwrap() {
                Cleaned::Opus(bytes) => {
                    assert_eq!(probe.get_nb_samples(&bytes).unwrap(), samples);
                    assert_ne!(&bytes[..], &out[..n]);
                }
                other => panic!("{samples}-sample frame must be cleaned, got {other:?}"),
            }
        }
        // 2.5 ms and 5 ms frames are shorter than a model block and pass through.
        let short = rumble.block(240);
        let n = enc.encode(&short, &mut out).unwrap();
        assert_eq!(cleaner.clean(&out[..n]).unwrap(), Cleaned::Passthrough);
        // TOC-only (DTX) packets pass through; garbage is an error.
        assert_eq!(cleaner.clean(&out[..1]).unwrap(), Cleaned::Passthrough);
        assert!(cleaner.clean(&[0xff; 40]).is_err());
    }

    #[test]
    fn uplink_denoiser_reuses_the_outcome_for_a_repeated_frame() {
        let pool = DenoisePool::new(config());
        let mut d = UplinkDenoiser::acquire(&pool).unwrap();
        let mut enc =
            opus::Encoder::new(48_000, opus::Channels::Mono, opus::Application::Voip).unwrap();
        let mut out = vec![0u8; 1275];
        let pcm = Rumble::new(3, 3000.0).block(960);
        let n = enc.encode(&pcm, &mut out).unwrap();
        let Cleaned::Opus(first) = d.clean_opus(960, &out[..n]).unwrap() else {
            panic!("20 ms frame must be cleaned");
        };
        assert_eq!(
            d.clean_opus(960, &out[..n]).unwrap(),
            Cleaned::Repeated(Some(first.clone()))
        );
        // A different timestamp is a new frame even with identical bytes.
        assert!(matches!(
            d.clean_opus(1920, &out[..n]).unwrap(),
            Cleaned::Opus(_)
        ));
        assert_eq!(d.clean_opus(2880, &out[..1]).unwrap(), Cleaned::Passthrough);
        assert_eq!(
            d.clean_opus(2880, &out[..1]).unwrap(),
            Cleaned::Repeated(None)
        );
    }

    #[test]
    fn opus_cleaner_attenuates_noise() {
        let mut enc =
            opus::Encoder::new(48_000, opus::Channels::Mono, opus::Application::Voip).unwrap();
        let mut cleaner = OpusUplinkCleaner::new(&config()).unwrap();
        let mut probe = opus::Decoder::new(48_000, opus::Channels::Mono).unwrap();
        let mut rumble = Rumble::new(99, 600.0);
        let mut out = vec![0u8; 1275];
        let mut decoded = vec![0i16; 960];
        let mut before = 0.0;
        let mut after = 0.0;
        for i in 0..200 {
            let pcm = rumble.block(960);
            let n = enc.encode(&pcm, &mut out).unwrap();
            let Cleaned::Opus(bytes) = cleaner.clean(&out[..n]).unwrap() else {
                panic!("20 ms frame must be cleaned");
            };
            probe.decode(&bytes, &mut decoded, false).unwrap();
            if i >= 150 {
                before += rms(&pcm);
                after += rms(&decoded);
            }
        }
        assert!(after < before * 0.25, "{before:.0} -> {after:.0}");
    }

    #[test]
    fn narrowband_denoiser_handles_every_pcmu_frame_and_attenuates_noise() {
        let mut d = NarrowbandDenoiser::new(NoiseSuppressionLevel::High);
        assert!(d.process(&mut [0i16; 100]).is_err());
        let mut rumble = Rumble::new(5, 600.0);
        for &len in &PCMU_FRAME_SIZES {
            let mut frame = rumble.block(len);
            d.process(&mut frame).unwrap();
        }
        let mut before = 0.0;
        let mut after = 0.0;
        for i in 0..400 {
            let mut frame = rumble.block(160);
            let b = rms(&frame);
            d.process(&mut frame).unwrap();
            if i >= 300 {
                before += b;
                after += rms(&frame);
            }
        }
        assert!(after < before * 0.25, "{before:.0} -> {after:.0}");
    }

    #[test]
    fn narrowband_denoiser_keeps_a_tone() {
        // A 300 Hz tone at 8 kHz survives resampling + model (speech-band, periodic).
        let mut d = NarrowbandDenoiser::new(NoiseSuppressionLevel::High);
        let mut phase = 0.0f32;
        let mut last_out = 0.0;
        let mut last_in = 0.0;
        for _ in 0..150 {
            let mut frame: Vec<i16> = (0..160)
                .map(|_| {
                    phase += 300.0 / 8000.0;
                    (8000.0 * (phase * std::f32::consts::TAU).sin()) as i16
                })
                .collect();
            last_in = rms(&frame);
            d.process(&mut frame).unwrap();
            last_out = rms(&frame);
        }
        assert!(
            last_out > last_in * 0.3,
            "in={last_in:.0} out={last_out:.0}"
        );
    }
}
