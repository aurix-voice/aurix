//! Server-side Opus mixer used for the WebRTC downlink.
//!
//! A browser negotiates a single audio track with the SFU, so audio from every other
//! participant in the channel is decoded, summed with per-sender volume (positional
//! attenuation, whisper, etc.) and re-encoded as one stereo Opus stream. Senders in a
//! directional positional channel are panned across the stereo field by their direction
//! relative to the listener; everybody else sits in the centre. Every sender is decoded as
//! stereo (libopus upmixes mono packets to identical channels at no extra cost), so a stereo
//! uplink — music, a DJ, a broadcast — keeps its image; a directional sender is downmixed
//! before panning since a stereo image and a positional pan are mutually exclusive.
//!
//! The mixer has no jitter buffer: a packet is decoded when it arrives and its PCM queued
//! for the next tick. Loss shows up as a jump in a sender's RTP clock when the following
//! packet lands; the missing frames are then rebuilt from that packet — the one right before
//! it from its in-band FEC, older ones from its Deep REDundancy (libopus 1.5+) — or
//! concealed (neural PLC at [`MixerConfig::decoder_complexity`] `>= 5`) and queued ahead of
//! it. The rebuilt frames play late by however long they were missing, so recovery is capped
//! at [`MAX_RECOVERY_FRAMES`] of added latency per sender; the tail of a longer burst is
//! rebuilt, its head stays the silence that already played out.

use aurix_common::error::{AurixError, Result};
use aurix_common::types::Direction;
use std::collections::{HashMap, VecDeque};
use std::time::Instant;

pub const SAMPLE_RATE: u32 = 48_000;
/// 20 ms at 48 kHz, per channel.
pub const FRAME_SAMPLES: usize = 960;
/// Channels in the encoded downlink.
pub const OUTPUT_CHANNELS: usize = 2;
/// Largest Opus frame we accept from a single packet (120 ms @ 48 kHz).
const MAX_DECODE_SAMPLES: usize = 5760;
/// Drop a sender's queued PCM if it grows past this (network burst / clock drift).
const MAX_QUEUE_SAMPLES: usize = FRAME_SAMPLES * OUTPUT_CHANNELS * 10;
/// Forget decoders for senders that have been silent this long.
const SENDER_IDLE_SECS: u64 = 30;
/// Most lost frames rebuilt or concealed when a sender's next packet arrives — each one
/// delays that sender in the mix by 20 ms until its queue drains, so this bounds the added
/// latency (80 ms).
pub const MAX_RECOVERY_FRAMES: usize = 4;
/// A jump longer than this (1 s) is a pause (VAD gate, DTX, hold) or a restarted clock, not
/// loss: nothing is concealed.
const MAX_GAP_FRAMES: usize = 50;
/// History asked of a packet's DRED for a gap of `n` frames: the redundancy is coded with an
/// encoder-side offset of a few frames, so asking for exactly the gap decodes too little.
fn dred_history_samples(gap: usize) -> usize {
    ((gap + 4) * FRAME_SAMPLES).min(SAMPLE_RATE as usize)
}

/// Tuning of a server mixer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MixerConfig {
    /// Bitrate of the encoded stereo downlink.
    pub bitrate_bps: i32,
    /// libopus decoder complexity `0..=10` for every sender: `>= 5` conceals lost frames
    /// with the neural PLC, `>= 6` adds OSCE speech enhancement of SILK frames. Costs CPU
    /// only while a sender is being concealed (PLC) or enhanced (OSCE).
    pub decoder_complexity: u8,
}

impl Default for MixerConfig {
    fn default() -> Self {
        Self {
            bitrate_bps: 32_000,
            decoder_complexity: 5,
        }
    }
}

impl MixerConfig {
    pub fn with_bitrate(bitrate_bps: i32) -> Self {
        Self {
            bitrate_bps,
            ..Self::default()
        }
    }
}

/// Lifetime loss handling of one mixer.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RecoveryTotals {
    /// Frames missing when a sender's next packet arrived.
    pub lost: u64,
    /// Of `lost`: rebuilt from the next packet's in-band FEC.
    pub fec_recovered: u64,
    /// Of `lost`: rebuilt from the next packet's DRED.
    pub dred_recovered: u64,
    /// Of `lost`: concealed by the decoder's PLC.
    pub concealed: u64,
    /// Of `lost`: neither — beyond [`MAX_RECOVERY_FRAMES`] or the packet's frame size is not
    /// 20 ms.
    pub skipped: u64,
    /// Packets for a slot that had already played out (reordered / duplicate), dropped.
    pub late: u64,
}

struct SenderState {
    decoder: opus::Decoder,
    /// Interleaved stereo PCM awaiting the next mix.
    queue: VecDeque<i16>,
    volume: f32,
    /// Constant-power `(left, right)` gains from the sender's direction; `None` keeps the
    /// sender's own stereo image.
    pan: Option<(f32, f32)>,
    last_seen: Instant,
    /// RTP clock (48 kHz) the next packet should carry; `None` until the first.
    next_ts: Option<u32>,
    /// DRED state of the last packet parsed, reused across gaps.
    dred: Option<opus::Dred>,
}

pub struct OpusMixer {
    senders: HashMap<u32, SenderState>,
    encoder: opus::Encoder,
    decoder_complexity: i32,
    /// `None` on a libopus without DRED.
    dred_decoder: Option<opus::DredDecoder>,
    /// Interleaved stereo accumulator.
    mix_buf: Vec<i32>,
    frame_buf: Vec<i16>,
    decode_buf: Vec<i16>,
    out_buf: Vec<u8>,
    pub frames_mixed: u64,
    pub recovery: RecoveryTotals,
}

impl OpusMixer {
    pub fn new(config: MixerConfig) -> Result<Self> {
        let mut encoder =
            opus::Encoder::new(SAMPLE_RATE, opus::Channels::Stereo, opus::Application::Voip)
                .map_err(|e| AurixError::Codec(format!("opus encoder: {e}")))?;
        encoder
            .set_bitrate(opus::Bitrate::Bits(
                config.bitrate_bps.clamp(6_000, 128_000),
            ))
            .map_err(|e| AurixError::Codec(format!("opus bitrate: {e}")))?;
        let _ = encoder.set_inband_fec(true);
        let _ = encoder.set_packet_loss_perc(10);
        Ok(Self {
            senders: HashMap::new(),
            encoder,
            decoder_complexity: i32::from(config.decoder_complexity.min(10)),
            dred_decoder: opus::DredDecoder::new().ok(),
            mix_buf: vec![0i32; FRAME_SAMPLES * OUTPUT_CHANNELS],
            frame_buf: vec![0i16; FRAME_SAMPLES * OUTPUT_CHANNELS],
            decode_buf: vec![0i16; MAX_DECODE_SAMPLES * OUTPUT_CHANNELS],
            out_buf: vec![0u8; 1275],
            frames_mixed: 0,
            recovery: RecoveryTotals::default(),
        })
    }

    /// This build's libopus parses Deep REDundancy.
    pub fn dred_supported(&self) -> bool {
        self.dred_decoder.is_some()
    }

    /// Decode one Opus packet from `sender` and queue its PCM for the next mix, remembering
    /// the gain and direction the sender should be rendered with. `timestamp` is the
    /// sender's RTP clock for the packet (48 kHz): a jump past the previous packet's end
    /// means frames were lost and they are rebuilt / concealed first; a packet behind it
    /// (reordered, duplicate) is dropped since its slot already played out.
    pub fn push_opus(
        &mut self,
        sender: u32,
        timestamp: u32,
        volume: f32,
        direction: Option<Direction>,
        packet: &[u8],
    ) -> Result<()> {
        if packet.is_empty() {
            return Ok(());
        }
        let pan = direction.as_ref().map(Direction::stereo_gains);
        let state = match self.senders.get_mut(&sender) {
            Some(s) => s,
            None => {
                let mut decoder = opus::Decoder::new(SAMPLE_RATE, opus::Channels::Stereo)
                    .map_err(|e| AurixError::Codec(format!("opus decoder: {e}")))?;
                let _ = decoder.set_complexity(self.decoder_complexity);
                self.senders.insert(
                    sender,
                    SenderState {
                        decoder,
                        queue: VecDeque::new(),
                        volume,
                        pan,
                        last_seen: Instant::now(),
                        next_ts: None,
                        dred: None,
                    },
                );
                self.senders.get_mut(&sender).expect("just inserted")
            }
        };
        state.volume = volume;
        state.pan = pan;
        state.last_seen = Instant::now();
        let samples = state
            .decoder
            .get_nb_samples(packet)
            .map_err(|e| AurixError::Codec(format!("opus packet: {e}")))?;
        if let Some(expected) = state.next_ts {
            let jump = timestamp.wrapping_sub(expected) as i32;
            if jump < 0 && jump > -((MAX_GAP_FRAMES * FRAME_SAMPLES) as i32) {
                self.recovery.late += 1;
                return Ok(());
            }
            let gap = (jump.max(0) as usize) / FRAME_SAMPLES;
            if (1..=MAX_GAP_FRAMES).contains(&gap) {
                Self::recover(
                    state,
                    self.dred_decoder.as_mut(),
                    &mut self.recovery,
                    &mut self.decode_buf,
                    gap,
                    samples == FRAME_SAMPLES,
                    packet,
                );
            }
        }
        state.next_ts = Some(timestamp.wrapping_add(samples as u32));
        let n = state
            .decoder
            .decode(packet, &mut self.decode_buf, false)
            .map_err(|e| AurixError::Codec(format!("opus decode: {e}")))?;
        Self::enqueue(state, &self.decode_buf[..n * OUTPUT_CHANNELS]);
        Ok(())
    }

    fn enqueue(state: &mut SenderState, pcm: &[i16]) {
        state.queue.extend(pcm.iter().copied());
        while state.queue.len() > MAX_QUEUE_SAMPLES {
            state.queue.pop_front();
        }
    }

    /// Fill the `gap` frames missing before `packet`: the last of them from the packet's
    /// in-band FEC, older ones from its DRED, the rest by PLC — newest-covered first is
    /// irrelevant, they are queued oldest first. Only the tail that fits the latency budget
    /// is filled, and only for 20 ms packets (`recoverable`).
    fn recover(
        state: &mut SenderState,
        dred_decoder: Option<&mut opus::DredDecoder>,
        totals: &mut RecoveryTotals,
        buf: &mut [i16],
        gap: usize,
        recoverable: bool,
        packet: &[u8],
    ) {
        totals.lost += gap as u64;
        let queued_frames = state.queue.len() / (FRAME_SAMPLES * OUTPUT_CHANNELS);
        let fill = if recoverable {
            gap.min(MAX_RECOVERY_FRAMES.saturating_sub(queued_frames))
        } else {
            0
        };
        let skipped = (gap - fill) as u64;
        totals.skipped += skipped;
        if skipped > 0 {
            aurix_metrics::MIXER_LOST_FRAMES
                .with_label_values(&["skipped"])
                .inc_by(skipped);
        }
        if fill == 0 {
            return;
        }
        let lbrr = opus::packet::has_lbrr(packet).unwrap_or(false);
        let dred_samples = if fill > 1 || !lbrr {
            dred_decoder
                .and_then(|dd| {
                    let dred = match state.dred.as_mut() {
                        Some(d) => d,
                        None => state.dred.insert(opus::Dred::new().ok()?),
                    };
                    dd.parse(dred, packet, dred_history_samples(fill), SAMPLE_RATE)
                        .ok()
                })
                .unwrap_or(0)
        } else {
            0
        };
        let out = &mut buf[..FRAME_SAMPLES * OUTPUT_CHANNELS];
        for remaining in (1..=fill).rev() {
            let offset = remaining * FRAME_SAMPLES;
            let method = if remaining == 1 && lbrr {
                state.decoder.decode(packet, out, true).map(|_| "fec")
            } else if dred_samples >= offset {
                match state.dred.as_ref() {
                    Some(dred) => state.decoder.dred_decode(dred, offset, out).map(|_| "dred"),
                    None => state.decoder.decode(&[], out, false).map(|_| "plc"),
                }
            } else {
                state.decoder.decode(&[], out, false).map(|_| "plc")
            };
            let method = match method {
                Ok(m) => m,
                Err(_) => match state.decoder.decode(&[], out, false) {
                    Ok(_) => "plc",
                    Err(_) => {
                        totals.skipped += 1;
                        aurix_metrics::MIXER_LOST_FRAMES
                            .with_label_values(&["skipped"])
                            .inc();
                        continue;
                    }
                },
            };
            match method {
                "fec" => totals.fec_recovered += 1,
                "dred" => totals.dred_recovered += 1,
                _ => totals.concealed += 1,
            }
            aurix_metrics::MIXER_LOST_FRAMES
                .with_label_values(&[method])
                .inc();
            Self::enqueue(state, out);
        }
    }

    /// Mix one 20 ms stereo frame from all senders with queued audio. Returns `None` when silent.
    pub fn mix_frame(&mut self) -> Result<Option<&[u8]>> {
        let now = Instant::now();
        self.senders
            .retain(|_, s| now.duration_since(s.last_seen).as_secs() < SENDER_IDLE_SECS);

        let mut any = false;
        self.mix_buf.iter_mut().for_each(|s| *s = 0);
        for state in self.senders.values_mut() {
            if state.queue.is_empty() {
                continue;
            }
            any = true;
            let vol = state
                .volume
                .clamp(0.0, crate::session::MAX_PARTICIPANT_GAIN);
            for frame in self.mix_buf.as_chunks_mut::<OUTPUT_CHANNELS>().0 {
                let (Some(l), Some(r)) = (state.queue.pop_front(), state.queue.pop_front()) else {
                    break;
                };
                match state.pan {
                    Some((left, right)) => {
                        let mono = (l as f32 + r as f32) * 0.5;
                        frame[0] += (mono * left * vol) as i32;
                        frame[1] += (mono * right * vol) as i32;
                    }
                    None => {
                        frame[0] += (l as f32 * vol) as i32;
                        frame[1] += (r as f32 * vol) as i32;
                    }
                }
            }
        }
        if !any {
            return Ok(None);
        }
        for (dst, src) in self.frame_buf.iter_mut().zip(self.mix_buf.iter()) {
            *dst = (*src).clamp(i16::MIN as i32, i16::MAX as i32) as i16;
        }
        let len = self
            .encoder
            .encode(&self.frame_buf, &mut self.out_buf)
            .map_err(|e| AurixError::Codec(format!("opus encode: {e}")))?;
        self.frames_mixed += 1;
        Ok(Some(&self.out_buf[..len]))
    }

    pub fn active_senders(&self) -> usize {
        self.senders.len()
    }
}

/// Encode a 20 ms PCM frame — used by tests and tooling to synthesize valid Opus packets.
pub fn encode_pcm_frame(pcm: &[i16]) -> Result<Vec<u8>> {
    let mut enc = opus::Encoder::new(SAMPLE_RATE, opus::Channels::Mono, opus::Application::Voip)
        .map_err(|e| AurixError::Codec(format!("opus encoder: {e}")))?;
    let mut out = vec![0u8; 1275];
    let n = enc
        .encode(pcm, &mut out)
        .map_err(|e| AurixError::Codec(format!("opus encode: {e}")))?;
    out.truncate(n);
    Ok(out)
}

/// Encode a 20 ms interleaved stereo PCM frame (tests and tooling).
pub fn encode_stereo_pcm_frame(pcm: &[i16]) -> Result<Vec<u8>> {
    let mut enc = opus::Encoder::new(
        SAMPLE_RATE,
        opus::Channels::Stereo,
        opus::Application::Audio,
    )
    .map_err(|e| AurixError::Codec(format!("opus encoder: {e}")))?;
    enc.set_bitrate(opus::Bitrate::Bits(96_000))
        .map_err(|e| AurixError::Codec(format!("opus bitrate: {e}")))?;
    let mut out = vec![0u8; 1275];
    let n = enc
        .encode(pcm, &mut out)
        .map_err(|e| AurixError::Codec(format!("opus encode: {e}")))?;
    out.truncate(n);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A sender's RTP clock: one 20 ms frame per packet.
    #[derive(Default)]
    struct Clock(u32);

    impl Clock {
        fn next(&mut self) -> u32 {
            let ts = self.0;
            self.0 = self.0.wrapping_add(FRAME_SAMPLES as u32);
            ts
        }
    }

    fn tone(freq: f32, amp: f32) -> Vec<i16> {
        (0..FRAME_SAMPLES)
            .map(|i| {
                ((i as f32 * freq * std::f32::consts::TAU / SAMPLE_RATE as f32).sin()
                    * amp
                    * i16::MAX as f32) as i16
            })
            .collect()
    }

    #[test]
    fn mixes_two_senders_into_one_opus_frame() {
        let mut mixer = OpusMixer::new(MixerConfig::with_bitrate(32_000)).unwrap();
        let mut ts = Clock::default();
        assert!(
            mixer.mix_frame().unwrap().is_none(),
            "silence yields no frame"
        );

        let a = encode_pcm_frame(&tone(440.0, 0.3)).unwrap();
        let b = encode_pcm_frame(&tone(880.0, 0.3)).unwrap();
        mixer.push_opus(1, ts.next(), 1.0, None, &a).unwrap();
        mixer.push_opus(2, ts.next(), 0.5, None, &b).unwrap();
        assert_eq!(mixer.active_senders(), 2);

        let frame = mixer.mix_frame().unwrap().expect("mixed frame");
        assert!(!frame.is_empty() && frame.len() <= 1275);

        // Decoding the mixed frame yields a full 20 ms of non-silent stereo audio, identical in
        // both ears for centred senders.
        let (l, r) = decode_stereo(frame);
        assert!(l.iter().any(|s| s.abs() > 100));
        assert!((rms(&l) - rms(&r)).abs() / rms(&l) < 0.05);

        assert!(mixer.mix_frame().unwrap().is_none(), "queues drained");
    }

    fn decode_stereo(frame: &[u8]) -> (Vec<i16>, Vec<i16>) {
        let mut dec = opus::Decoder::new(SAMPLE_RATE, opus::Channels::Stereo).unwrap();
        let mut pcm = vec![0i16; FRAME_SAMPLES * OUTPUT_CHANNELS];
        let n = dec.decode(frame, &mut pcm, false).unwrap();
        assert_eq!(n, FRAME_SAMPLES);
        let left = pcm.iter().step_by(2).copied().collect();
        let right = pcm.iter().skip(1).step_by(2).copied().collect();
        (left, right)
    }

    fn rms(pcm: &[i16]) -> f32 {
        (pcm.iter().map(|s| (*s as f32).powi(2)).sum::<f32>() / pcm.len() as f32).sqrt()
    }

    #[test]
    fn directional_sender_is_panned_and_mono_decoders_still_hear_it() {
        let mut mixer = OpusMixer::new(MixerConfig::with_bitrate(48_000)).unwrap();
        let mut ts = Clock::default();
        let a = encode_pcm_frame(&tone(440.0, 0.3)).unwrap();
        let right = Direction {
            azimuth: std::f32::consts::FRAC_PI_2,
            elevation: 0.0,
        };
        // Prime the encoder/decoder (the first Opus frame carries codec warm-up).
        for _ in 0..3 {
            mixer.push_opus(1, ts.next(), 1.0, Some(right), &a).unwrap();
            mixer.mix_frame().unwrap().unwrap();
        }
        mixer.push_opus(1, ts.next(), 1.0, Some(right), &a).unwrap();
        let frame = mixer.mix_frame().unwrap().unwrap().to_vec();
        let (l, r) = decode_stereo(&frame);
        assert!(
            rms(&l) < rms(&r) * 0.25,
            "hard right: left {} vs right {}",
            rms(&l),
            rms(&r)
        );

        // Pan follows the latest direction for the sender.
        let left = Direction {
            azimuth: -std::f32::consts::FRAC_PI_2,
            elevation: 0.0,
        };
        for _ in 0..3 {
            mixer.push_opus(1, ts.next(), 1.0, Some(left), &a).unwrap();
            mixer.mix_frame().unwrap().unwrap();
        }
        mixer.push_opus(1, ts.next(), 1.0, Some(left), &a).unwrap();
        let frame = mixer.mix_frame().unwrap().unwrap().to_vec();
        let (l, r) = decode_stereo(&frame);
        assert!(rms(&r) < rms(&l) * 0.25, "hard left");

        // A legacy mono decoder still gets the voice (Opus downmixes).
        let mut mono = opus::Decoder::new(SAMPLE_RATE, opus::Channels::Mono).unwrap();
        let mut pcm = vec![0i16; FRAME_SAMPLES];
        let n = mono.decode(&frame, &mut pcm, false).unwrap();
        assert_eq!(n, FRAME_SAMPLES);
        assert!(rms(&pcm) > 1000.0);
    }

    #[test]
    fn rejects_garbage_packets() {
        let mut mixer = OpusMixer::new(MixerConfig::with_bitrate(32_000)).unwrap();
        let mut ts = Clock::default();
        assert!(mixer
            .push_opus(7, ts.next(), 1.0, None, &[0xff; 3])
            .is_err());
    }

    #[test]
    fn stereo_sender_keeps_its_image_unless_panned() {
        let mut mixer = OpusMixer::new(MixerConfig::with_bitrate(96_000)).unwrap();
        let mut ts = Clock::default();
        // Tone hard left only: L = tone, R = silence.
        let left_only: Vec<i16> = tone(440.0, 0.3).into_iter().flat_map(|s| [s, 0]).collect();
        let packet = encode_stereo_pcm_frame(&left_only).unwrap();
        assert!(
            aurix_common::protocol::opus_packet_is_stereo(&packet),
            "test packet must carry the stereo TOC bit"
        );
        for _ in 0..3 {
            mixer.push_opus(1, ts.next(), 1.0, None, &packet).unwrap();
            mixer.mix_frame().unwrap().unwrap();
        }
        mixer.push_opus(1, ts.next(), 1.0, None, &packet).unwrap();
        let frame = mixer.mix_frame().unwrap().unwrap().to_vec();
        let (l, r) = decode_stereo(&frame);
        assert!(
            rms(&r) < rms(&l) * 0.2,
            "image preserved: left {} vs right {}",
            rms(&l),
            rms(&r)
        );

        // With a direction the image collapses to mono and follows the pan (hard right here).
        let right = Direction {
            azimuth: std::f32::consts::FRAC_PI_2,
            elevation: 0.0,
        };
        for _ in 0..3 {
            mixer
                .push_opus(1, ts.next(), 1.0, Some(right), &packet)
                .unwrap();
            mixer.mix_frame().unwrap().unwrap();
        }
        mixer
            .push_opus(1, ts.next(), 1.0, Some(right), &packet)
            .unwrap();
        let frame = mixer.mix_frame().unwrap().unwrap().to_vec();
        let (l, r) = decode_stereo(&frame);
        assert!(rms(&l) < rms(&r) * 0.25, "panned right after downmix");

        // A mono sender still mixes alongside (same decoder type, upmixed by libopus).
        let mono = encode_pcm_frame(&tone(880.0, 0.3)).unwrap();
        assert!(!aurix_common::protocol::opus_packet_is_stereo(&mono));
        mixer.push_opus(2, ts.next(), 1.0, None, &mono).unwrap();
        let frame = mixer.mix_frame().unwrap().unwrap().to_vec();
        let (l, r) = decode_stereo(&frame);
        assert!(rms(&l) > 500.0 && rms(&r) > 500.0);
    }
}

#[cfg(test)]
mod loss_tests {
    use super::*;
    use opus::testing::speech_like_i16;

    const TS: u32 = FRAME_SAMPLES as u32;

    /// Speech-like mono uplink with in-band FEC and DRED, as a native client on the
    /// `Moderate` / `High` loss profile sends it.
    fn redundant_packets(frames: usize, fec: bool, dred_ms: u32) -> Vec<Vec<u8>> {
        let mut e =
            opus::Encoder::new(SAMPLE_RATE, opus::Channels::Mono, opus::Application::Voip).unwrap();
        e.set_bitrate(opus::Bitrate::Bits(40_000)).unwrap();
        e.set_signal(opus::Signal::Voice).unwrap();
        e.set_complexity(10).unwrap();
        e.set_inband_fec(fec).unwrap();
        e.set_packet_loss_perc(if fec { 20 } else { 0 }).unwrap();
        if dred_ms > 0 {
            e.set_dred_duration(dred_ms / 10).unwrap();
        }
        speech_like_i16(frames)
            .chunks(FRAME_SAMPLES)
            .map(|f| e.encode_vec(f, 1275).unwrap())
            .collect()
    }

    fn mixer() -> OpusMixer {
        OpusMixer::new(MixerConfig::default()).unwrap()
    }

    fn push(mixer: &mut OpusMixer, packets: &[Vec<u8>], i: usize) {
        mixer
            .push_opus(1, i as u32 * TS, 1.0, None, &packets[i])
            .unwrap();
    }

    fn drain(mixer: &mut OpusMixer) -> usize {
        let mut n = 0;
        while mixer.mix_frame().unwrap().is_some() {
            n += 1;
        }
        n
    }

    #[test]
    fn one_lost_frame_is_rebuilt_from_the_next_packets_fec() {
        let packets = redundant_packets(60, true, 0);
        let mut m = mixer();
        for i in 0..30 {
            push(&mut m, &packets, i);
        }
        drain(&mut m);
        // Lose a frame whose successor carries LBRR (libopus emits it for voiced frames).
        let lost = (30..55)
            .find(|&i| opus::packet::has_lbrr(&packets[i + 1]).unwrap())
            .expect("an LBRR-carrying packet");
        for i in 30..lost {
            push(&mut m, &packets, i);
            drain(&mut m);
        }
        push(&mut m, &packets, lost + 1);
        assert_eq!(drain(&mut m), 2, "the lost frame plus the packet's own");
        assert_eq!(
            m.recovery,
            RecoveryTotals {
                lost: 1,
                fec_recovered: 1,
                ..RecoveryTotals::default()
            }
        );
    }

    #[test]
    fn burst_is_rebuilt_from_dred_within_the_latency_budget() {
        let packets = redundant_packets(60, true, 200);
        let mut m = mixer();
        assert!(m.dred_supported());
        for i in 0..40 {
            push(&mut m, &packets, i);
        }
        drain(&mut m);
        // Frames 40..45 lost (6 × 20 ms); packet 46 arrives.
        push(&mut m, &packets, 46);
        let r = m.recovery;
        assert_eq!(r.lost, 6);
        assert_eq!(
            r.fec_recovered + r.dred_recovered + r.concealed,
            MAX_RECOVERY_FRAMES as u64,
            "{r:?}"
        );
        assert_eq!(r.skipped, 2, "{r:?}");
        assert!(r.dred_recovered >= 2, "DRED covers the older frames: {r:?}");
        assert_eq!(drain(&mut m), MAX_RECOVERY_FRAMES + 1);
    }

    #[test]
    fn plc_conceals_when_the_packet_carries_no_redundancy() {
        let packets = redundant_packets(50, false, 0);
        let mut m = mixer();
        for i in 0..30 {
            push(&mut m, &packets, i);
        }
        drain(&mut m);
        push(&mut m, &packets, 32);
        assert_eq!(
            m.recovery,
            RecoveryTotals {
                lost: 2,
                concealed: 2,
                ..RecoveryTotals::default()
            }
        );
        assert_eq!(drain(&mut m), 3);
    }

    #[test]
    fn recovery_yields_to_audio_already_queued() {
        let packets = redundant_packets(50, true, 200);
        let mut m = mixer();
        for i in 0..30 {
            push(&mut m, &packets, i);
        }
        drain(&mut m);
        // Three frames waiting for the mixer: only one more may be added for the gap.
        for i in 30..33 {
            push(&mut m, &packets, i);
        }
        push(&mut m, &packets, 37);
        let r = m.recovery;
        assert_eq!(r.lost, 4);
        assert_eq!(r.skipped, 3, "{r:?}");
        assert_eq!(r.fec_recovered + r.dred_recovered + r.concealed, 1);
        assert_eq!(drain(&mut m), 5);
    }

    #[test]
    fn late_and_duplicate_packets_are_dropped_and_pauses_are_not_loss() {
        let packets = redundant_packets(50, true, 200);
        let mut m = mixer();
        for i in 0..10 {
            push(&mut m, &packets, i);
        }
        push(&mut m, &packets, 9); // duplicate
        push(&mut m, &packets, 7); // reordered, slot played out
        assert_eq!(drain(&mut m), 10);
        assert_eq!(m.recovery.late, 2);

        // A VAD / DTX pause longer than MAX_GAP_FRAMES: the clock jumps, nothing is concealed.
        m.push_opus(1, 10 * TS + 200 * TS, 1.0, None, &packets[10])
            .unwrap();
        assert_eq!(drain(&mut m), 1);
        assert_eq!(m.recovery.lost, 0);
        assert_eq!(m.recovery.skipped, 0);
    }

    #[test]
    fn non_20ms_packets_are_not_used_for_recovery() {
        let mut e =
            opus::Encoder::new(SAMPLE_RATE, opus::Channels::Mono, opus::Application::Voip).unwrap();
        e.set_inband_fec(true).unwrap();
        e.set_packet_loss_perc(20).unwrap();
        let pcm = speech_like_i16(20);
        let packets: Vec<Vec<u8>> = pcm
            .chunks(FRAME_SAMPLES * 2)
            .map(|f| e.encode_vec(f, 1275).unwrap())
            .collect();
        let mut m = mixer();
        m.push_opus(1, 0, 1.0, None, &packets[0]).unwrap();
        m.push_opus(1, 2 * TS, 1.0, None, &packets[1]).unwrap();
        assert_eq!(drain(&mut m), 4, "two 40 ms packets");
        m.push_opus(1, 6 * TS, 1.0, None, &packets[3]).unwrap();
        assert_eq!(
            m.recovery,
            RecoveryTotals {
                lost: 2,
                skipped: 2,
                ..RecoveryTotals::default()
            }
        );
        assert_eq!(drain(&mut m), 2);
    }

    #[test]
    fn decoder_complexity_follows_the_config() {
        let m = OpusMixer::new(MixerConfig {
            bitrate_bps: 32_000,
            decoder_complexity: 7,
        })
        .unwrap();
        assert_eq!(m.decoder_complexity, 7);
        let mut m = m;
        let packets = redundant_packets(3, false, 0);
        push(&mut m, &packets, 0);
        assert_eq!(
            m.senders
                .get_mut(&1)
                .unwrap()
                .decoder
                .get_complexity()
                .unwrap(),
            7
        );
    }
}
