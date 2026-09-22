//! ITU-T G.711 μ-law (PCMU) and A-law (PCMA): 8 kHz mono, one byte per sample, 64 kbit/s.
//!
//! The fallback codec of the native AURX transport for clients that cannot run Opus (tiny
//! embedded targets, telephony bridges, debugging with `sox`/Wireshark). A G.711 frame is
//! `PCMU_FRAME_SAMPLES` bytes per 20 ms in either law; the server transcodes between G.711
//! sessions and the Opus everybody else speaks (see `PacketFlags::Pcmu` / `PacketFlags::Pcma`),
//! except for end-to-end encrypted frames, which it relays as they are.

/// Sample rate of a G.711 stream (either law).
pub const PCMU_SAMPLE_RATE: u32 = 8_000;
/// Samples (= bytes) in one 20 ms G.711 frame.
pub const PCMU_FRAME_SAMPLES: usize = 160;
/// Frame sizes (bytes) the server accepts on the uplink: 10, 20, 40 and 60 ms.
pub const PCMU_FRAME_SIZES: [usize; 4] = [80, 160, 320, 480];

const BIAS: i32 = 0x84;
const CLIP: i32 = 32_635;
const ALAW_MASK: u8 = 0x55;
const ALAW_SEG_END: [i32; 8] = [0x1F, 0x3F, 0x7F, 0xFF, 0x1FF, 0x3FF, 0x7FF, 0xFFF];

/// Companding law of a G.711 stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Law {
    /// μ-law (PCMU, RTP payload type 0).
    Mu,
    /// A-law (PCMA, RTP payload type 8).
    A,
}

impl Law {
    /// Encode one 16-bit linear sample.
    pub fn encode_sample(self, sample: i16) -> u8 {
        match self {
            Law::Mu => ulaw_encode(sample),
            Law::A => alaw_encode(sample),
        }
    }

    /// Decode one companded byte to a 16-bit linear sample.
    pub fn decode_sample(self, byte: u8) -> i16 {
        match self {
            Law::Mu => ulaw_decode(byte),
            Law::A => alaw_decode(byte),
        }
    }

    /// Encode a buffer of linear samples.
    pub fn encode(self, pcm: &[i16], out: &mut Vec<u8>) {
        out.extend(pcm.iter().map(|&s| self.encode_sample(s)));
    }

    /// Decode a buffer of companded bytes.
    pub fn decode(self, data: &[u8], out: &mut Vec<i16>) {
        out.extend(data.iter().map(|&b| self.decode_sample(b)));
    }

    /// Encode `f32` samples in `-1.0..=1.0`.
    pub fn encode_f32(self, pcm: &[f32], out: &mut Vec<u8>) {
        out.extend(
            pcm.iter()
                .map(|&s| self.encode_sample((s.clamp(-1.0, 1.0) * 32767.0).round() as i16)),
        );
    }

    /// Decode to `f32` samples in `-1.0..=1.0`.
    pub fn decode_f32(self, data: &[u8], out: &mut Vec<f32>) {
        out.extend(data.iter().map(|&b| self.decode_sample(b) as f32 / 32768.0));
    }

    /// The byte a silent sample encodes to.
    pub fn silence(self) -> u8 {
        self.encode_sample(0)
    }
}

/// Encode one 16-bit linear sample.
pub fn ulaw_encode(sample: i16) -> u8 {
    let mut pcm = sample as i32;
    let sign: u8 = if pcm < 0 {
        pcm = -pcm;
        0x80
    } else {
        0x00
    };
    if pcm > CLIP {
        pcm = CLIP;
    }
    pcm += BIAS;
    // Position of the highest set bit (bits 7..=14), 0 when the sample is below 2^8.
    let exponent = (31 - (pcm as u32).leading_zeros()).saturating_sub(7) as i32;
    let mantissa = ((pcm >> (exponent + 3)) & 0x0F) as u8;
    !(sign | ((exponent as u8) << 4) | mantissa)
}

/// Decode one μ-law byte to a 16-bit linear sample.
pub fn ulaw_decode(byte: u8) -> i16 {
    let u = !byte as i32;
    let sign = u & 0x80;
    let exponent = (u >> 4) & 0x07;
    let mantissa = u & 0x0F;
    let magnitude = (((mantissa << 3) + BIAS) << exponent) - BIAS;
    (if sign != 0 { -magnitude } else { magnitude }) as i16
}

/// Encode one 16-bit linear sample to A-law (G.711 with the even-bit inversion applied).
pub fn alaw_encode(sample: i16) -> u8 {
    // A-law works on 13-bit magnitudes.
    let mut pcm = (sample as i32) >> 3;
    let mask: u8 = if pcm >= 0 {
        0xD5
    } else {
        pcm = -pcm - 1;
        ALAW_MASK
    };
    let seg = ALAW_SEG_END.iter().position(|&end| pcm <= end);
    match seg {
        None => 0x7F ^ mask,
        Some(seg) => {
            let shift = if seg < 2 { 1 } else { seg };
            let aval = ((seg as u8) << 4) | ((pcm >> shift) & 0x0F) as u8;
            aval ^ mask
        }
    }
}

/// Decode one A-law byte to a 16-bit linear sample.
pub fn alaw_decode(byte: u8) -> i16 {
    let a = (byte ^ ALAW_MASK) as i32;
    let mut t = (a & 0x0F) << 4;
    let seg = (a & 0x70) >> 4;
    match seg {
        0 => t += 8,
        1 => t += 0x108,
        _ => {
            t += 0x108;
            t <<= seg - 1;
        }
    }
    (if a & 0x80 != 0 { t } else { -t }) as i16
}

/// Encode a buffer of linear samples.
pub fn encode(pcm: &[i16], out: &mut Vec<u8>) {
    out.extend(pcm.iter().map(|&s| ulaw_encode(s)));
}

/// Decode a buffer of μ-law bytes.
pub fn decode(ulaw: &[u8], out: &mut Vec<i16>) {
    out.extend(ulaw.iter().map(|&b| ulaw_decode(b)));
}

/// Encode `f32` samples in `-1.0..=1.0`.
pub fn encode_f32(pcm: &[f32], out: &mut Vec<u8>) {
    out.extend(
        pcm.iter()
            .map(|&s| ulaw_encode((s.clamp(-1.0, 1.0) * 32767.0).round() as i16)),
    );
}

/// Decode to `f32` samples in `-1.0..=1.0`.
pub fn decode_f32(ulaw: &[u8], out: &mut Vec<f32>) {
    out.extend(ulaw.iter().map(|&b| ulaw_decode(b) as f32 / 32768.0));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_reference_values() {
        // Well-known G.711 μ-law code points.
        assert_eq!(ulaw_encode(0), 0xFF);
        assert_eq!(ulaw_encode(-1), 0x7F);
        assert_eq!(ulaw_encode(32767), 0x80);
        assert_eq!(ulaw_encode(-32768), 0x00);
        assert_eq!(ulaw_encode(8), 0xFE);
        assert_eq!(ulaw_encode(1000), 0xCE);
        assert_eq!(ulaw_decode(0xFF), 0);
        assert_eq!(ulaw_decode(0x80), 32_124);
        assert_eq!(ulaw_decode(0x00), -32_124);
        assert_eq!(ulaw_decode(0xCE), 988);
    }

    #[test]
    fn alaw_matches_reference_values() {
        // Well-known G.711 A-law code points (after the even-bit inversion).
        assert_eq!(alaw_encode(0), 0xD5);
        assert_eq!(alaw_encode(-1), 0x55);
        assert_eq!(alaw_encode(32767), 0xAA);
        assert_eq!(alaw_encode(-32768), 0x2A);
        assert_eq!(alaw_encode(1000), 0xFA);
        assert_eq!(alaw_decode(0xD5), 8);
        assert_eq!(alaw_decode(0x55), -8);
        assert_eq!(alaw_decode(0xAA), 32_256);
        assert_eq!(alaw_decode(0x2A), -32_256);
        assert_eq!(alaw_decode(0xFA), 1008);
        assert_eq!(Law::A.silence(), 0xD5);
        assert_eq!(Law::Mu.silence(), 0xFF);
    }

    #[test]
    fn alaw_decode_encode_is_identity_on_code_points() {
        for b in 0..=255u8 {
            let pcm = alaw_decode(b);
            assert_eq!(alaw_encode(pcm), b, "code point {b:#x} → {pcm}");
        }
    }

    #[test]
    fn alaw_round_trip_error_is_bounded_by_segment_step() {
        for s in (-32_768..=32_767).step_by(7) {
            let back = alaw_decode(alaw_encode(s as i16)) as i32;
            let err = (back - s).abs();
            // 13-bit input (step 8 near zero), then half a segment step (1/32 of the magnitude).
            assert!(err <= s.abs() / 32 + 8, "{s} → {back}");
        }
    }

    #[test]
    fn laws_share_the_f32_helpers() {
        let tone: Vec<f32> = (0..PCMU_FRAME_SAMPLES)
            .map(|i| (i as f32 / 20.0 * std::f32::consts::TAU).sin() * 0.5)
            .collect();
        let rms = |p: &[f32]| (p.iter().map(|s| s * s).sum::<f32>() / p.len() as f32).sqrt();
        for law in [Law::Mu, Law::A] {
            let mut coded = Vec::new();
            law.encode_f32(&tone, &mut coded);
            assert_eq!(coded.len(), PCMU_FRAME_SAMPLES);
            let mut back = Vec::new();
            law.decode_f32(&coded, &mut back);
            assert!((rms(&back) - rms(&tone)).abs() < 0.01, "{law:?}");
        }
        // The two laws are different codes for the same samples.
        let mut mu = Vec::new();
        let mut a = Vec::new();
        Law::Mu.encode_f32(&tone, &mut mu);
        Law::A.encode_f32(&tone, &mut a);
        assert_ne!(mu, a);
    }

    #[test]
    fn decode_encode_is_identity_on_code_points() {
        // 0x7F is "negative zero" and decodes to the same sample as 0xFF.
        for b in (0..=255u8).filter(|&b| b != 0x7F) {
            let pcm = ulaw_decode(b);
            assert_eq!(ulaw_encode(pcm), b, "code point {b:#x} → {pcm}");
        }
    }

    #[test]
    fn round_trip_error_is_bounded_by_segment_step() {
        let mut worst = 0.0f32;
        for s in (-32_768..=32_767).step_by(7) {
            let back = ulaw_decode(ulaw_encode(s as i16)) as i32;
            let err = (back - s).abs() as f32;
            // Quantisation step doubles every segment: |err| ≤ 2^(exp+3) / 2 + bias effects.
            let mag = (s.abs() + BIAS).max(1) as f32;
            worst = worst.max(err / mag);
        }
        assert!(worst < 0.07, "relative error {worst}");
    }

    #[test]
    fn f32_helpers_keep_level() {
        let tone: Vec<f32> = (0..PCMU_FRAME_SAMPLES)
            .map(|i| (i as f32 / 20.0 * std::f32::consts::TAU).sin() * 0.5)
            .collect();
        let mut ulaw = Vec::new();
        encode_f32(&tone, &mut ulaw);
        assert_eq!(ulaw.len(), PCMU_FRAME_SAMPLES);
        let mut back = Vec::new();
        decode_f32(&ulaw, &mut back);
        let rms = |p: &[f32]| (p.iter().map(|s| s * s).sum::<f32>() / p.len() as f32).sqrt();
        assert!((rms(&back) - rms(&tone)).abs() < 0.01);
    }
}
