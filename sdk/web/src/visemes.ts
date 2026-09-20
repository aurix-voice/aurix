/**
 * Lip-sync from audio, locally — the browser port of `aurix-client/src/visemes.rs`. Every
 * 20 ms of a participant's decoded track (or of our own processed microphone) is reduced to a
 * compact mouth state: a weight per {@link Viseme} bucket plus openness, level and confidence.
 * The analysis runs on the receiver over audio it plays anyway: nothing is sent to the server,
 * no phoneme data crosses the wire, and E2EE audio works because it is decrypted here.
 *
 * Runs in an `AudioWorkletProcessor` tapped off each track (`visemeWorkletSource`); the
 * analyser class is plain (no imports / module constants) so it can be serialised into the
 * worklet and unit-tested on the main thread.
 */

/** Mouth-shape buckets, ordered as in {@link VisemeFrame.weights} (same order as the native enum). */
export type Viseme = 'sil' | 'PP' | 'FF' | 'SS' | 'aa' | 'E' | 'ih' | 'oh' | 'ou';

export const VISEMES: readonly Viseme[] = ['sil', 'PP', 'FF', 'SS', 'aa', 'E', 'ih', 'oh', 'ou'];
export const VISEME_COUNT = 9;

/** Mouth state derived from the latest analysed frame. */
export interface VisemeFrame {
  /** Smoothed weight per bucket (index = position in {@link VISEMES}), summing to ~1. */
  weights: number[];
  /** The heaviest bucket. */
  dominant: Viseme;
  /** Jaw openness `0..1`. */
  mouthOpen: number;
  /** RMS level of the frame, `0..1`. */
  energy: number;
  /** How clear-cut the classification is, `0..1`. */
  confidence: number;
  /** Frames analysed so far; unchanged between two reads = no new audio. */
  sequence: number;
}

export function silentVisemeFrame(sequence = 0): VisemeFrame {
  const weights = new Array<number>(VISEME_COUNT).fill(0);
  weights[0] = 1;
  return { weights, dominant: 'sil', mouthOpen: 0, energy: 0, confidence: 1, sequence };
}

// ── DSP_UNITS_BEGIN (self-contained: serialised into the worklet) ─────────────────────────

/**
 * Per-stream lip-sync analyser: feed 20 ms mono frames (any sample rate; the frame length
 * is `sampleRate / 50`), read `frame` at will. Allocates only in the constructor.
 */
export class VisemeAnalyzer {
  readonly frameSamples: number;
  private readonly fftSize: number;
  private readonly bins: number;
  private readonly binHz: number;
  private readonly window: Float32Array;
  private readonly input: Float32Array;
  private readonly re: Float32Array;
  private readonly im: Float32Array;
  private readonly cosTable: Float32Array;
  private readonly sinTable: Float32Array;
  private readonly rev: Uint32Array;
  private readonly power: Float32Array;
  private readonly deemphasis: Float32Array;
  private readonly smooth: Float32Array;
  private readonly target = new Float32Array(9);
  private readonly weights = new Float32Array(9);
  private readonly sampleRate: number;
  private noiseFloor = 0.002;
  private dominant = 0;
  private mouthOpen = 0;
  private energy = 0;
  private confidence = 1;
  private sequence = 0;

  constructor(sampleRate = 48000) {
    this.sampleRate = sampleRate;
    this.frameSamples = Math.max(64, Math.round(sampleRate / 50));
    let fft = 1024;
    while (fft < this.frameSamples) fft *= 2;
    this.fftSize = fft;
    this.bins = fft / 2 + 1;
    this.binHz = sampleRate / fft;
    this.window = new Float32Array(this.frameSamples);
    for (let n = 0; n < this.frameSamples; n++) {
      this.window[n] = 0.5 - 0.5 * Math.cos((2 * Math.PI * n) / this.frameSamples);
    }
    this.input = new Float32Array(fft);
    this.re = new Float32Array(fft);
    this.im = new Float32Array(fft);
    this.cosTable = new Float32Array(fft / 2);
    this.sinTable = new Float32Array(fft / 2);
    for (let i = 0; i < fft / 2; i++) {
      this.cosTable[i] = Math.cos((2 * Math.PI * i) / fft);
      this.sinTable[i] = Math.sin((2 * Math.PI * i) / fft);
    }
    this.rev = new Uint32Array(fft);
    const bits = Math.log2(fft);
    for (let i = 0; i < fft; i++) {
      let r = 0;
      for (let b = 0; b < bits; b++) r |= ((i >>> b) & 1) << (bits - 1 - b);
      this.rev[i] = r;
    }
    this.power = new Float32Array(this.bins);
    this.deemphasis = new Float32Array(this.bins);
    const pre = 0.97;
    for (let i = 0; i < this.bins; i++) {
      const w = (2 * Math.PI * i) / fft;
      this.deemphasis[i] = 1 / (1 + pre * pre - 2 * pre * Math.cos(w));
    }
    this.smooth = new Float32Array(this.bins);
    this.weights[0] = 1;
  }

  /** Latest mouth state (a fresh object; the analyser keeps its own buffers). */
  frame(): VisemeFrame {
    const names: Viseme[] = ['sil', 'PP', 'FF', 'SS', 'aa', 'E', 'ih', 'oh', 'ou'];
    return {
      weights: Array.from(this.weights),
      dominant: names[this.dominant] ?? 'sil',
      mouthOpen: this.mouthOpen,
      energy: this.energy,
      confidence: this.confidence,
      sequence: this.sequence,
    };
  }

  /** Forget smoothing history (a new talk spurt after a long gap starts clean). */
  reset(): void {
    this.weights.fill(0);
    this.weights[0] = 1;
    this.dominant = 0;
    this.mouthOpen = 0;
    this.energy = 0;
    this.confidence = 1;
    this.noiseFloor = 0.002;
  }

  private bin(hz: number): number {
    return Math.min(this.bins - 1, Math.floor(hz / this.binHz));
  }

  /** In-place radix-2 FFT of `re`/`im` (bit-reversed copy of `input` first). */
  private fft(): void {
    const n = this.fftSize;
    const re = this.re;
    const im = this.im;
    for (let i = 0; i < n; i++) {
      re[this.rev[i] as number] = this.input[i] as number;
    }
    im.fill(0);
    for (let size = 2; size <= n; size *= 2) {
      const half = size / 2;
      const step = n / size;
      for (let start = 0; start < n; start += size) {
        for (let k = 0; k < half; k++) {
          const wr = this.cosTable[k * step] as number;
          const wi = -(this.sinTable[k * step] as number);
          const a = start + k;
          const b = a + half;
          const br = re[b] as number;
          const bi = im[b] as number;
          const tr = br * wr - bi * wi;
          const ti = br * wi + bi * wr;
          const ar = re[a] as number;
          const ai = im[a] as number;
          re[b] = ar - tr;
          im[b] = ai - ti;
          re[a] = ar + tr;
          im[a] = ai + ti;
        }
      }
    }
  }

  /**
   * Analyse one frame of mono PCM (`-1..1`); shorter input is zero-padded, longer truncated.
   * Interleaved multichannel audio can be passed with `channels` > 1 (downmixed).
   */
  push(pcm: Float32Array, channels = 1): void {
    channels = Math.min(8, Math.max(1, channels | 0));
    const input = this.input;
    input.fill(0);
    let energy = 0;
    let crossings = 0;
    let prev = 0;
    const n = Math.min(Math.floor(pcm.length / channels), this.frameSamples);
    for (let i = 0; i < n; i++) {
      let s = 0;
      for (let c = 0; c < channels; c++) s += pcm[i * channels + c] as number;
      s /= channels;
      energy += s * s;
      if (i > 0 && s < 0 !== prev < 0) crossings++;
      input[i] = (s - 0.97 * prev) * (this.window[i] as number);
      prev = s;
    }
    const rms = n > 0 ? Math.sqrt(energy / n) : 0;
    const zcr = n > 1 ? crossings / (n - 1) : 0;
    this.classify(rms, zcr);
    this.smoothInto(rms);
  }

  private classify(rms: number, zcr: number): void {
    const w = this.target;
    w.fill(0);
    if (rms < this.noiseFloor) this.noiseFloor = Math.max(rms, 0.0005);
    else this.noiseFloor += (rms - this.noiseFloor) * 0.002;
    if (rms < 0.002 || rms < this.noiseFloor * 3) {
      w[0] = 1;
      return;
    }
    this.fft();
    const bins = this.bins;
    const power = this.power;
    const de = this.deemphasis;
    let total = 0;
    for (let i = 0; i < bins; i++) {
      const r = this.re[i] as number;
      const im = this.im[i] as number;
      const p = r * r + im * im;
      power[i] = p;
      total += p * (de[i] as number);
    }
    if (total <= 0) {
      w[0] = 1;
      return;
    }
    const band = (lo: number, hi: number): number => {
      const a = this.bin(lo);
      const b = this.bin(hi) + 1;
      let sum = 0;
      for (let i = Math.min(a, b); i < b; i++) sum += (power[i] as number) * (de[i] as number);
      return sum / total;
    };
    const high = band(4000, this.sampleRate / 2);
    const midHigh = band(2000, 4000);
    const low = band(0, 500);
    const friction = Math.min(1, high * 2 + midHigh);
    if (zcr > 0.16 || high > 0.45) {
      const sibilant = Math.min(1, Math.max(0, (high - 0.3) / 0.4));
      w[3] = sibilant;
      w[2] = 1 - sibilant;
      return;
    }
    const smooth = this.smooth;
    for (let i = 0; i < bins; i++) {
      const lo = Math.max(0, i - 3);
      const hi = Math.min(bins, i + 4);
      let sum = 0;
      for (let j = lo; j < hi; j++) sum += power[j] as number;
      smooth[i] = sum / (hi - lo);
    }
    // F1: strongest peak in 200–1000 Hz; F2: strongest above the valley after F1 (≥ 700 Hz, ≤ 3200 Hz).
    let i1 = this.bin(200);
    let p1 = 0;
    for (let i = this.bin(200); i <= this.bin(1000); i++) {
      if ((smooth[i] as number) > p1) {
        p1 = smooth[i] as number;
        i1 = i;
      }
    }
    let valley = i1 + 1;
    const f2End = this.bin(3200) + 1;
    while (valley + 1 < f2End && (smooth[valley + 1] as number) <= (smooth[valley] as number)) valley++;
    const f2Start = Math.max(valley, this.bin(700));
    let i2 = f2Start;
    let p2 = 0;
    for (let i = f2Start; i < f2End; i++) {
      if ((smooth[i] as number) > p2) {
        p2 = smooth[i] as number;
        i2 = i;
      }
    }
    const f1 = i1 * this.binHz;
    const f2 = i2 * this.binHz;
    const f2Ratio = p2 / Math.max(1e-12, p1);
    if (low > 0.85 && f2Ratio < 0.02 && friction < 0.05) {
      w[1] = 1;
      return;
    }
    // Nearest vowel centroid (F1, F2) in log-frequency, soft-assigned: aa E ih oh ou.
    const c1 = [750, 520, 330, 520, 330];
    const c2 = [1250, 1900, 2350, 900, 780];
    let sum = 0;
    for (let v = 0; v < 5; v++) {
      const d1 = Math.log(f1 / (c1[v] as number));
      const d2 = Math.log(f2 / (c2[v] as number));
      const weight = Math.exp(-(d1 * d1 * 6 + d2 * d2 * 6));
      w[4 + v] = weight;
      sum += weight;
    }
    if (sum > 0) for (let v = 4; v < 9; v++) w[v] = (w[v] as number) / sum;
    if (friction > 0.15) {
      const leak = Math.min(0.5, (friction - 0.15) / 0.5);
      for (let v = 0; v < 9; v++) w[v] = (w[v] as number) * (1 - leak);
      w[2] = (w[2] as number) + leak;
    }
  }

  private smoothInto(rms: number): void {
    const cur = this.weights;
    const t = this.target;
    let sum = 0;
    for (let i = 0; i < 9; i++) {
      const target = t[i] as number;
      const w = cur[i] as number;
      const alpha = target > w ? 0.65 : 0.3;
      const next = w + (target - w) * alpha;
      cur[i] = next;
      sum += next;
    }
    let best = 0;
    let bestW = 0;
    let second = 0;
    for (let i = 0; i < 9; i++) {
      const w = sum > 0 ? (cur[i] as number) / sum : (cur[i] as number);
      cur[i] = w;
      if (w > bestW) {
        second = bestW;
        bestW = w;
        best = i;
      } else if (w > second) {
        second = w;
      }
    }
    this.dominant = best;
    this.energy = Math.min(1, rms);
    this.confidence = Math.min(1, Math.max(0, bestW - second));
    const level = Math.sqrt(Math.min(1, rms / 0.1));
    const open = [0, 0, 0.2, 0.25, 1, 0.65, 0.4, 0.6, 0.3];
    let openness = 0;
    for (let i = 0; i < 9; i++) openness += (cur[i] as number) * (open[i] as number);
    const targetOpen = Math.min(1, Math.max(0, level * openness));
    const alpha = targetOpen > this.mouthOpen ? 0.65 : 0.3;
    this.mouthOpen += (targetOpen - this.mouthOpen) * alpha;
    this.sequence++;
  }
}

/** Posted by the worklet after every analysed frame. */
export interface VisemeWorkletFrame {
  type: 'frame';
  frame: VisemeFrame;
}

interface WorkletProcessorLike {
  port: {
    onmessage: ((ev: { data: { type: 'reset' } }) => void) | null;
    postMessage(msg: VisemeWorkletFrame): void;
  };
}

interface WorkletScopeLike {
  sampleRate: number;
  AudioWorkletProcessor: new () => WorkletProcessorLike;
  registerProcessor(name: string, ctor: unknown): void;
}

/**
 * Body of the worklet module: registers `aurix-visemes`, a sink processor that downmixes its
 * input, gathers 20 ms frames and posts one {@link VisemeFrame} per frame to the main thread.
 */
export function visemeWorkletMain(scope: WorkletScopeLike): void {
  const Base = scope.AudioWorkletProcessor;
  class AurixVisemeProcessor extends Base {
    private readonly analyzer: VisemeAnalyzer;
    private readonly buf: Float32Array;
    private fill = 0;
    constructor() {
      super();
      this.analyzer = new VisemeAnalyzer(scope.sampleRate);
      this.buf = new Float32Array(this.analyzer.frameSamples);
      this.port.onmessage = (ev) => {
        if (ev.data.type === 'reset') {
          this.analyzer.reset();
          this.fill = 0;
        }
      };
    }
    process(inputs: Float32Array[][]): boolean {
      const input = inputs[0];
      if (!input || input.length === 0) return true;
      const first = input[0] as Float32Array;
      const n = first.length;
      const chs = input.length;
      for (let i = 0; i < n; i++) {
        let s = 0;
        for (let c = 0; c < chs; c++) s += (input[c] as Float32Array)[i] as number;
        this.buf[this.fill++] = s / chs;
        if (this.fill === this.buf.length) {
          this.fill = 0;
          this.analyzer.push(this.buf, 1);
          this.port.postMessage({ type: 'frame', frame: this.analyzer.frame() });
        }
      }
      return true;
    }
  }
  scope.registerProcessor('aurix-visemes', AurixVisemeProcessor);
}

// ── DSP_UNITS_END ────────────────────────────────────────────────────────────────────────

export const VISEME_PROCESSOR = 'aurix-visemes';

/** JavaScript source of the worklet module (self-contained; served from a `blob:` URL). */
export function visemeWorkletSource(): string {
  return `'use strict';\n${VisemeAnalyzer.toString()}\n${visemeWorkletMain.toString()}\nvisemeWorkletMain(globalThis);\n`;
}

/** Whether this browser can run the viseme worklet. */
export function supportsVisemes(): boolean {
  return (
    typeof AudioContext !== 'undefined' &&
    typeof AudioWorkletNode !== 'undefined' &&
    typeof Blob !== 'undefined' &&
    typeof URL !== 'undefined' &&
    typeof URL.createObjectURL === 'function'
  );
}

const loadedContexts = new WeakSet<BaseAudioContext>();
export async function loadVisemeWorklet(ctx: BaseAudioContext): Promise<void> {
  if (loadedContexts.has(ctx)) return;
  if (!supportsVisemes()) throw new Error('AudioWorklet is unavailable: visemes are not supported here');
  const url = URL.createObjectURL(new Blob([visemeWorkletSource()], { type: 'text/javascript' }));
  try {
    await ctx.audioWorklet.addModule(url);
  } finally {
    URL.revokeObjectURL(url);
  }
  loadedContexts.add(ctx);
}

/**
 * Creates a viseme sink on `ctx` (module loaded); `onFrame` receives every analysed frame.
 * Connect any source to it; it produces no audio.
 */
export function createVisemeNode(ctx: BaseAudioContext, onFrame: (frame: VisemeFrame) => void): AudioWorkletNode {
  const node = new AudioWorkletNode(ctx, VISEME_PROCESSOR, {
    numberOfInputs: 1,
    numberOfOutputs: 0,
    channelCount: 2,
    channelCountMode: 'clamped-max',
  });
  node.port.onmessage = (ev: MessageEvent<VisemeWorkletFrame>) => {
    if (ev.data?.type === 'frame') onFrame(ev.data.frame);
  };
  return node;
}
