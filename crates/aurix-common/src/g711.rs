//! ITU-T G.711 μ-law (PCMU): 8 kHz mono, one byte per sample, 64 kbit/s.
//!
//! The fallback codec of the native AURX transport for clients that cannot run Opus (tiny
//! embedded targets, telephony bridges, debugging with `sox`/Wireshark). A PCMU frame is
//! `PCMU_FRAME_SAMPLES` bytes per 20 ms; the server transcodes between PCMU sessions and the
//! Opus everybody else speaks (see `PacketFlags::Pcmu`).

/// Sample rate of a PCMU stream.
pub const PCMU_SAMPLE_RATE: u32 = 8_000;
/// Samples (= bytes) in one 20 ms PCMU frame.
pub const PCMU_FRAME_SAMPLES: usize = 160;
/// Frame sizes (bytes) the server accepts on the uplink: 10, 20, 40 and 60 ms.
pub const PCMU_FRAME_SIZES: [usize; 4] = [80, 160, 320, 480];

const BIAS: i32 = 0x84;
const CLIP: i32 = 32_635;

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
