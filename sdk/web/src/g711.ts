/**
 * G.711 (μ-law / A-law) decoding for AURX frames that reach a browser as G.711: the sealed
 * frames of a native participant who negotiated the PCMU / PCMA fallback in an end-to-end
 * encrypted channel (the node cannot transcode what it cannot open). Plaintext channels never
 * deliver G.711 to a browser — the node re-encodes to Opus — so encoding is not needed here.
 *
 * Mirrors `aurix_common::g711`.
 */

export type G711Law = 'pcmu' | 'pcma';

/** Sample rate of G.711 audio. */
export const G711_SAMPLE_RATE = 8000;
/** Frame sizes (bytes = samples) a G.711 AURX frame may have: 10 / 20 / 40 / 60 ms. */
export const G711_FRAME_SIZES: readonly number[] = [80, 160, 320, 480];

const ULAW_BIAS = 0x84;

function ulawDecodeSample(byte: number): number {
  const u = ~byte & 0xff;
  let t = ((u & 0x0f) << 3) + ULAW_BIAS;
  t <<= (u & 0x70) >> 4;
  return u & 0x80 ? ULAW_BIAS - t : t - ULAW_BIAS;
}

function alawDecodeSample(byte: number): number {
  const a = (byte ^ 0x55) & 0xff;
  let t = (a & 0x0f) << 4;
  const seg = (a & 0x70) >> 4;
  if (seg === 0) t += 8;
  else if (seg === 1) t += 0x108;
  else {
    t += 0x108;
    t <<= seg - 1;
  }
  return a & 0x80 ? t : -t;
}

function table(decode: (byte: number) => number): Float32Array {
  const out = new Float32Array(256);
  for (let i = 0; i < 256; i++) out[i] = decode(i) / 32768;
  return out;
}

const ULAW_TABLE = table(ulawDecodeSample);
const ALAW_TABLE = table(alawDecodeSample);

/** Decoded 8 kHz samples of a G.711 frame in `[-1, 1]`; `undefined` for an unsupported size. */
export function decodeG711(law: G711Law, frame: Uint8Array): Float32Array | undefined {
  if (!G711_FRAME_SIZES.includes(frame.length)) return undefined;
  const lut = law === 'pcmu' ? ULAW_TABLE : ALAW_TABLE;
  const out = new Float32Array(frame.length);
  for (let i = 0; i < frame.length; i++) out[i] = lut[frame[i]!]!;
  return out;
}

/**
 * Linear-interpolation upsampler from 8 kHz to `ratio × 8 kHz` that stays continuous across
 * frames (one per downlink stream). Telephone-band audio has nothing above 4 kHz, so the
 * interpolation images sit where the signal is silent anyway.
 */
export class G711Upsampler {
  private last = 0;

  constructor(readonly ratio: number) {}

  process(input: Float32Array): Float32Array {
    const ratio = this.ratio;
    const out = new Float32Array(input.length * ratio);
    let prev = this.last;
    let n = 0;
    for (let i = 0; i < input.length; i++) {
      const cur = input[i]!;
      for (let k = 1; k <= ratio; k++) out[n++] = prev + ((cur - prev) * k) / ratio;
      prev = cur;
    }
    this.last = prev;
    return out;
  }

  reset(): void {
    this.last = 0;
  }
}
