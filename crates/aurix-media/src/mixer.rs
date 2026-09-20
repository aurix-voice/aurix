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

struct SenderState {
    decoder: opus::Decoder,
    /// Interleaved stereo PCM awaiting the next mix.
    queue: VecDeque<i16>,
    volume: f32,
    /// Constant-power `(left, right)` gains from the sender's direction; `None` keeps the
    /// sender's own stereo image.
    pan: Option<(f32, f32)>,
    last_seen: Instant,
}

pub struct OpusMixer {
    senders: HashMap<u32, SenderState>,
    encoder: opus::Encoder,
    /// Interleaved stereo accumulator.
    mix_buf: Vec<i32>,
    frame_buf: Vec<i16>,
    decode_buf: Vec<i16>,
    out_buf: Vec<u8>,
    pub frames_mixed: u64,
}

impl OpusMixer {
    pub fn new(bitrate_bps: i32) -> Result<Self> {
        let mut encoder =
            opus::Encoder::new(SAMPLE_RATE, opus::Channels::Stereo, opus::Application::Voip)
                .map_err(|e| AurixError::Codec(format!("opus encoder: {e}")))?;
        encoder
            .set_bitrate(opus::Bitrate::Bits(bitrate_bps.clamp(6_000, 128_000)))
            .map_err(|e| AurixError::Codec(format!("opus bitrate: {e}")))?;
        let _ = encoder.set_inband_fec(true);
        let _ = encoder.set_packet_loss_perc(10);
        Ok(Self {
            senders: HashMap::new(),
            encoder,
            mix_buf: vec![0i32; FRAME_SAMPLES * OUTPUT_CHANNELS],
            frame_buf: vec![0i16; FRAME_SAMPLES * OUTPUT_CHANNELS],
            decode_buf: vec![0i16; MAX_DECODE_SAMPLES * OUTPUT_CHANNELS],
            out_buf: vec![0u8; 1275],
            frames_mixed: 0,
        })
    }

    /// Decode one Opus packet from `sender` and queue its PCM for the next mix, remembering
    /// the gain and direction the sender should be rendered with.
    pub fn push_opus(
        &mut self,
        sender: u32,
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
                let decoder = opus::Decoder::new(SAMPLE_RATE, opus::Channels::Stereo)
                    .map_err(|e| AurixError::Codec(format!("opus decoder: {e}")))?;
                self.senders.insert(
                    sender,
                    SenderState {
                        decoder,
                        queue: VecDeque::new(),
                        volume,
                        pan,
                        last_seen: Instant::now(),
                    },
                );
                self.senders.get_mut(&sender).expect("just inserted")
            }
        };
        state.volume = volume;
        state.pan = pan;
        state.last_seen = Instant::now();
        let n = state
            .decoder
            .decode(packet, &mut self.decode_buf, false)
            .map_err(|e| AurixError::Codec(format!("opus decode: {e}")))?;
        state
            .queue
            .extend(self.decode_buf[..n * OUTPUT_CHANNELS].iter().copied());
        while state.queue.len() > MAX_QUEUE_SAMPLES {
            state.queue.pop_front();
        }
        Ok(())
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
        let mut mixer = OpusMixer::new(32_000).unwrap();
        assert!(
            mixer.mix_frame().unwrap().is_none(),
            "silence yields no frame"
        );

        let a = encode_pcm_frame(&tone(440.0, 0.3)).unwrap();
        let b = encode_pcm_frame(&tone(880.0, 0.3)).unwrap();
        mixer.push_opus(1, 1.0, None, &a).unwrap();
        mixer.push_opus(2, 0.5, None, &b).unwrap();
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
        let mut mixer = OpusMixer::new(48_000).unwrap();
        let a = encode_pcm_frame(&tone(440.0, 0.3)).unwrap();
        let right = Direction {
            azimuth: std::f32::consts::FRAC_PI_2,
            elevation: 0.0,
        };
        // Prime the encoder/decoder (the first Opus frame carries codec warm-up).
        for _ in 0..3 {
            mixer.push_opus(1, 1.0, Some(right), &a).unwrap();
            mixer.mix_frame().unwrap().unwrap();
        }
        mixer.push_opus(1, 1.0, Some(right), &a).unwrap();
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
            mixer.push_opus(1, 1.0, Some(left), &a).unwrap();
            mixer.mix_frame().unwrap().unwrap();
        }
        mixer.push_opus(1, 1.0, Some(left), &a).unwrap();
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
        let mut mixer = OpusMixer::new(32_000).unwrap();
        assert!(mixer.push_opus(7, 1.0, None, &[0xff; 3]).is_err());
    }

    #[test]
    fn stereo_sender_keeps_its_image_unless_panned() {
        let mut mixer = OpusMixer::new(96_000).unwrap();
        // Tone hard left only: L = tone, R = silence.
        let left_only: Vec<i16> = tone(440.0, 0.3).into_iter().flat_map(|s| [s, 0]).collect();
        let packet = encode_stereo_pcm_frame(&left_only).unwrap();
        assert!(
            aurix_common::protocol::opus_packet_is_stereo(&packet),
            "test packet must carry the stereo TOC bit"
        );
        for _ in 0..3 {
            mixer.push_opus(1, 1.0, None, &packet).unwrap();
            mixer.mix_frame().unwrap().unwrap();
        }
        mixer.push_opus(1, 1.0, None, &packet).unwrap();
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
            mixer.push_opus(1, 1.0, Some(right), &packet).unwrap();
            mixer.mix_frame().unwrap().unwrap();
        }
        mixer.push_opus(1, 1.0, Some(right), &packet).unwrap();
        let frame = mixer.mix_frame().unwrap().unwrap().to_vec();
        let (l, r) = decode_stereo(&frame);
        assert!(rms(&l) < rms(&r) * 0.25, "panned right after downmix");

        // A mono sender still mixes alongside (same decoder type, upmixed by libopus).
        let mono = encode_pcm_frame(&tone(880.0, 0.3)).unwrap();
        assert!(!aurix_common::protocol::opus_packet_is_stereo(&mono));
        mixer.push_opus(2, 1.0, None, &mono).unwrap();
        let frame = mixer.mix_frame().unwrap().unwrap().to_vec();
        let (l, r) = decode_stereo(&frame);
        assert!(rms(&l) > 500.0 && rms(&r) > 500.0);
    }
}
