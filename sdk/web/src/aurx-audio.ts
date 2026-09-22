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
 * once `prime` frames are queued; silence on underrun.
 */
function aurxWorkletMain(scope: AudioWorkletScope): void {
  const FRAME = 960;
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
    private readonly cap = FRAME * 12;
    private readonly left: Float32Array;
    private readonly right: Float32Array;
    private readIdx = 0;
    private writeIdx = 0;
    private queued = 0;
    private primed = false;
    private prime = FRAME * 2;
    private underruns = 0;
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
        else if (d.type === 'prime') this.prime = Math.max(FRAME, Math.min(this.cap - FRAME, d.samples as number));
        else if (d.type === 'flush') {
          this.readIdx = this.writeIdx = this.queued = 0;
          this.primed = false;
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
      }
      for (let i = 0; i < n; i++) {
        this.left[this.writeIdx] = l[i] as number;
        this.right[this.writeIdx] = r[i] as number;
        this.writeIdx = (this.writeIdx + 1) % this.cap;
      }
      this.queued += n;
      if (!this.primed && this.queued >= this.prime) this.primed = true;
    }
    process(_inputs: Float32Array[][], outputs: Float32Array[][]): boolean {
      const out = outputs[0];
      if (!out || out.length === 0) return true;
      const ol = out[0] as Float32Array;
      const or = (out[1] ?? out[0]) as Float32Array;
      const n = ol.length;
      if (!this.primed || this.queued < n) {
        if (this.primed) {
          this.underruns++;
          this.primed = false;
          this.port.postMessage({ type: 'underrun', count: this.underruns });
        }
        ol.fill(0);
        if (or !== ol) or.fill(0);
        return true;
      }
      for (let i = 0; i < n; i++) {
        ol[i] = this.left[this.readIdx] as number;
        or[i] = this.right[this.readIdx] as number;
        this.readIdx = (this.readIdx + 1) % this.cap;
      }
      this.queued -= n;
      return true;
    }
  }

  scope.registerProcessor('aurix-aurx-capture', AurixAurxCapture);
  scope.registerProcessor('aurix-aurx-player', AurixAurxPlayer);
}

export function aurxWorkletSource(): string {
  return `'use strict';\n${aurxWorkletMain.toString()}\naurxWorkletMain(globalThis);\n`;
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
}

export interface AurxPlaybackStats {
  slots: number;
  framesDecoded: number;
  framesDropped: number;
  underruns: number;
}

/**
 * Decoded playback per downlink SSRC on a shared `AudioContext` (the spatial renderer's).
 * {@link node} of a slot is what the renderer connects; the caller places it.
 */
export class AurxPlayback {
  private readonly slots = new Map<number, PlaybackSlot>();
  private framesDecoded = 0;
  private framesDropped = 0;
  /** Playout buffer before a slot starts, in frames (default 2 = 40 ms). */
  primeFrames = 2;

  constructor(
    readonly context: BaseAudioContext,
    private readonly onError: (error: Error) => void,
  ) {}

  async init(): Promise<void> {
    await loadAurxWorklet(this.context);
  }

  get stats(): AurxPlaybackStats {
    let underruns = 0;
    for (const s of this.slots.values()) underruns += s.underruns;
    return { slots: this.slots.size, framesDecoded: this.framesDecoded, framesDropped: this.framesDropped, underruns };
  }

  has(ssrc: number): boolean {
    return this.slots.has(ssrc);
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
    node.port.postMessage({ type: 'prime', samples: this.primeFrames * AURX_FRAME_SAMPLES });
    const slot: PlaybackSlot = { node, decoder: undefined, channels: stereo ? 2 : 1, lastSeq: undefined, underruns: 0 };
    node.port.onmessage = (ev: MessageEvent<{ type: string; count?: number }>) => {
      if (ev.data?.type === 'underrun') slot.underruns = ev.data.count ?? slot.underruns + 1;
    };
    this.slots.set(ssrc, slot);
    return node;
  }

  /** Decode one Opus frame of `ssrc` and queue it for playback. */
  push(ssrc: number, frame: Uint8Array, sequence: number, timestamp: number, stereo: boolean): void {
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
