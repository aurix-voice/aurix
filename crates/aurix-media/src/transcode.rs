//! PCMU ⇄ Opus transcoding for sessions that negotiated `AudioCodec::Pcmu`.
//!
//! Channels always carry Opus. A PCMU session's uplink frames are decoded from μ-law and
//! re-encoded with an 8 kHz Opus encoder before they enter the router (so recording, STT,
//! cascade and every Opus receiver see an ordinary Opus stream); Opus frames addressed to a
//! PCMU receiver are decoded at 8 kHz and μ-law encoded per sender.

use aurix_common::error::{AurixError, Result};
use aurix_common::g711::{self, PCMU_FRAME_SIZES, PCMU_SAMPLE_RATE};
use bytes::Bytes;
use std::time::Instant;

/// Opus bitrate of the transcoded uplink of a PCMU session. Narrowband speech is transparent
/// well below this; it stays comfortably under every channel policy floor.
pub const PCMU_UPLINK_BITRATE: i32 = 24_000;
/// Longest Opus frame a PCMU receiver may get (120 ms @ 8 kHz).
const MAX_DECODE_SAMPLES: usize = 960;

/// μ-law → Opus for one session's uplink.
pub struct PcmuUplink {
    encoder: opus::Encoder,
    pcm: Vec<i16>,
    out: Vec<u8>,
}

impl std::fmt::Debug for PcmuUplink {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("PcmuUplink")
    }
}

impl PcmuUplink {
    pub fn new() -> Result<Self> {
        let mut encoder = opus::Encoder::new(
            PCMU_SAMPLE_RATE,
            opus::Channels::Mono,
            opus::Application::Voip,
        )
        .map_err(|e| AurixError::Codec(format!("pcmu uplink encoder: {e}")))?;
        encoder
            .set_bitrate(opus::Bitrate::Bits(PCMU_UPLINK_BITRATE))
            .map_err(|e| AurixError::Codec(format!("pcmu uplink bitrate: {e}")))?;
        let _ = encoder.set_inband_fec(true);
        let _ = encoder.set_packet_loss_perc(10);
        Ok(Self {
            encoder,
            pcm: Vec::with_capacity(480),
            out: vec![0u8; 1275],
        })
    }

    /// Encode one μ-law frame (10/20/40/60 ms) to Opus.
    pub fn transcode(&mut self, ulaw: &[u8]) -> Result<Bytes> {
        if !PCMU_FRAME_SIZES.contains(&ulaw.len()) {
            return Err(AurixError::Codec(format!(
                "pcmu frame of {} bytes (expected one of {:?})",
                ulaw.len(),
                PCMU_FRAME_SIZES
            )));
        }
        self.pcm.clear();
        g711::decode(ulaw, &mut self.pcm);
        let n = self
            .encoder
            .encode(&self.pcm, &mut self.out)
            .map_err(|e| AurixError::Codec(format!("pcmu uplink encode: {e}")))?;
        Ok(Bytes::copy_from_slice(&self.out[..n]))
    }
}

/// Opus → μ-law for the frames of one sender heard by a PCMU receiver.
pub struct PcmuDownlink {
    decoder: opus::Decoder,
    pcm: Vec<i16>,
    out: Vec<u8>,
    pub last_used: Instant,
}

impl std::fmt::Debug for PcmuDownlink {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PcmuDownlink")
            .field("last_used", &self.last_used)
            .finish()
    }
}

impl PcmuDownlink {
    pub fn new() -> Result<Self> {
        let decoder = opus::Decoder::new(PCMU_SAMPLE_RATE, opus::Channels::Mono)
            .map_err(|e| AurixError::Codec(format!("pcmu downlink decoder: {e}")))?;
        Ok(Self {
            decoder,
            pcm: vec![0i16; MAX_DECODE_SAMPLES],
            out: Vec::with_capacity(MAX_DECODE_SAMPLES),
            last_used: Instant::now(),
        })
    }

    /// Decode one Opus frame at 8 kHz and μ-law encode it.
    pub fn transcode(&mut self, opus: &[u8]) -> Result<Bytes> {
        self.last_used = Instant::now();
        let n = self
            .decoder
            .decode(opus, &mut self.pcm, false)
            .map_err(|e| AurixError::Codec(format!("pcmu downlink decode: {e}")))?;
        self.out.clear();
        g711::encode(&self.pcm[..n], &mut self.out);
        Ok(Bytes::copy_from_slice(&self.out))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aurix_common::g711::PCMU_FRAME_SAMPLES;

    fn tone(frames: usize, hz: f32, amp: f32) -> Vec<i16> {
        (0..frames * PCMU_FRAME_SAMPLES)
            .map(|i| {
                let t = i as f32 / PCMU_SAMPLE_RATE as f32;
                ((t * hz * std::f32::consts::TAU).sin() * amp * 32767.0) as i16
            })
            .collect()
    }

    fn rms(pcm: &[i16]) -> f32 {
        (pcm.iter()
            .map(|&s| (s as f32 / 32768.0).powi(2))
            .sum::<f32>()
            / pcm.len() as f32)
            .sqrt()
    }

    #[test]
    fn uplink_rejects_odd_frame_sizes() {
        let mut up = PcmuUplink::new().unwrap();
        assert!(up.transcode(&[0xFF; 159]).is_err());
        assert!(up.transcode(&[]).is_err());
        assert!(up.transcode(&[0xFF; 160]).is_ok());
        assert!(up.transcode(&[0xFF; 480]).is_ok());
    }

    #[test]
    fn pcmu_round_trip_through_opus_keeps_level() {
        let pcm = tone(25, 440.0, 0.4);
        let mut ulaw = Vec::new();
        g711::encode(&pcm, &mut ulaw);
        let mut up = PcmuUplink::new().unwrap();
        let mut down = PcmuDownlink::new().unwrap();
        let mut back = Vec::new();
        for frame in ulaw.chunks(PCMU_FRAME_SAMPLES) {
            let opus = up.transcode(frame).unwrap();
            assert!(!opus.is_empty() && opus.len() < 100, "{} bytes", opus.len());
            let out = down.transcode(&opus).unwrap();
            assert_eq!(out.len(), PCMU_FRAME_SAMPLES);
            g711::decode(&out, &mut back);
        }
        // Skip the encoder's warm-up frames.
        let tail = &back[PCMU_FRAME_SAMPLES * 5..];
        let expected = rms(&pcm[PCMU_FRAME_SAMPLES * 5..]);
        let got = rms(tail);
        assert!(
            (got - expected).abs() < expected * 0.15,
            "rms {got} vs {expected}"
        );
    }

    #[test]
    fn downlink_decodes_fullband_opus_to_narrowband() {
        // A 48 kHz Opus stream (what channels carry) decodes fine at 8 kHz.
        let mut enc =
            opus::Encoder::new(48_000, opus::Channels::Mono, opus::Application::Voip).unwrap();
        let pcm: Vec<i16> = (0..960)
            .map(|i| ((i as f32 / 48_000.0 * 300.0 * std::f32::consts::TAU).sin() * 8000.0) as i16)
            .collect();
        let mut buf = vec![0u8; 1275];
        let n = enc.encode(&pcm, &mut buf).unwrap();
        let mut down = PcmuDownlink::new().unwrap();
        let out = down.transcode(&buf[..n]).unwrap();
        assert_eq!(out.len(), PCMU_FRAME_SAMPLES);
    }

    #[test]
    fn downlink_downmixes_a_stereo_uplink() {
        // Stereo (music) senders reach PCMU receivers, recording, STT and live taps through mono
        // decoders: libopus downmixes, nothing needs to inspect the packet first.
        let mut enc =
            opus::Encoder::new(48_000, opus::Channels::Stereo, opus::Application::Audio).unwrap();
        let pcm: Vec<i16> = (0..960)
            .flat_map(|i| {
                let l =
                    ((i as f32 / 48_000.0 * 300.0 * std::f32::consts::TAU).sin() * 8000.0) as i16;
                [l, 0]
            })
            .collect();
        let mut buf = vec![0u8; 1275];
        let mut down = PcmuDownlink::new().unwrap();
        let mut last = Vec::new();
        for _ in 0..8 {
            let n = enc.encode(&pcm, &mut buf).unwrap();
            assert!(aurix_common::protocol::opus_packet_is_stereo(&buf[..n]));
            let out = down.transcode(&buf[..n]).unwrap();
            assert_eq!(out.len(), PCMU_FRAME_SAMPLES);
            last.clear();
            g711::decode(&out, &mut last);
        }
        // Left-only 8000 → downmix ≈ 4000 peak, so the μ-law frame carries signal, not silence.
        let level = rms(&last);
        assert!(level > 0.05 && level < 0.12, "rms {level}");
    }
}
