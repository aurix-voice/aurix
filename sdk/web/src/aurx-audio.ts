/**
 * Audio for the WebTransport media path: the browser encodes and decodes Opus itself with
 * WebCodecs (`AudioEncoder` / `AudioDecoder`) instead of WebRTC, which is what gives it the
 * full encoder parameter set (complexity, signal, application, expected loss, FEC, DTX,
 * bitrate) the native SDKs have.
 *
 * - {@link AurxCapture}: microphone stream → 48 kHz AudioWorklet (20 ms frames + RMS) →
 *   `AudioEncoder` → Opus packets.
 * - {@link AurxPlayback}: one `AudioDecoder` and one playback worklet node per downlink
 *   SSRC; the node is a plain `AudioNode` the spatial renderer places like a WebRTC track.
 */

import type { DownlinkCodec } from './aurx.js';
import { decodeG711, G711_SAMPLE_RATE, G711Upsampler, type G711Law } from './g711.js';
import type { AudioPolicy } from './opus.js';

export const AURX_SAMPLE_RATE = 48_000;
/** 20 ms at 48 kHz. */
export const AURX_FRAME_SAMPLES = 960;

/** WebCodecs Opus encoder controls (the WebCodecs Opus registration + Chromium extras). */
export interface AurxOpusConfig {
  bitrateBps: number;
  channels: 1 | 2;
  /** 0–10; the browser default when unset. */
  complexity?: number;
  signal: 'auto' | 'voice' | 'music';
  application: 'voip' | 'audio' | 'lowdelay';
  fec: boolean;
  dtx: boolean;
  /** Expected packet loss in percent (drives in-band FEC redundancy). */
  expectedLossPct: number;
  /** `true` for constant bitrate. */
  cbr: boolean;
}

/** Encoder configuration for a channel policy (the same mapping the native core applies). */
export function opusConfigFor(policy: AudioPolicy, overrides: Partial<AurxOpusConfig> = {}): AurxOpusConfig {
  const cfg: AurxOpusConfig = {
    bitrateBps: policy.bitrateBps,
    channels: policy.stereo ? 2 : 1,
    signal: policy.signal,
    application: policy.signal === 'music' ? 'audio' : 'voip',
    fec: policy.fec,
    dtx: policy.dtx,
    expectedLossPct: policy.fec ? 10 : 0,
    cbr: false,
    ...overrides,
  };
  if (policy.complexity !== undefined && overrides.complexity === undefined) cfg.complexity = policy.complexity;
  return cfg;
}

interface OpusEncoderConfigExt {
  format?: 'opus';
  frameDuration?: number;
  complexity?: number;
  packetlossperc?: number;
  useinbandfec?: boolean;
  usedtx?: boolean;
  signal?: 'auto' | 'music' | 'voice';
  application?: 'voip' | 'audio' | 'lowdelay';
}

function encoderConfig(cfg: AurxOpusConfig): AudioEncoderConfig {
  const opus: OpusEncoderConfigExt = {
    format: 'opus',
    frameDuration: 20_000,
    packetlossperc: Math.min(100, Math.max(0, Math.round(cfg.expectedLossPct))),
    useinbandfec: cfg.fec,
    usedtx: cfg.dtx,
    signal: cfg.signal,
    application: cfg.application,
  };
  if (cfg.complexity !== undefined) opus.complexity = Math.min(10, Math.max(0, Math.round(cfg.complexity)));
  return {
    codec: 'opus',
    sampleRate: AURX_SAMPLE_RATE,
    numberOfChannels: cfg.channels,
    bitrate: Math.max(6_000, Math.round(cfg.bitrateBps)),
    bitrateMode: cfg.cbr ? 'constant' : 'variable',
    opus: opus as OpusEncoderConfig,
  };
}

/** Whether this browser can run the AURX audio path (WebCodecs Opus + AudioWorklet). */
export function supportsAurxAudio(): boolean {
  return (
    typeof AudioEncoder === 'function' &&
    typeof AudioDecoder === 'function' &&
    typeof AudioData === 'function' &&
    typeof EncodedAudioChunk === 'function' &&
    typeof AudioContext !== 'undefined' &&
    typeof AudioWorkletNode !== 'undefined' &&
    typeof Blob !== 'undefined' &&
    typeof URL !== 'undefined' &&
    typeof URL.createObjectURL === 'function'
  );
}

/** `AudioEncoder.isConfigSupported` for the policy's shape (Opus 48 kHz mono/stereo). */
export async function aurxOpusSupported(channels: 1 | 2 = 1): Promise<boolean> {
  if (typeof AudioEncoder !== 'function' || typeof AudioDecoder !== 'function') return false;
  try {
    const [enc, dec] = await Promise.all([
      AudioEncoder.isConfigSupported(encoderConfig({ ...DEFAULT_OPUS, channels })),
      AudioDecoder.isConfigSupported({ codec: 'opus', sampleRate: AURX_SAMPLE_RATE, numberOfChannels: channels }),
    ]);
    return enc.supported === true && dec.supported === true;
  } catch {
    return false;
  }
}

const DEFAULT_OPUS: AurxOpusConfig = {
  bitrateBps: 48_000,
  channels: 1,
  signal: 'voice',
  application: 'voip',
  fec: true,
  dtx: true,
  expectedLossPct: 10,
  cbr: false,
};

// ── Worklets ──

interface AudioWorkletProcessorLike {
  readonly port: MessagePort;
}
interface AudioWorkletScope {
  AudioWorkletProcessor: new () => AudioWorkletProcessorLike;
  registerProcessor(name: string, ctor: unknown): void;
  sampleRate: number;
}

/**
 * Capture: sums the input to `channels`, cuts 960-sample frames and posts them (planar
 * Float32) with their RMS. Playback: a ring buffer per node fed by `pcm` messages, played
 * once `target` samples are queued. The target depth is set by the main thread from the
 * measured arrival jitter; when the queue runs dry the player conceals with pitch-repeated
 * history fading out over 60 ms, and when the queue sits above the target for a while it
 * shortens the queued audio one pitch period at a time (overlap-add of two periods, WSOLA
 * style) so latency shrinks again without an audible skip.
 */
function aurxWorkletMain(scope: AudioWorkletScope): void {
  const FRAME = 960;
  /** History kept for concealment (pitch periods down to 50 Hz need two periods). */
  const HIST = FRAME * 2;
  /** Longest run of concealed output before falling back to silence: 60 ms. */
  const PLC_MAX = FRAME * 3;
  /** Crossfade from concealment back into real audio. */
  const XFADE = 96;
  /** Quanta the queue must sit above target + one frame before the first trim (~2 s). */
  const TRIM_HOLD = 750;
  /** Quanta between further trims while the queue stays above target (~100 ms). */
  const TRIM_REPEAT = 38;
  const Base = scope.AudioWorkletProcessor;

  class AurixAurxCapture extends Base {
    private channels = 1;
    private planes: Float32Array[] = [new Float32Array(FRAME)];
    private fill = 0;
    constructor() {
      super();
      this.port.onmessage = (ev) => {
        if (ev.data.type === 'channels') {
          this.channels = ev.data.channels === 2 ? 2 : 1;
          this.planes = [];
          for (let c = 0; c < this.channels; c++) this.planes.push(new Float32Array(FRAME));
          this.fill = 0;
        }
      };
    }
    process(inputs: Float32Array[][]): boolean {
      const input = inputs[0];
      if (!input || input.length === 0) return true;
      const n = (input[0] as Float32Array).length;
      const inChs = input.length;
      for (let i = 0; i < n; i++) {
        if (this.channels === 1) {
          let s = 0;
          for (let c = 0; c < inChs; c++) s += (input[c] as Float32Array)[i] as number;
          (this.planes[0] as Float32Array)[this.fill] = s / inChs;
        } else {
          const l = (input[0] as Float32Array)[i] as number;
          const r = (input[inChs > 1 ? 1 : 0] as Float32Array)[i] as number;
          (this.planes[0] as Float32Array)[this.fill] = l;
          (this.planes[1] as Float32Array)[this.fill] = r;
        }
        this.fill++;
        if (this.fill === FRAME) {
          this.fill = 0;
          let sum = 0;
          for (const plane of this.planes) for (let k = 0; k < FRAME; k++) sum += (plane[k] as number) * (plane[k] as number);
          const rms = Math.sqrt(sum / (FRAME * this.planes.length));
          const out = this.planes.map((p) => p.slice());
          this.port.postMessage({ type: 'frame', planes: out, rms }, out.map((p) => p.buffer));
        }
      }
      return true;
    }
  }

  class AurixAurxPlayer extends Base {
    private readonly cap = FRAME * 16;
    private readonly left: Float32Array;
    private readonly right: Float32Array;
    private readIdx = 0;
    private writeIdx = 0;
    private queued = 0;
    private primed = false;
    private target = FRAME * 2;
    private underruns = 0;
    private concealed = 0;
    private trimmed = 0;
    private overflowDropped = 0;
    private highFor = 0;
    private quanta = 0;
    private readonly histL = new Float32Array(HIST);
    private readonly histR = new Float32Array(HIST);
    private histIdx = 0;
    private histFill = 0;
    private plcL = new Float32Array(0);
    private plcR = new Float32Array(0);
    private plcPos = 0;
    private plcDone = 0;
    private plcActive = false;
    private xfadeLeft = 0;
    /** Period being removed by the current trim (0 = none) and how far its overlap-add got. */
    private trimLag = 0;
    private trimPos = 0;
    /** 48 kHz → context rate (`1` when the context runs at 48 kHz). */
    private readonly step = 48_000 / scope.sampleRate;
    private phase = 0;
    private lastL = 0;
    private lastR = 0;
    constructor() {
      super();
      this.left = new Float32Array(this.cap);
      this.right = new Float32Array(this.cap);
      this.port.onmessage = (ev) => {
        const d = ev.data;
        if (d.type === 'pcm') this.push(d.planes as Float32Array[]);
        else if (d.type === 'target') this.target = Math.max(FRAME, Math.min(this.cap - 2 * FRAME, Math.round(d.samples as number)));
        else if (d.type === 'flush') {
          this.readIdx = this.writeIdx = this.queued = 0;
          this.primed = false;
          this.plcActive = false;
          this.xfadeLeft = 0;
          this.histFill = 0;
          this.highFor = 0;
          this.trimLag = 0;
        }
      };
    }
    private push(planes: Float32Array[]): void {
      const l = planes[0];
      if (!l) return;
      const r = planes[1] ?? l;
      if (this.step !== 1) {
        this.pushResampled(l, r);
        return;
      }
      this.write(l, r, l.length);
    }
    /** Linear interpolation from 48 kHz into the context rate, continuous across frames. */
    private pushResampled(l: Float32Array, r: Float32Array): void {
      const n = l.length;
      const outN = Math.floor((n - this.phase) / this.step) + 1;
      const ol = new Float32Array(Math.max(0, outN));
      const or = new Float32Array(Math.max(0, outN));
      let count = 0;
      let pos = this.phase;
      while (pos < n) {
        const i = Math.floor(pos);
        const frac = pos - i;
        const pl = i === 0 ? this.lastL : (l[i - 1] as number);
        const pr = i === 0 ? this.lastR : (r[i - 1] as number);
        ol[count] = pl + ((l[i] as number) - pl) * frac;
        or[count] = pr + ((r[i] as number) - pr) * frac;
        count++;
        pos += this.step;
      }
      this.phase = pos - n;
      this.lastL = l[n - 1] as number;
      this.lastR = r[n - 1] as number;
      this.write(ol, or, count);
    }
    private write(l: Float32Array, r: Float32Array, n: number): void {
      if (this.queued + n > this.cap) {
        // Late by more than the buffer: drop the oldest to keep latency bounded.
        const drop = this.queued + n - this.cap;
        this.readIdx = (this.readIdx + drop) % this.cap;
        this.queued -= drop;
        this.overflowDropped += drop;
      }
      for (let i = 0; i < n; i++) {
        this.left[this.writeIdx] = l[i] as number;
        this.right[this.writeIdx] = r[i] as number;
        this.writeIdx = (this.writeIdx + 1) % this.cap;
      }
      this.queued += n;
      if (!this.primed && this.queued >= this.target) this.primed = true;
    }
    process(_inputs: Float32Array[][], outputs: Float32Array[][]): boolean {
      const out = outputs[0];
      if (!out || out.length === 0) return true;
      const ol = out[0] as Float32Array;
      const or = (out[1] ?? out[0]) as Float32Array;
      const n = ol.length;
      if (++this.quanta % 64 === 0) this.report();
      if (!this.primed || this.queued < n) {
        if (this.primed) {
          this.underruns++;
          this.primed = false;
          this.port.postMessage({ type: 'underrun', count: this.underruns });
          this.startConcealment();
        }
        this.conceal(ol, or, n);
        return true;
      }
      if (this.trimLag === 0) {
        if (this.queued > this.target + FRAME) {
          if (++this.highFor >= TRIM_HOLD && this.queued >= 2 * FRAME + n) {
            this.startTrim();
            this.highFor = TRIM_HOLD - TRIM_REPEAT;
          }
        } else this.highFor = 0;
      }
      if (this.trimLag !== 0 && this.queued < 2 * this.trimLag + n) this.trimLag = 0;
      if (this.trimLag !== 0) this.trim(ol, or, n);
      else {
        for (let i = 0; i < n; i++) {
          ol[i] = this.left[this.readIdx] as number;
          or[i] = this.right[this.readIdx] as number;
          this.readIdx = (this.readIdx + 1) % this.cap;
        }
        this.queued -= n;
        if (this.xfadeLeft > 0) this.blendIn(ol, or, n);
        this.remember(ol, or, n);
      }
      return true;
    }
    /**
     * Choose the period to remove: the lag (2.5–20 ms) at which the next 20 ms of queued audio
     * best matches the audio one lag later, so overlap-adding the two is seamless.
     */
    private startTrim(): void {
      const cap = this.cap;
      const m = new Float32Array(2 * FRAME);
      for (let i = 0; i < 2 * FRAME; i++) {
        const j = (this.readIdx + i) % cap;
        m[i] = ((this.left[j] as number) + (this.right[j] as number)) * 0.5;
      }
      let e0 = 0;
      for (let i = 0; i < FRAME; i++) e0 += (m[i] as number) * (m[i] as number);
      const score = (lag: number): number => {
        let c = 0;
        let e1 = 0;
        for (let i = 0; i < FRAME; i++) {
          const v = m[i + lag] as number;
          c += (m[i] as number) * v;
          e1 += v * v;
        }
        return c / Math.sqrt(e0 * e1 + 1e-12);
      };
      let best = FRAME;
      let bestScore = -Infinity;
      for (let lag = 120; lag <= FRAME; lag += 4) {
        const sc = score(lag);
        if (sc > bestScore) {
          bestScore = sc;
          best = lag;
        }
      }
      for (let lag = Math.max(120, best - 3); lag <= Math.min(FRAME, best + 3); lag++) {
        const sc = score(lag);
        if (sc > bestScore) {
          bestScore = sc;
          best = lag;
        }
      }
      if (e0 < 1e-9) best = FRAME;
      this.trimLag = best;
      this.trimPos = 0;
    }
    /**
     * Overlap-add the period starting at the read point with the one after it; when the
     * period is done the read point skips it, so `trimLag` samples of latency are gone.
     */
    private trim(ol: Float32Array, or: Float32Array, n: number): void {
      const cap = this.cap;
      const lag = this.trimLag;
      for (let i = 0; i < n; i++) {
        if (this.trimLag === 0) {
          ol[i] = this.left[this.readIdx] as number;
          or[i] = this.right[this.readIdx] as number;
          this.readIdx = (this.readIdx + 1) % cap;
          this.queued--;
          continue;
        }
        const a = this.readIdx;
        const b = (a + lag) % cap;
        const w = this.trimPos / lag;
        ol[i] = (this.left[a] as number) * (1 - w) + (this.left[b] as number) * w;
        or[i] = (this.right[a] as number) * (1 - w) + (this.right[b] as number) * w;
        this.readIdx = (a + 1) % cap;
        this.queued--;
        if (++this.trimPos === lag) {
          this.readIdx = (this.readIdx + lag) % cap;
          this.queued -= lag;
          this.trimmed += lag;
          this.trimLag = 0;
        }
      }
      if (this.xfadeLeft > 0) this.blendIn(ol, or, n);
      this.remember(ol, or, n);
    }
    private remember(ol: Float32Array, or: Float32Array, n: number): void {
      for (let i = 0; i < n; i++) {
        this.histL[this.histIdx] = ol[i] as number;
        this.histR[this.histIdx] = or[i] as number;
        this.histIdx = (this.histIdx + 1) % HIST;
      }
      this.histFill = Math.min(HIST, this.histFill + n);
    }
    /**
     * Pick the repetition period from the last 20 ms of output (normalised autocorrelation
     * over 2.5–20 ms lags, coarse search then refinement) and copy that period out of history.
     */
    private startConcealment(): void {
      if (this.histFill < HIST) {
        this.plcActive = false;
        return;
      }
      const m = new Float32Array(HIST);
      for (let i = 0; i < HIST; i++) {
        const j = (this.histIdx + i) % HIST;
        m[i] = ((this.histL[j] as number) + (this.histR[j] as number)) * 0.5;
      }
      const win = FRAME;
      const start = HIST - win;
      let e0 = 0;
      for (let i = start; i < HIST; i++) e0 += (m[i] as number) * (m[i] as number);
      const score = (lag: number): number => {
        let c = 0;
        let e1 = 0;
        for (let i = start; i < HIST; i++) {
          const v = m[i - lag] as number;
          c += (m[i] as number) * v;
          e1 += v * v;
        }
        return c / Math.sqrt(e0 * e1 + 1e-12);
      };
      let best = FRAME;
      let bestScore = -Infinity;
      for (let lag = 120; lag <= FRAME; lag += 4) {
        const s = score(lag);
        if (s > bestScore) {
          bestScore = s;
          best = lag;
        }
      }
      for (let lag = Math.max(120, best - 3); lag <= Math.min(FRAME, best + 3); lag++) {
        const s = score(lag);
        if (s > bestScore) {
          bestScore = s;
          best = lag;
        }
      }
      if (e0 < 1e-9) best = FRAME;
      this.plcL = new Float32Array(best);
      this.plcR = new Float32Array(best);
      for (let i = 0; i < best; i++) {
        const j = (this.histIdx + HIST - best + i) % HIST;
        this.plcL[i] = this.histL[j] as number;
        this.plcR[i] = this.histR[j] as number;
      }
      this.plcPos = 0;
      this.plcDone = 0;
      this.plcActive = true;
    }
    /** One concealed sample pair (already faded); advances the concealment state. */
    private plcSample(): [number, number] {
      const period = this.plcL.length;
      if (!this.plcActive || period === 0 || this.plcDone >= PLC_MAX) return [0, 0];
      const g = 1 - this.plcDone / PLC_MAX;
      const l = (this.plcL[this.plcPos] as number) * g;
      const r = (this.plcR[this.plcPos] as number) * g;
      this.plcPos = (this.plcPos + 1) % period;
      this.plcDone++;
      return [l, r];
    }
    private conceal(ol: Float32Array, or: Float32Array, n: number): void {
      if (!this.plcActive || this.plcDone >= PLC_MAX) {
        ol.fill(0);
        if (or !== ol) or.fill(0);
        if (this.plcActive) this.plcActive = false;
        return;
      }
      for (let i = 0; i < n; i++) {
        const [l, r] = this.plcSample();
        ol[i] = l;
        or[i] = r;
      }
      this.concealed += n;
      this.xfadeLeft = XFADE;
      this.remember(ol, or, n);
    }
    /** First real samples after concealment: fade the repeated history out under them. */
    private blendIn(ol: Float32Array, or: Float32Array, n: number): void {
      for (let i = 0; i < n && this.xfadeLeft > 0; i++) {
        const w = this.xfadeLeft / XFADE;
        const [l, r] = this.plcSample();
        ol[i] = (ol[i] as number) * (1 - w) + l * w;
        or[i] = (or[i] as number) * (1 - w) + r * w;
        this.xfadeLeft--;
      }
      if (this.xfadeLeft === 0) this.plcActive = false;
    }
    private report(): void {
      this.port.postMessage({
        type: 'stats',
        queued: this.queued,
        target: this.target,
        underruns: this.underruns,
        concealed: this.concealed,
        trimmed: this.trimmed,
        overflowDropped: this.overflowDropped,
      });
    }
  }

  scope.registerProcessor('aurix-aurx-capture', AurixAurxCapture);
  scope.registerProcessor('aurix-aurx-player', AurixAurxPlayer);
}

export function aurxWorkletSource(): string {
  return `'use strict';\n(${aurxWorkletMain.toString()})(globalThis);\n`;
}

const loadedContexts = new WeakSet<BaseAudioContext>();
export async function loadAurxWorklet(ctx: BaseAudioContext): Promise<void> {
  if (loadedContexts.has(ctx)) return;
  const url = URL.createObjectURL(new Blob([aurxWorkletSource()], { type: 'text/javascript' }));
  try {
    await ctx.audioWorklet.addModule(url);
  } finally {
    URL.revokeObjectURL(url);
  }
  loadedContexts.add(ctx);
}

// ── Capture + encoder ──

export interface AurxCaptureFrame {
  opus: Uint8Array;
  /** Frame timestamp in 48 kHz samples (wraps at 2^32). */
  timestamp: number;
  /** Linear RMS of the frame before encoding (`0..1`). */
  energy: number;
}

interface CaptureWorkletFrame {
  type: 'frame';
  planes: Float32Array[];
  rms: number;
}

/**
 * Microphone → 20 ms Opus packets. Its own 48 kHz `AudioContext` (the source stream may be
 * from another context or device rate; the browser resamples across the `MediaStream`).
 */
export class AurxCapture {
  private ctx: AudioContext | undefined;
  private source: MediaStreamAudioSourceNode | undefined;
  private node: AudioWorkletNode | undefined;
  private encoder: AudioEncoder | undefined;
  private config: AurxOpusConfig = DEFAULT_OPUS;
  private samples = 0;
  /** Frame timestamp (µs) → RMS of the frame handed to the encoder (DTX may skip outputs). */
  private pendingEnergy = new Map<number, number>();
  private closed = false;
  private paused = false;

  constructor(
    private readonly onFrame: (frame: AurxCaptureFrame) => void,
    private readonly onError: (error: Error) => void,
  ) {}

  get audioContext(): AudioContext | undefined {
    return this.ctx;
  }

  get opusConfig(): AurxOpusConfig {
    return this.config;
  }

  async start(stream: MediaStream, config: AurxOpusConfig): Promise<void> {
    if (this.closed) throw new Error('capture closed');
    this.config = config;
    const ctx = new AudioContext({ sampleRate: AURX_SAMPLE_RATE, latencyHint: 'interactive' });
    this.ctx = ctx;
    await loadAurxWorklet(ctx);
    if (this.closed) {
      void ctx.close().catch(() => undefined);
      return;
    }
    const node = new AudioWorkletNode(ctx, 'aurix-aurx-capture', {
      numberOfInputs: 1,
      numberOfOutputs: 0,
      channelCount: 2,
      channelCountMode: 'clamped-max',
    });
    node.port.postMessage({ type: 'channels', channels: config.channels });
    node.port.onmessage = (ev: MessageEvent<CaptureWorkletFrame>) => {
      if (ev.data?.type === 'frame') this.encode(ev.data.planes, ev.data.rms);
    };
    this.node = node;
    this.setStream(stream);
    this.openEncoder();
    if (ctx.state === 'suspended') void ctx.resume().catch(() => undefined);
  }

  /** Feed a different (processed) microphone stream. */
  setStream(stream: MediaStream): void {
    if (!this.ctx || !this.node) return;
    this.source?.disconnect();
    this.source = this.ctx.createMediaStreamSource(stream);
    this.source.connect(this.node);
  }

  /** Muted: frames are neither encoded nor sent (DTX-like silence on the wire). */
  setPaused(paused: boolean): void {
    this.paused = paused;
  }

  /** Apply new encoder parameters; a channel-count change restarts the encoder. */
  reconfigure(config: AurxOpusConfig): void {
    const restart = config.channels !== this.config.channels;
    this.config = config;
    if (!this.encoder) return;
    if (restart) {
      this.node?.port.postMessage({ type: 'channels', channels: config.channels });
      this.closeEncoder();
      this.openEncoder();
      return;
    }
    try {
      this.encoder.configure(encoderConfig(config));
    } catch (e) {
      this.onError(e instanceof Error ? e : new Error(String(e)));
    }
  }

  async resume(): Promise<boolean> {
    if (!this.ctx) return false;
    try {
      await this.ctx.resume();
    } catch {
      return false;
    }
    return this.ctx.state === 'running';
  }

  stop(): void {
    this.closed = true;
    this.closeEncoder();
    this.source?.disconnect();
    this.source = undefined;
    if (this.node) {
      this.node.port.onmessage = null;
      this.node.port.close();
      this.node.disconnect();
      this.node = undefined;
    }
    const ctx = this.ctx;
    this.ctx = undefined;
    if (ctx) void ctx.close().catch(() => undefined);
  }

  private openEncoder(): void {
    this.pendingEnergy.clear();
    const encoder = new AudioEncoder({
      output: (chunk) => {
        if (this.encoder !== encoder) return;
        const energy = this.takeEnergy(chunk.timestamp);
        if (chunk.byteLength === 0) return;
        const opus = new Uint8Array(chunk.byteLength);
        chunk.copyTo(opus);
        // Timestamps are µs; the wire carries 48 kHz samples.
        const timestamp = Math.round((chunk.timestamp * AURX_SAMPLE_RATE) / 1_000_000) >>> 0;
        this.onFrame({ opus, timestamp, energy });
      },
      error: (e) => {
        if (this.encoder !== encoder) return;
        this.onError(e instanceof Error ? e : new Error(String(e)));
      },
    });
    encoder.configure(encoderConfig(this.config));
    this.encoder = encoder;
  }

  /**
   * Level of the frame the encoder just emitted. Chromium stamps outputs from its own frame
   * count rather than echoing the input timestamps, so when they disagree the entries are
   * consumed in order (one output per 20 ms input).
   */
  private takeEnergy(chunkTimestampUs: number): number {
    const exact = this.pendingEnergy.get(chunkTimestampUs);
    if (exact !== undefined) {
      for (const key of Array.from(this.pendingEnergy.keys())) {
        if (key > chunkTimestampUs) break;
        this.pendingEnergy.delete(key);
      }
      return exact;
    }
    const oldest = this.pendingEnergy.entries().next().value;
    if (!oldest) return 0;
    this.pendingEnergy.delete(oldest[0]);
    return oldest[1];
  }

  private closeEncoder(): void {
    const enc = this.encoder;
    this.encoder = undefined;
    if (!enc) return;
    try {
      if (enc.state !== 'closed') enc.close();
    } catch {
      // already closed
    }
  }

  private encode(planes: Float32Array[], rms: number): void {
    const encoder = this.encoder;
    const ts = this.samples;
    this.samples = (this.samples + AURX_FRAME_SAMPLES) >>> 0;
    if (!encoder || encoder.state !== 'configured' || this.paused) return;
    const channels = this.config.channels;
    const data = new Float32Array(AURX_FRAME_SAMPLES * channels);
    for (let c = 0; c < channels; c++) data.set(planes[c] ?? planes[0] ?? new Float32Array(AURX_FRAME_SAMPLES), c * AURX_FRAME_SAMPLES);
    const timestampUs = Math.round((ts * 1_000_000) / AURX_SAMPLE_RATE);
    let audio: AudioData;
    try {
      audio = new AudioData({
        format: 'f32-planar',
        sampleRate: AURX_SAMPLE_RATE,
        numberOfFrames: AURX_FRAME_SAMPLES,
        numberOfChannels: channels,
        timestamp: timestampUs,
        data,
      });
    } catch (e) {
      this.onError(e instanceof Error ? e : new Error(String(e)));
      return;
    }
    // Bound the queue so a stalled encoder does not eat memory; RMS list follows the frames.
    if (encoder.encodeQueueSize > 8) {
      audio.close();
      return;
    }
    this.pendingEnergy.set(timestampUs, rms);
    if (this.pendingEnergy.size > 64) {
      const oldest = this.pendingEnergy.keys().next().value;
      if (oldest !== undefined) this.pendingEnergy.delete(oldest);
    }
    try {
      encoder.encode(audio);
    } catch (e) {
      this.onError(e instanceof Error ? e : new Error(String(e)));
    } finally {
      audio.close();
    }
  }
}

// ── Decoder + playback ──

interface PlaybackSlot {
  node: AudioWorkletNode;
  decoder: AudioDecoder | undefined;
  channels: 1 | 2;
  lastSeq: number | undefined;
  underruns: number;
  concealedSamples: number;
  trimmedSamples: number;
  overflowDropped: number;
  /** Queue depth the worklet last reported, in samples. */
  depth: number;
  /** Target depth currently pushed to the worklet, in frames. */
  targetFrames: number;
  lastArrivalMs: number | undefined;
  lastTimestamp: number | undefined;
  /** RFC 3550 interarrival jitter estimate, ms. */
  jitterMs: number;
  /** Decaying peak of late arrivals (arrival gap beyond the media gap), ms. */
  lateMs: number;
  /** 8 kHz → 48 kHz interpolator, created on the first G.711 frame of the stream. */
  g711: G711Upsampler | undefined;
}

export interface AurxPlaybackStats {
  slots: number;
  framesDecoded: number;
  framesDropped: number;
  underruns: number;
  /** Samples the players filled by concealment (pitch-repeated history), all slots. */
  concealedSamples: number;
  /** Samples trimmed to shrink playout delay after the arrival jitter settled. */
  trimmedSamples: number;
  /** Samples dropped because a burst overflowed a player's buffer. */
  overflowDropped: number;
  /** Largest interarrival jitter estimate across slots, ms. */
  jitterMs: number;
  /** Largest playout target across slots, ms. */
  targetDelayMs: number;
  /** Largest reported queue depth across slots, ms. */
  depthMs: number;
}

interface PlayerMessage {
  type: 'underrun' | 'stats';
  count?: number;
  queued?: number;
  target?: number;
  underruns?: number;
  concealed?: number;
  trimmed?: number;
  overflowDropped?: number;
}

/** Playout depth bounds (frames of 20 ms). */
export const AURX_PLAYOUT_MIN_FRAMES = 2;
export const AURX_PLAYOUT_MAX_FRAMES = 12;

/**
 * Target depth for the measured arrival behaviour: cover the recent peak lateness with a
 * quarter of headroom and a few ms for decoder scheduling, rounded up to whole frames.
 */
export function playoutTargetFrames(lateMs: number, jitterMs: number, min = AURX_PLAYOUT_MIN_FRAMES, max = AURX_PLAYOUT_MAX_FRAMES): number {
  const needMs = Math.max(lateMs * 1.25, jitterMs * 3) + 8;
  return Math.max(min, Math.min(max, Math.ceil(needMs / 20)));
}

/** The peak lateness that makes `playoutTargetFrames` ask for exactly `frames`. */
function latenessForFrames(frames: number): number {
  return (frames * 20 - 8) / 1.25;
}

/** Peak lateness decays per packet so a quiet network shrinks the target within ~10 s. */
export const AURX_LATE_DECAY = 0.996;

/**
 * Decoded playback per downlink SSRC on a shared `AudioContext` (the spatial renderer's).
 * {@link node} of a slot is what the renderer connects; the caller places it.
 */
export class AurxPlayback {
  private readonly slots = new Map<number, PlaybackSlot>();
  private framesDecoded = 0;
  private framesDropped = 0;
  /** Smallest playout depth, in frames (default 2 = 40 ms); the adaptive target never goes below it. */
  minFrames = AURX_PLAYOUT_MIN_FRAMES;
  /** Largest playout depth, in frames (default 12 = 240 ms). */
  maxFrames = AURX_PLAYOUT_MAX_FRAMES;
  /** Clock for arrival times (ms); replaceable for tests. */
  now: () => number = () => (typeof performance !== 'undefined' ? performance.now() : Date.now());

  constructor(
    readonly context: BaseAudioContext,
    private readonly onError: (error: Error) => void,
  ) {}

  async init(): Promise<void> {
    await loadAurxWorklet(this.context);
  }

  get stats(): AurxPlaybackStats {
    const out: AurxPlaybackStats = {
      slots: this.slots.size,
      framesDecoded: this.framesDecoded,
      framesDropped: this.framesDropped,
      underruns: 0,
      concealedSamples: 0,
      trimmedSamples: 0,
      overflowDropped: 0,
      jitterMs: 0,
      targetDelayMs: 0,
      depthMs: 0,
    };
    for (const s of this.slots.values()) {
      out.underruns += s.underruns;
      out.concealedSamples += s.concealedSamples;
      out.trimmedSamples += s.trimmedSamples;
      out.overflowDropped += s.overflowDropped;
      out.jitterMs = Math.max(out.jitterMs, s.jitterMs);
      out.targetDelayMs = Math.max(out.targetDelayMs, s.targetFrames * 20);
      out.depthMs = Math.max(out.depthMs, (s.depth / AURX_SAMPLE_RATE) * 1000);
    }
    return out;
  }

  has(ssrc: number): boolean {
    return this.slots.has(ssrc);
  }

  /** Current playout target of `ssrc` in ms, if the slot exists. */
  targetDelayMs(ssrc: number): number | undefined {
    const s = this.slots.get(ssrc);
    return s ? s.targetFrames * 20 : undefined;
  }

  /** The output node of `ssrc`'s voice, creating the slot (silent until frames arrive). */
  node(ssrc: number, stereo = false): AudioWorkletNode {
    const existing = this.slots.get(ssrc);
    if (existing) return existing.node;
    const node = new AudioWorkletNode(this.context, 'aurix-aurx-player', {
      numberOfInputs: 0,
      numberOfOutputs: 1,
      outputChannelCount: [2],
    });
    const slot: PlaybackSlot = {
      node,
      decoder: undefined,
      channels: stereo ? 2 : 1,
      lastSeq: undefined,
      underruns: 0,
      concealedSamples: 0,
      trimmedSamples: 0,
      overflowDropped: 0,
      depth: 0,
      targetFrames: this.minFrames,
      lastArrivalMs: undefined,
      lastTimestamp: undefined,
      jitterMs: 0,
      lateMs: 0,
      g711: undefined,
    };
    node.port.postMessage({ type: 'target', samples: slot.targetFrames * AURX_FRAME_SAMPLES });
    node.port.onmessage = (ev: MessageEvent<PlayerMessage>) => {
      const d = ev.data;
      if (!d) return;
      if (d.type === 'underrun') {
        slot.underruns = d.count ?? slot.underruns + 1;
        // The depth was not enough: hold one frame more than what just ran dry.
        slot.lateMs = Math.max(slot.lateMs, latenessForFrames(slot.targetFrames + 1));
        this.retarget(slot);
      } else if (d.type === 'stats') {
        slot.depth = d.queued ?? slot.depth;
        slot.underruns = d.underruns ?? slot.underruns;
        slot.concealedSamples = d.concealed ?? slot.concealedSamples;
        slot.trimmedSamples = d.trimmed ?? slot.trimmedSamples;
        slot.overflowDropped = d.overflowDropped ?? slot.overflowDropped;
      }
    };
    this.slots.set(ssrc, slot);
    return node;
  }

  /**
   * Arrival bookkeeping for one packet: RFC 3550 jitter plus a decaying peak of how much later
   * than its media gap the packet came (a burst after a stall shows up as one late packet).
   * A gap longer than the deepest buffer is a pause of the stream, not jitter it could absorb:
   * it is left out, and the underrun it caused adds one frame on its own.
   */
  private observeArrival(slot: PlaybackSlot, timestamp: number): void {
    const now = this.now();
    if (slot.lastArrivalMs !== undefined && slot.lastTimestamp !== undefined) {
      const mediaMs = (((timestamp - slot.lastTimestamp) | 0) / AURX_SAMPLE_RATE) * 1000;
      const d = now - slot.lastArrivalMs - mediaMs;
      if (Number.isFinite(d) && Math.abs(d) <= this.maxFrames * 20) {
        slot.jitterMs += (Math.abs(d) - slot.jitterMs) / 16;
        slot.lateMs = Math.max(d, slot.lateMs * AURX_LATE_DECAY);
      }
    }
    slot.lastArrivalMs = now;
    slot.lastTimestamp = timestamp;
    this.retarget(slot);
  }

  private retarget(slot: PlaybackSlot): void {
    const frames = playoutTargetFrames(slot.lateMs, slot.jitterMs, this.minFrames, this.maxFrames);
    if (frames === slot.targetFrames) return;
    slot.targetFrames = frames;
    slot.node.port.postMessage({ type: 'target', samples: frames * AURX_FRAME_SAMPLES });
  }

  /** Decode one frame of `ssrc` (Opus, or G.711 when `codec` says so) and queue it for playback. */
  push(ssrc: number, frame: Uint8Array, sequence: number, timestamp: number, stereo: boolean, codec: DownlinkCodec = 'opus'): void {
    const slot = this.slots.get(ssrc);
    if (!slot) return;
    if (slot.lastSeq !== undefined) {
      const delta = (sequence - slot.lastSeq) | 0;
      if (delta <= 0) {
        this.framesDropped += 1;
        return;
      }
    }
    slot.lastSeq = sequence;
    this.observeArrival(slot, timestamp);
    if (codec !== 'opus') {
      this.pushG711(slot, codec, frame);
      return;
    }
    // An Opus DTX "nothing to send" frame decodes to nothing; the player just runs dry.
    if (frame.length === 0) return;
    const wantChannels: 1 | 2 = stereo ? 2 : 1;
    if (!slot.decoder || slot.channels !== wantChannels || slot.decoder.state === 'closed') {
      this.closeDecoder(slot);
      slot.channels = wantChannels;
      slot.decoder = this.openDecoder(slot);
    }
    const decoder = slot.decoder;
    if (!decoder || decoder.state !== 'configured') return;
    if (decoder.decodeQueueSize > 16) {
      this.framesDropped += 1;
      return;
    }
    try {
      decoder.decode(
        new EncodedAudioChunk({
          type: 'key',
          timestamp: Math.round((timestamp * 1_000_000) / AURX_SAMPLE_RATE),
          data: frame as Uint8Array<ArrayBuffer>,
        }),
      );
    } catch (e) {
      this.framesDropped += 1;
      this.onError(e instanceof Error ? e : new Error(String(e)));
    }
  }

  /** G.711 is decoded here (no WebCodecs decoder exists for it): table lookup, then 8 → 48 kHz. */
  private pushG711(slot: PlaybackSlot, law: G711Law, frame: Uint8Array): void {
    const narrow = decodeG711(law, frame);
    if (!narrow) {
      this.framesDropped += 1;
      return;
    }
    slot.g711 ??= new G711Upsampler(AURX_SAMPLE_RATE / G711_SAMPLE_RATE);
    const wide = slot.g711.process(narrow);
    this.framesDecoded += 1;
    slot.node.port.postMessage({ type: 'pcm', planes: [wide] }, [wide.buffer]);
  }

  remove(ssrc: number): void {
    const slot = this.slots.get(ssrc);
    if (!slot) return;
    this.slots.delete(ssrc);
    this.closeDecoder(slot);
    slot.node.port.onmessage = null;
    slot.node.port.close();
    slot.node.disconnect();
  }

  clear(): void {
    for (const ssrc of Array.from(this.slots.keys())) this.remove(ssrc);
  }

  private openDecoder(slot: PlaybackSlot): AudioDecoder | undefined {
    const decoder = new AudioDecoder({
      output: (audio) => {
        if (slot.decoder !== decoder) {
          audio.close();
          return;
        }
        this.framesDecoded += 1;
        const planes: Float32Array[] = [];
        try {
          for (let c = 0; c < audio.numberOfChannels; c++) {
            const plane = new Float32Array(audio.numberOfFrames);
            audio.copyTo(plane, { planeIndex: c, format: 'f32-planar' });
            planes.push(plane);
          }
        } finally {
          audio.close();
        }
        slot.node.port.postMessage({ type: 'pcm', planes }, planes.map((p) => p.buffer));
      },
      error: (e) => {
        if (slot.decoder !== decoder) return;
        this.onError(e instanceof Error ? e : new Error(String(e)));
        this.closeDecoder(slot);
      },
    });
    try {
      decoder.configure({ codec: 'opus', sampleRate: AURX_SAMPLE_RATE, numberOfChannels: slot.channels });
    } catch (e) {
      this.onError(e instanceof Error ? e : new Error(String(e)));
      return undefined;
    }
    return decoder;
  }

  private closeDecoder(slot: PlaybackSlot): void {
    const d = slot.decoder;
    slot.decoder = undefined;
    if (!d) return;
    try {
      if (d.state !== 'closed') d.close();
    } catch {
      // already closed
    }
  }
}
