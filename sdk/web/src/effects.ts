/**
 * Voice effects for the browser uplink: the same library as the native core
 * (`aurix-client/src/effects.rs`) — filters, formant shift, pitch shift, ring modulator,
 * distortion, tremolo, static, reverb — and the same presets (robot, monster, radio, helium,
 * ghost), run in an `AudioWorkletProcessor` on the microphone path only (injected audio and
 * the downlink are untouched). Every stage preallocates in its constructor and keeps
 * per-channel state (no bleed); `process` neither allocates nor blocks.
 *
 * The DSP classes below are plain (no imports, no module-level constants) so their source can
 * be serialised into the worklet module (`voiceEffectsWorkletSource`), like the E2EE worker.
 * They also run on the main thread in tests.
 */

/** Every built-in stage in one place; `0` / omitted = that stage is off. Mirrors `VoiceEffectParams`. */
export interface VoiceEffectParams {
  /** High-pass corner in Hz (`0`: off). */
  highpassHz?: number;
  /** Low-pass corner in Hz (`0`: off). */
  lowpassHz?: number;
  /** Formant shift in semitones (`±12`): vocal-tract size without changing the pitch. */
  formantSemitones?: number;
  /** Pitch shift in semitones (`±24`), formants follow. */
  pitchSemitones?: number;
  /** Ring-modulator carrier in Hz (`0..2000`). */
  ringModHz?: number;
  /** Saturation drive (`0`: off, `1..20`). */
  distortionDrive?: number;
  /** Tremolo rate in Hz (`0..20`). */
  tremoloHz?: number;
  /** Tremolo depth `0..1`. */
  tremoloDepth?: number;
  /** Static / hiss level `0..1` mixed in while the voice is active. */
  staticLevel?: number;
  /** Reverb wet mix `0..1` (`0`: off). */
  reverbMix?: number;
  /** Reverb room size `0..1`. */
  reverbSize?: number;
  /** Reverb high-frequency damping `0..1`. */
  reverbDamping?: number;
}

/** Fully populated, range-clamped parameters (what the worklet receives). */
export interface ResolvedVoiceEffectParams {
  highpassHz: number;
  lowpassHz: number;
  formantSemitones: number;
  pitchSemitones: number;
  ringModHz: number;
  distortionDrive: number;
  tremoloHz: number;
  tremoloDepth: number;
  staticLevel: number;
  reverbMix: number;
  reverbSize: number;
  reverbDamping: number;
}

export const MAX_PITCH_SEMITONES = 24;
export const MAX_FORMANT_SEMITONES = 12;
export const MAX_RING_MOD_HZ = 2000;
export const MAX_DISTORTION_DRIVE = 20;
export const MAX_TREMOLO_HZ = 20;
export const MIN_FILTER_HZ = 20;
export const MAX_FILTER_HZ = 20_000;

export const VOICE_EFFECTS_BYPASS: Readonly<ResolvedVoiceEffectParams> = Object.freeze({
  highpassHz: 0,
  lowpassHz: 0,
  formantSemitones: 0,
  pitchSemitones: 0,
  ringModHz: 0,
  distortionDrive: 0,
  tremoloHz: 0,
  tremoloDepth: 0,
  staticLevel: 0,
  reverbMix: 0,
  reverbSize: 0,
  reverbDamping: 0,
});

/** Ready-made voices; the same tuning as the native `EffectPreset`s. */
export type VoiceEffectPreset = 'robot' | 'monster' | 'radio' | 'helium' | 'ghost';

export const VOICE_EFFECT_PRESETS: readonly VoiceEffectPreset[] = ['robot', 'monster', 'radio', 'helium', 'ghost'];

export function voiceEffectPreset(name: VoiceEffectPreset | string): ResolvedVoiceEffectParams {
  const off = VOICE_EFFECTS_BYPASS;
  switch (name.trim().toLowerCase()) {
    case 'robot':
      return { ...off, highpassHz: 200, lowpassHz: 4000, ringModHz: 60, distortionDrive: 2 };
    case 'monster':
      return {
        ...off,
        formantSemitones: -5,
        pitchSemitones: -7,
        distortionDrive: 1.5,
        reverbMix: 0.15,
        reverbSize: 0.6,
        reverbDamping: 0.5,
      };
    case 'radio':
      return { ...off, highpassHz: 400, lowpassHz: 3000, distortionDrive: 3, staticLevel: 0.03 };
    case 'helium':
      return { ...off, formantSemitones: 6, pitchSemitones: 6 };
    case 'ghost':
      return {
        ...off,
        lowpassHz: 5000,
        formantSemitones: 2,
        pitchSemitones: -3,
        tremoloHz: 5,
        tremoloDepth: 0.5,
        reverbMix: 0.6,
        reverbSize: 0.9,
        reverbDamping: 0.3,
      };
    default:
      throw new RangeError(`unknown voice effect preset: ${name}`);
  }
}

/** Clamps every field into its documented range (NaN / missing → off), like `VoiceEffectParams::sanitized`. */
export function sanitizeVoiceEffects(p: VoiceEffectParams | undefined): ResolvedVoiceEffectParams {
  const clamp = (v: number | undefined, lo: number, hi: number): number =>
    typeof v === 'number' && Number.isFinite(v) ? Math.min(hi, Math.max(lo, v)) : 0;
  const corner = (v: number | undefined): number =>
    typeof v === 'number' && Number.isFinite(v) && v > 0 ? Math.min(MAX_FILTER_HZ, Math.max(MIN_FILTER_HZ, v)) : 0;
  const drive = p?.distortionDrive;
  return {
    highpassHz: corner(p?.highpassHz),
    lowpassHz: corner(p?.lowpassHz),
    formantSemitones: clamp(p?.formantSemitones, -MAX_FORMANT_SEMITONES, MAX_FORMANT_SEMITONES),
    pitchSemitones: clamp(p?.pitchSemitones, -MAX_PITCH_SEMITONES, MAX_PITCH_SEMITONES),
    ringModHz: clamp(p?.ringModHz, 0, MAX_RING_MOD_HZ),
    distortionDrive:
      typeof drive === 'number' && Number.isFinite(drive) && drive > 0
        ? Math.min(MAX_DISTORTION_DRIVE, Math.max(1, drive))
        : 0,
    tremoloHz: clamp(p?.tremoloHz, 0, MAX_TREMOLO_HZ),
    tremoloDepth: clamp(p?.tremoloDepth, 0, 1),
    staticLevel: clamp(p?.staticLevel, 0, 1),
    reverbMix: clamp(p?.reverbMix, 0, 1),
    reverbSize: clamp(p?.reverbSize, 0, 1),
    reverbDamping: clamp(p?.reverbDamping, 0, 1),
  };
}

export function isVoiceEffectsBypass(p: VoiceEffectParams | undefined): boolean {
  const s = sanitizeVoiceEffects(p);
  return (
    s.highpassHz === 0 &&
    s.lowpassHz === 0 &&
    s.formantSemitones === 0 &&
    s.pitchSemitones === 0 &&
    s.ringModHz === 0 &&
    s.distortionDrive === 0 &&
    (s.tremoloHz === 0 || s.tremoloDepth === 0) &&
    s.staticLevel === 0 &&
    s.reverbMix === 0
  );
}

// ── DSP_UNITS_BEGIN (self-contained: serialised into the worklet) ─────────────────────────

/** One stage: planar `-1..1` PCM, `channels[c]` valid for `n` samples, processed in place. */
export interface VoiceEffectStage {
  process(channels: Float32Array[], n: number): void;
  reset(): void;
}

/** Second-order IIR filter (RBJ cookbook), transposed direct form II, per-channel state. */
export class BiquadStage implements VoiceEffectStage {
  private readonly b0: number;
  private readonly b1: number;
  private readonly b2: number;
  private readonly a1: number;
  private readonly a2: number;
  private readonly z1 = new Float32Array(2);
  private readonly z2 = new Float32Array(2);

  constructor(kind: 'lowpass' | 'highpass', hz: number, q: number, sampleRate: number) {
    hz = Math.min(20000, Math.max(20, Math.min(hz, sampleRate * 0.45)));
    q = Math.max(0.1, q);
    const w0 = (2 * Math.PI * hz) / sampleRate;
    const sin = Math.sin(w0);
    const cos = Math.cos(w0);
    const alpha = sin / (2 * q);
    const a0 = 1 + alpha;
    let b0: number;
    let b1: number;
    let b2: number;
    if (kind === 'lowpass') {
      b0 = (1 - cos) / 2;
      b1 = 1 - cos;
      b2 = (1 - cos) / 2;
    } else {
      b0 = (1 + cos) / 2;
      b1 = -(1 + cos);
      b2 = (1 + cos) / 2;
    }
    this.b0 = b0 / a0;
    this.b1 = b1 / a0;
    this.b2 = b2 / a0;
    this.a1 = (-2 * cos) / a0;
    this.a2 = (1 - alpha) / a0;
  }

  process(channels: Float32Array[], n: number): void {
    for (let c = 0; c < channels.length && c < 2; c++) {
      const buf = channels[c] as Float32Array;
      let z1 = this.z1[c] as number;
      let z2 = this.z2[c] as number;
      for (let i = 0; i < n; i++) {
        const x = buf[i] as number;
        const y = this.b0 * x + z1;
        z1 = this.b1 * x - this.a1 * y + z2;
        z2 = this.b2 * x - this.a2 * y;
        buf[i] = y;
      }
      this.z1[c] = z1;
      this.z2[c] = z2;
    }
  }

  reset(): void {
    this.z1.fill(0);
    this.z2.fill(0);
  }
}

/**
 * Delay-line pitch shifter: two read taps sweep a ring buffer at `ratio` while a raised-cosine
 * crossfade hides the wrap-around (the classic granular voice changer; formants follow).
 */
export class PitchShiftStage implements VoiceEffectStage {
  private readonly ratio: number;
  private readonly grain: number;
  private readonly len: number;
  private readonly ring: Float32Array[];
  private write = 0;
  private phase = 0;

  constructor(semitones: number, sampleRate: number) {
    this.ratio = Math.pow(2, semitones / 12);
    this.grain = Math.max(256, Math.round((1024 * sampleRate) / 48000));
    this.len = this.grain * 2;
    this.ring = [new Float32Array(this.len), new Float32Array(this.len)];
  }

  private static read(ring: Float32Array, pos: number, len: number): number {
    const base = Math.floor(pos);
    const frac = pos - base;
    const i = base % len;
    const a = ring[i] as number;
    const b = ring[(i + 1) % len] as number;
    return a + (b - a) * frac;
  }

  process(channels: Float32Array[], n: number): void {
    const chs = Math.min(channels.length, 2);
    const len = this.len;
    const grain = this.grain;
    for (let i = 0; i < n; i++) {
      for (let c = 0; c < chs; c++) (this.ring[c] as Float32Array)[this.write] = (channels[c] as Float32Array)[i] as number;
      const delayA = this.phase;
      const delayB = (this.phase + grain) % len;
      const wA = 0.5 - 0.5 * Math.cos((2 * Math.PI * delayA) / len);
      const wB = 1 - wA;
      const write = this.write;
      for (let c = 0; c < chs; c++) {
        const ring = this.ring[c] as Float32Array;
        let pa = (write - delayA) % len;
        if (pa < 0) pa += len;
        let pb = (write - delayB) % len;
        if (pb < 0) pb += len;
        (channels[c] as Float32Array)[i] =
          PitchShiftStage.read(ring, pa, len) * wA + PitchShiftStage.read(ring, pb, len) * wB;
      }
      this.write = (this.write + 1) % len;
      this.phase = (this.phase + (1 - this.ratio)) % len;
      if (this.phase < 0) this.phase += len;
    }
  }

  reset(): void {
    for (const r of this.ring) r.fill(0);
    this.write = 0;
    this.phase = 0;
  }
}

/**
 * Pitch-synchronous overlap-add formant shifter: grains two pitch periods long are cut at the
 * tracked pitch marks, resampled by the formant ratio and laid back down at the same marks —
 * the pitch stays, the spectral envelope scales. ~43 ms of latency. Stereo channels are
 * processed independently with the pitch tracked on their mix.
 */
export class FormantShiftStage implements VoiceEffectStage {
  private readonly ratio: number;
  private readonly minPeriod: number;
  private readonly maxPeriod: number;
  private readonly unvoicedPeriod: number;
  private readonly latency: number;
  private readonly ringLen: number;
  private readonly window: number;
  private readonly input: Float32Array[];
  private readonly output: Float32Array[];
  private readonly mono: Float32Array;
  private monoFill = 0;
  private written = 0;
  private nextMark = 0;
  private period: number;
  private channels = 0;

  constructor(semitones: number, sampleRate: number) {
    this.ratio = Math.pow(2, semitones / 12);
    const k = sampleRate / 48000;
    this.minPeriod = Math.max(8, Math.round(60 * k));
    this.maxPeriod = Math.round(600 * k);
    this.unvoicedPeriod = Math.round(200 * k);
    this.latency = Math.round(2048 * k);
    this.ringLen = 1 << Math.ceil(Math.log2(Math.round(8192 * k)));
    this.window = Math.round(960 * k);
    this.input = [new Float32Array(this.ringLen), new Float32Array(this.ringLen)];
    this.output = [new Float32Array(this.ringLen), new Float32Array(this.ringLen)];
    this.mono = new Float32Array(this.window + this.maxPeriod);
    this.period = this.unvoicedPeriod;
  }

  private clearHistory(): void {
    for (const r of this.input) r.fill(0);
    for (const r of this.output) r.fill(0);
    this.mono.fill(0);
    this.monoFill = 0;
    this.written = 0;
    this.nextMark = 0;
    this.period = this.unvoicedPeriod;
  }

  private corr(lag: number, energy: number): number {
    const x = this.mono;
    const end = x.length;
    const w = this.window;
    let dot = 0;
    let pastEnergy = 0;
    for (let i = 0; i < w; i++) {
      const a = x[end - w + i] as number;
      const b = x[end - w - lag + i] as number;
      dot += a * b;
      pastEnergy += b * b;
    }
    return dot / Math.max(1e-9, Math.sqrt(energy * pastEnergy));
  }

  /** Normalised autocorrelation over the last window; shortest strong lag wins (against octave errors). `0` = unvoiced. */
  private trackPitch(): number {
    const x = this.mono;
    const end = x.length;
    const w = this.window;
    let energy = 0;
    for (let i = end - w; i < end; i++) {
      const s = x[i] as number;
      energy += s * s;
    }
    if (energy < 1e-4 * w) return 0;
    let bestLag = 0;
    let bestR = -Infinity;
    for (let lag = this.minPeriod; lag <= this.maxPeriod; lag += 4) {
      const r = this.corr(lag, energy);
      if (r > bestR) {
        bestR = r;
        bestLag = lag;
      }
    }
    if (bestR < 0.5) return 0;
    let chosen = bestLag;
    for (let lag = this.minPeriod; lag < bestLag; lag += 4) {
      if (this.corr(lag, energy) >= bestR * 0.85) {
        chosen = lag;
        break;
      }
    }
    const lo = Math.max(this.minPeriod, chosen - 3);
    const hi = Math.min(this.maxPeriod, chosen + 3);
    let refined = chosen;
    let refinedR = -Infinity;
    for (let lag = lo; lag <= hi; lag++) {
      const r = this.corr(lag, energy);
      if (r > refinedR) {
        refinedR = r;
        refined = lag;
      }
    }
    return refined;
  }

  private static read(ring: Float32Array, pos: number, len: number): number {
    const base = Math.floor(pos);
    const frac = pos - base;
    const i = ((base % len) + len) % len;
    const a = ring[i] as number;
    const b = ring[(i + 1) % len] as number;
    return a + (b - a) * frac;
  }

  private synthesize(mark: number, period: number): void {
    const len = this.ringLen;
    for (let c = 0; c < this.channels; c++) {
      const input = this.input[c] as Float32Array;
      const output = this.output[c] as Float32Array;
      for (let k = -period; k < period; k++) {
        const w = 0.5 + 0.5 * Math.cos((Math.PI * k) / period);
        const sample = FormantShiftStage.read(input, mark + k * this.ratio, len);
        const dst = (((mark + k) % len) + len) % len;
        output[dst] = (output[dst] as number) + w * sample;
      }
    }
  }

  process(channels: Float32Array[], n: number): void {
    const chs = Math.min(channels.length, 2);
    if (this.channels !== chs) {
      this.channels = chs;
      this.clearHistory();
    }
    const len = this.ringLen;
    const mono = this.mono;
    const histLen = mono.length;
    // Ingest into the per-channel rings and the mono pitch history (a sliding window).
    const take = Math.min(n, histLen);
    if (take >= histLen) mono.fill(0);
    else mono.copyWithin(0, take);
    const monoFrom = histLen - take;
    const skip = n - take;
    for (let i = 0; i < n; i++) {
      const idx = (this.written + i) % len;
      let sum = 0;
      for (let c = 0; c < chs; c++) {
        const s = (channels[c] as Float32Array)[i] as number;
        (this.input[c] as Float32Array)[idx] = s;
        sum += s;
      }
      if (i >= skip) mono[monoFrom + i - skip] = sum / chs;
    }
    this.written += n;
    this.monoFill += n;
    // Pitch tracking once per analysis window (960 samples at 48 kHz), as the native core does per frame.
    if (this.monoFill >= this.window) {
      this.monoFill = 0;
      const p = this.trackPitch();
      this.period = p > 0 ? p : this.unvoicedPeriod;
    }
    const reach = Math.ceil(this.period * Math.max(1, this.ratio));
    while (this.nextMark + reach <= this.written) {
      this.synthesize(this.nextMark, this.period);
      this.nextMark += this.period;
    }
    const end = this.written - this.latency;
    for (let i = 0; i < n; i++) {
      const t = end - n + i;
      for (let c = 0; c < chs; c++) {
        const out = channels[c] as Float32Array;
        if (t < 0) {
          out[i] = 0;
          continue;
        }
        const idx = t % len;
        const output = this.output[c] as Float32Array;
        out[i] = output[idx] as number;
        output[idx] = 0;
      }
    }
  }

  reset(): void {
    this.clearHistory();
  }
}

/** Multiplies the voice by a sine carrier — the robot / Dalek effect. */
export class RingModulatorStage implements VoiceEffectStage {
  private readonly step: number;
  private phase = 0;

  constructor(carrierHz: number, sampleRate: number) {
    this.step = (2 * Math.PI * carrierHz) / sampleRate;
  }

  process(channels: Float32Array[], n: number): void {
    const chs = Math.min(channels.length, 2);
    for (let i = 0; i < n; i++) {
      const carrier = Math.sin(this.phase);
      for (let c = 0; c < chs; c++) {
        const buf = channels[c] as Float32Array;
        buf[i] = (buf[i] as number) * carrier;
      }
      this.phase += this.step;
      if (this.phase >= 2 * Math.PI) this.phase -= 2 * Math.PI;
    }
  }

  reset(): void {
    this.phase = 0;
  }
}

/** Soft saturation: tanh-shaped waveshaper normalised so full scale stays full scale. */
export class DistortionStage implements VoiceEffectStage {
  private readonly drive: number;
  private readonly norm: number;

  constructor(drive: number) {
    this.drive = Math.min(20, Math.max(1, drive));
    this.norm = 1 / DistortionStage.shape(this.drive);
  }

  private static shape(x: number): number {
    if (x > 3) x = 3;
    else if (x < -3) x = -3;
    const x2 = x * x;
    return (x * (27 + x2)) / (27 + 9 * x2);
  }

  process(channels: Float32Array[], n: number): void {
    for (let c = 0; c < channels.length && c < 2; c++) {
      const buf = channels[c] as Float32Array;
      for (let i = 0; i < n; i++) buf[i] = DistortionStage.shape((buf[i] as number) * this.drive) * this.norm;
    }
  }

  reset(): void {}
}

/** Amplitude modulation by a slow sine: level dips by `depth` at `rateHz`. */
export class TremoloStage implements VoiceEffectStage {
  private readonly step: number;
  private readonly depth: number;
  private phase = 0;

  constructor(rateHz: number, depth: number, sampleRate: number) {
    this.step = (2 * Math.PI * rateHz) / sampleRate;
    this.depth = Math.min(1, Math.max(0, depth));
  }

  process(channels: Float32Array[], n: number): void {
    const chs = Math.min(channels.length, 2);
    for (let i = 0; i < n; i++) {
      const gain = 1 - this.depth * (0.5 - 0.5 * Math.cos(this.phase));
      for (let c = 0; c < chs; c++) {
        const buf = channels[c] as Float32Array;
        buf[i] = (buf[i] as number) * gain;
      }
      this.phase += this.step;
      if (this.phase >= 2 * Math.PI) this.phase -= 2 * Math.PI;
    }
  }

  reset(): void {
    this.phase = 0;
  }
}

/** White-noise hiss gated by the voice level (silence stays silent, so it does not trip the VAD). */
export class StaticStage implements VoiceEffectStage {
  private readonly level: number;
  private seed = 0x9e3779b9;
  private envelope = 0;

  constructor(level: number) {
    this.level = Math.min(1, Math.max(0, level));
  }

  private noise(): number {
    this.seed = (Math.imul(this.seed, 1664525) + 1013904223) >>> 0;
    return ((this.seed >>> 8) / 16777216) * 2 - 1;
  }

  process(channels: Float32Array[], n: number): void {
    const chs = Math.min(channels.length, 2);
    if (n === 0 || chs === 0) return;
    let energy = 0;
    for (let c = 0; c < chs; c++) {
      const buf = channels[c] as Float32Array;
      for (let i = 0; i < n; i++) energy += (buf[i] as number) * (buf[i] as number);
    }
    const rms = Math.sqrt(energy / (n * chs));
    // The native stage releases 0.8× per 20 ms frame; scale the decay to this block length.
    const decay = Math.pow(0.8, n / 960);
    this.envelope = Math.max(rms, this.envelope * decay);
    const gate = Math.min(1, this.envelope * 20);
    if (gate <= 0) return;
    const gain = this.level * gate;
    for (let i = 0; i < n; i++) {
      const v = this.noise() * gain;
      for (let c = 0; c < chs; c++) {
        const buf = channels[c] as Float32Array;
        buf[i] = (buf[i] as number) + v;
      }
    }
  }

  reset(): void {
    this.envelope = 0;
  }
}

/**
 * Schroeder / Freeverb-style reverb: four damped feedback combs into two series all-passes,
 * mixed with the dry voice; per-channel networks (right one detuned for width).
 */
export class ReverbStage implements VoiceEffectStage {
  private readonly feedback: number;
  private readonly damp: number;
  private readonly wet: number;
  private readonly dry: number;
  private readonly combs: Float32Array[][];
  private readonly combIdx: Int32Array[];
  private readonly combStore: Float32Array[];
  private readonly allpasses: Float32Array[][];
  private readonly apIdx: Int32Array[];

  constructor(size: number, damping: number, mix: number, sampleRate: number) {
    size = Math.min(1, Math.max(0, size));
    damping = Math.min(1, Math.max(0, damping));
    mix = Math.min(1, Math.max(0, mix));
    this.feedback = 0.7 + 0.28 * size;
    this.damp = damping * 0.4;
    this.wet = mix * 0.1;
    this.dry = 1 - mix * 0.5;
    const k = sampleRate / 48000;
    const combDelays = [1215, 1293, 1390, 1476];
    const apDelays = [605, 480];
    const spreads = [0, 23];
    this.combs = spreads.map((spread) => combDelays.map((d) => new Float32Array(Math.max(1, Math.round((d + spread) * k)))));
    this.combIdx = spreads.map(() => new Int32Array(combDelays.length));
    this.combStore = spreads.map(() => new Float32Array(combDelays.length));
    this.allpasses = spreads.map((spread) => apDelays.map((d) => new Float32Array(Math.max(1, Math.round((d + spread) * k)))));
    this.apIdx = spreads.map(() => new Int32Array(apDelays.length));
  }

  private tick(c: number, input: number): number {
    let out = 0;
    const combs = this.combs[c] as Float32Array[];
    const idx = this.combIdx[c] as Int32Array;
    const store = this.combStore[c] as Float32Array;
    for (let k = 0; k < combs.length; k++) {
      const buf = combs[k] as Float32Array;
      const i = idx[k] as number;
      const y = buf[i] as number;
      const s = y * (1 - this.damp) + (store[k] as number) * this.damp;
      store[k] = s;
      buf[i] = input + s * this.feedback;
      idx[k] = (i + 1) % buf.length;
      out += y;
    }
    const aps = this.allpasses[c] as Float32Array[];
    const apIdx = this.apIdx[c] as Int32Array;
    for (let k = 0; k < aps.length; k++) {
      const buf = aps[k] as Float32Array;
      const i = apIdx[k] as number;
      const buffered = buf[i] as number;
      buf[i] = out + buffered * 0.5;
      apIdx[k] = (i + 1) % buf.length;
      out = buffered - out;
    }
    return out;
  }

  process(channels: Float32Array[], n: number): void {
    const chs = Math.min(channels.length, 2);
    for (let c = 0; c < chs; c++) {
      const buf = channels[c] as Float32Array;
      for (let i = 0; i < n; i++) {
        const x = buf[i] as number;
        buf[i] = x * this.dry + this.tick(c, x) * this.wet;
      }
    }
  }

  reset(): void {
    for (let c = 0; c < 2; c++) {
      for (const b of this.combs[c] as Float32Array[]) b.fill(0);
      for (const b of this.allpasses[c] as Float32Array[]) b.fill(0);
      (this.combIdx[c] as Int32Array).fill(0);
      (this.combStore[c] as Float32Array).fill(0);
      (this.apIdx[c] as Int32Array).fill(0);
    }
  }
}

/**
 * Ordered stages built from resolved parameters in the native order (filters, formant, pitch,
 * ring modulator, distortion, tremolo, static, reverb); empty = bypass. Construction
 * allocates; `process` does not.
 */
export class VoiceEffectChain {
  readonly stages: VoiceEffectStage[] = [];

  constructor(p: ResolvedVoiceEffectParams, sampleRate: number) {
    if (p.highpassHz > 0) this.stages.push(new BiquadStage('highpass', p.highpassHz, 0.707, sampleRate));
    if (p.lowpassHz > 0) this.stages.push(new BiquadStage('lowpass', p.lowpassHz, 0.707, sampleRate));
    if (p.formantSemitones !== 0) this.stages.push(new FormantShiftStage(p.formantSemitones, sampleRate));
    if (p.pitchSemitones !== 0) this.stages.push(new PitchShiftStage(p.pitchSemitones, sampleRate));
    if (p.ringModHz > 0) this.stages.push(new RingModulatorStage(p.ringModHz, sampleRate));
    if (p.distortionDrive > 0) this.stages.push(new DistortionStage(p.distortionDrive));
    if (p.tremoloHz > 0 && p.tremoloDepth > 0) this.stages.push(new TremoloStage(p.tremoloHz, p.tremoloDepth, sampleRate));
    if (p.staticLevel > 0) this.stages.push(new StaticStage(p.staticLevel));
    if (p.reverbMix > 0) this.stages.push(new ReverbStage(p.reverbSize, p.reverbDamping, p.reverbMix, sampleRate));
  }

  get isBypass(): boolean {
    return this.stages.length === 0;
  }

  process(channels: Float32Array[], n: number): void {
    if (this.stages.length === 0) return;
    for (const s of this.stages) s.process(channels, n);
    for (let c = 0; c < channels.length && c < 2; c++) {
      const buf = channels[c] as Float32Array;
      for (let i = 0; i < n; i++) {
        const v = buf[i] as number;
        buf[i] = v > 1 ? 1 : v < -1 ? -1 : v;
      }
    }
  }

  reset(): void {
    for (const s of this.stages) s.reset();
  }
}

/** Messages the main thread posts to the worklet processor. */
export type VoiceEffectsWorkletMessage = { type: 'params'; params: ResolvedVoiceEffectParams } | { type: 'reset' };

interface WorkletProcessorLike {
  port: { onmessage: ((ev: { data: VoiceEffectsWorkletMessage }) => void) | null };
}

interface WorkletScopeLike {
  sampleRate: number;
  AudioWorkletProcessor: new () => WorkletProcessorLike;
  registerProcessor(name: string, ctor: unknown): void;
}

/**
 * Body of the worklet module: registers `aurix-voice-effects`, a processor that copies its
 * input to its output through a {@link VoiceEffectChain}. The chain is rebuilt on the
 * `params` message (allocation happens there, between render quanta, not per sample).
 */
export function voiceEffectsWorkletMain(scope: WorkletScopeLike): void {
  const Base = scope.AudioWorkletProcessor;
  class AurixVoiceEffectsProcessor extends Base {
    private chain: VoiceEffectChain | undefined;
    constructor() {
      super();
      this.port.onmessage = (ev) => {
        const m = ev.data;
        if (m.type === 'params') {
          const chain = new VoiceEffectChain(m.params, scope.sampleRate);
          this.chain = chain.isBypass ? undefined : chain;
        } else if (m.type === 'reset') {
          this.chain?.reset();
        }
      };
    }
    process(inputs: Float32Array[][], outputs: Float32Array[][]): boolean {
      const input = inputs[0];
      const output = outputs[0];
      if (!input || !output || input.length === 0) return true;
      const chs = Math.min(input.length, output.length);
      let n = 0;
      for (let c = 0; c < chs; c++) {
        const src = input[c] as Float32Array;
        const dst = output[c] as Float32Array;
        dst.set(src.subarray(0, dst.length));
        n = dst.length;
      }
      // A mono microphone on a stereo output: duplicate rather than leave the right side silent.
      for (let c = chs; c < output.length; c++) (output[c] as Float32Array).set(output[0] as Float32Array);
      if (this.chain) this.chain.process(output.slice(0, chs), n);
      return true;
    }
  }
  scope.registerProcessor('aurix-voice-effects', AurixVoiceEffectsProcessor);
}

// ── DSP_UNITS_END ────────────────────────────────────────────────────────────────────────

/** Registered processor name. */
export const VOICE_EFFECTS_PROCESSOR = 'aurix-voice-effects';

/** JavaScript source of the worklet module (self-contained; served from a `blob:` URL). */
export function voiceEffectsWorkletSource(): string {
  const units = [
    BiquadStage,
    PitchShiftStage,
    FormantShiftStage,
    RingModulatorStage,
    DistortionStage,
    TremoloStage,
    StaticStage,
    ReverbStage,
    VoiceEffectChain,
  ];
  return `'use strict';\n${units.map((u) => u.toString()).join('\n')}\n(${voiceEffectsWorkletMain.toString()})(globalThis);\n`;
}

/** Whether this browser can run the effects worklet. */
export function supportsVoiceEffects(): boolean {
  return (
    typeof AudioContext !== 'undefined' &&
    typeof AudioWorkletNode !== 'undefined' &&
    typeof Blob !== 'undefined' &&
    typeof URL !== 'undefined' &&
    typeof URL.createObjectURL === 'function'
  );
}

/** Loads the worklet module into `ctx` once (subsequent calls on the same context are no-ops). */
const loadedContexts = new WeakSet<BaseAudioContext>();
export async function loadVoiceEffectsWorklet(ctx: BaseAudioContext): Promise<void> {
  if (loadedContexts.has(ctx)) return;
  if (!supportsVoiceEffects()) throw new Error('AudioWorklet is unavailable: voice effects are not supported here');
  const url = URL.createObjectURL(new Blob([voiceEffectsWorkletSource()], { type: 'text/javascript' }));
  try {
    await ctx.audioWorklet.addModule(url);
  } finally {
    URL.revokeObjectURL(url);
  }
  loadedContexts.add(ctx);
}

/** Creates the effects node on `ctx` (module loaded) and pushes `params` to it. */
export function createVoiceEffectsNode(ctx: BaseAudioContext, params: ResolvedVoiceEffectParams): AudioWorkletNode {
  const node = new AudioWorkletNode(ctx, VOICE_EFFECTS_PROCESSOR, {
    numberOfInputs: 1,
    numberOfOutputs: 1,
    channelCount: 2,
    channelCountMode: 'clamped-max',
  });
  const msg: VoiceEffectsWorkletMessage = { type: 'params', params };
  node.port.postMessage(msg);
  return node;
}
