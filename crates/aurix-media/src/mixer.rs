//! Server-side Opus mixer used for the WebRTC downlink.
//!
//! A browser negotiates a single audio track with the SFU, so audio from every other
//! participant in the channel is decoded, summed with per-sender volume (positional
//! attenuation, whisper, etc.) and re-encoded as one Opus stream.

use aurix_common::error::{AurixError, Result};
use std::collections::{HashMap, VecDeque};
use std::time::Instant;

pub const SAMPLE_RATE: u32 = 48_000;
/// 20 ms at 48 kHz mono.
pub const FRAME_SAMPLES: usize = 960;
/// Largest Opus frame we accept from a single packet (120 ms @ 48 kHz).
const MAX_DECODE_SAMPLES: usize = 5760;
/// Drop a sender's queued PCM if it grows past this (network burst / clock drift).
const MAX_QUEUE_SAMPLES: usize = FRAME_SAMPLES * 10;
/// Forget decoders for senders that have been silent this long.
const SENDER_IDLE_SECS: u64 = 30;

struct SenderState {
    decoder: opus::Decoder,
    queue: VecDeque<i16>,
    volume: f32,
    last_seen: Instant,
}

pub struct OpusMixer {
    senders: HashMap<u32, SenderState>,
    encoder: opus::Encoder,
    mix_buf: Vec<i32>,
    frame_buf: Vec<i16>,
    decode_buf: Vec<i16>,
    out_buf: Vec<u8>,
    pub frames_mixed: u64,
}

impl OpusMixer {
    pub fn new(bitrate_bps: i32) -> Result<Self> {
        let mut encoder = opus::Encoder::new(SAMPLE_RATE, opus::Channels::Mono, opus::Application::Voip)
            .map_err(|e| AurixError::Codec(format!("opus encoder: {e}")))?;
        encoder
            .set_bitrate(opus::Bitrate::Bits(bitrate_bps.clamp(6_000, 128_000)))
            .map_err(|e| AurixError::Codec(format!("opus bitrate: {e}")))?;
        let _ = encoder.set_inband_fec(true);
        let _ = encoder.set_packet_loss_perc(10);
        Ok(Self {
            senders: HashMap::new(),
            encoder,
            mix_buf: vec![0i32; FRAME_SAMPLES],
            frame_buf: vec![0i16; FRAME_SAMPLES],
            decode_buf: vec![0i16; MAX_DECODE_SAMPLES],
            out_buf: vec![0u8; 1275],
            frames_mixed: 0,
        })
    }

    /// Decode one Opus packet from `sender` and queue its PCM for the next mix.
    pub fn push_opus(&mut self, sender: u32, volume: f32, packet: &[u8]) -> Result<()> {
        if packet.is_empty() {
            return Ok(());
        }
        let state = match self.senders.get_mut(&sender) {
            Some(s) => s,
            None => {
                let decoder = opus::Decoder::new(SAMPLE_RATE, opus::Channels::Mono)
                    .map_err(|e| AurixError::Codec(format!("opus decoder: {e}")))?;
                self.senders.insert(
                    sender,
                    SenderState { decoder, queue: VecDeque::new(), volume, last_seen: Instant::now() },
                );
                self.senders.get_mut(&sender).expect("just inserted")
            }
        };
        state.volume = volume;
        state.last_seen = Instant::now();
        let n = state
            .decoder
            .decode(packet, &mut self.decode_buf, false)
            .map_err(|e| AurixError::Codec(format!("opus decode: {e}")))?;
        state.queue.extend(self.decode_buf[..n].iter().copied());
        while state.queue.len() > MAX_QUEUE_SAMPLES {
            state.queue.pop_front();
        }
        Ok(())
    }

    /// Mix one 20 ms frame from all senders with queued audio. Returns `None` when silent.
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
            let vol = state.volume.clamp(0.0, 1.0);
            for slot in self.mix_buf.iter_mut() {
                match state.queue.pop_front() {
                    Some(sample) => *slot += (sample as f32 * vol) as i32,
                    None => break,
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
    let n = enc.encode(pcm, &mut out).map_err(|e| AurixError::Codec(format!("opus encode: {e}")))?;
    out.truncate(n);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tone(freq: f32, amp: f32) -> Vec<i16> {
        (0..FRAME_SAMPLES)
            .map(|i| ((i as f32 * freq * std::f32::consts::TAU / SAMPLE_RATE as f32).sin() * amp * i16::MAX as f32) as i16)
            .collect()
    }

    #[test]
    fn mixes_two_senders_into_one_opus_frame() {
        let mut mixer = OpusMixer::new(32_000).unwrap();
        assert!(mixer.mix_frame().unwrap().is_none(), "silence yields no frame");

        let a = encode_pcm_frame(&tone(440.0, 0.3)).unwrap();
        let b = encode_pcm_frame(&tone(880.0, 0.3)).unwrap();
        mixer.push_opus(1, 1.0, &a).unwrap();
        mixer.push_opus(2, 0.5, &b).unwrap();
        assert_eq!(mixer.active_senders(), 2);

        let frame = mixer.mix_frame().unwrap().expect("mixed frame");
        assert!(!frame.is_empty() && frame.len() <= 1275);

        // Decoding the mixed frame yields a full 20 ms of non-silent audio.
        let mut dec = opus::Decoder::new(SAMPLE_RATE, opus::Channels::Mono).unwrap();
        let mut pcm = vec![0i16; FRAME_SAMPLES];
        let n = dec.decode(frame, &mut pcm, false).unwrap();
        assert_eq!(n, FRAME_SAMPLES);
        assert!(pcm.iter().any(|s| s.abs() > 100));

        assert!(mixer.mix_frame().unwrap().is_none(), "queues drained");
    }

    #[test]
    fn rejects_garbage_packets() {
        let mut mixer = OpusMixer::new(32_000).unwrap();
        assert!(mixer.push_opus(7, 1.0, &[0xff; 3]).is_err());
    }
}
