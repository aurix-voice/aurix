/**
 * Client-side level metering / voice activity detection on top of Web Audio.
 *
 * Remote participants' energy comes from the server (`ChannelEnergy` events, derived from the
 * RTP audio-level extension browsers attach to every packet). The local microphone is metered
 * here so the UI can show the user's own level and speaking state without a round trip.
 */

export interface AudioLevelMeterOptions {
  /** Linear RMS threshold (0..1) above which a frame counts as speech. Default 0.01 (≈ -40 dBov). */
  threshold?: number;
  /** Quiet time (ms) before speech is considered over. Default 300. */
  hangoverMs?: number;
  /** Sampling period (ms) of the meter. Default 50. */
  intervalMs?: number;
  /** Smoothing factor for {@link AudioLevelMeter.energy} (0 = raw, 1 = frozen). Default 0.5. */
  smoothing?: number;
  /** Reuse an existing `AudioContext` (otherwise one is created and closed with the meter). */
  audioContext?: AudioContext;
}

export interface AudioLevelSample {
  /** Smoothed linear RMS 0..1. */
  energy: number;
  /** Raw RMS of the last window. */
  rms: number;
  speaking: boolean;
  /** `true` on the sample where `speaking` flipped. */
  changed: boolean;
}

/** Wire/serialised audio level (RFC 6464 style): `0` = full scale, `127` = silence, else `-dBov`. */
export const AUDIO_LEVEL_SILENCE = 127;

export function encodeAudioLevel(energy: number): number {
  if (!(energy > 0)) return AUDIO_LEVEL_SILENCE;
  const dbov = -20 * Math.log10(energy);
  return Math.round(Math.min(AUDIO_LEVEL_SILENCE, Math.max(0, dbov)));
}

export function decodeAudioLevel(level: number): number {
  if (level >= AUDIO_LEVEL_SILENCE) return 0;
  return Math.pow(10, -level / 20);
}

export function rms(samples: Float32Array): number {
  if (samples.length === 0) return 0;
  let sum = 0;
  for (let i = 0; i < samples.length; i++) sum += samples[i]! * samples[i]!;
  return Math.sqrt(sum / samples.length);
}

/**
 * Pure energy VAD with hangover, usable on any RMS series (the meter below drives it with
 * Web Audio; tests and non-browser hosts can feed it directly).
 */
export class VoiceActivityDetector {
  threshold: number;
  hangoverMs: number;
  smoothing: number;
  energy = 0;
  speaking = false;
  private quietSinceMs: number | undefined;

  constructor(opts: Pick<AudioLevelMeterOptions, 'threshold' | 'hangoverMs' | 'smoothing'> = {}) {
    this.threshold = opts.threshold ?? 0.01;
    this.hangoverMs = opts.hangoverMs ?? 300;
    this.smoothing = opts.smoothing ?? 0.5;
  }

  /** Feed one RMS measurement taken at `nowMs`. */
  process(rmsValue: number, nowMs: number): AudioLevelSample {
    const s = Math.min(1, Math.max(0, this.smoothing));
    this.energy = this.energy * s + rmsValue * (1 - s);
    const was = this.speaking;
    if (rmsValue >= this.threshold) {
      this.quietSinceMs = undefined;
      this.speaking = true;
    } else if (this.speaking) {
      this.quietSinceMs ??= nowMs;
      if (nowMs - this.quietSinceMs >= this.hangoverMs) this.speaking = false;
    }
    return { energy: this.energy, rms: rmsValue, speaking: this.speaking, changed: was !== this.speaking };
  }

  reset(): void {
    this.energy = 0;
    this.speaking = false;
    this.quietSinceMs = undefined;
  }
}

/**
 * Meters a `MediaStream` (microphone or a remote stream) with an `AnalyserNode` and reports
 * samples at a fixed interval. `start()` is a no-op where Web Audio is unavailable.
 */
export class AudioLevelMeter {
  readonly vad: VoiceActivityDetector;
  private readonly intervalMs: number;
  private readonly ownsContext: boolean;
  private ctx: AudioContext | undefined;
  private source: MediaStreamAudioSourceNode | undefined;
  private analyser: AnalyserNode | undefined;
  private buffer: Float32Array<ArrayBuffer> | undefined;
  private timer: ReturnType<typeof setInterval> | undefined;

  constructor(
    private readonly stream: MediaStream,
    private readonly onSample: (sample: AudioLevelSample) => void,
    opts: AudioLevelMeterOptions = {},
  ) {
    this.vad = new VoiceActivityDetector(opts);
    this.intervalMs = Math.max(10, opts.intervalMs ?? 50);
    this.ctx = opts.audioContext;
    this.ownsContext = !opts.audioContext;
  }

  get running(): boolean {
    return this.timer !== undefined;
  }

  get energy(): number {
    return this.vad.energy;
  }

  get speaking(): boolean {
    return this.vad.speaking;
  }

  start(): boolean {
    if (this.timer) return true;
    if (typeof AudioContext === 'undefined') return false;
    if (this.stream.getAudioTracks().length === 0) return false;
    try {
      this.ctx ??= new AudioContext();
      this.source = this.ctx.createMediaStreamSource(this.stream);
      this.analyser = this.ctx.createAnalyser();
      this.analyser.fftSize = 1024;
      this.source.connect(this.analyser);
      this.buffer = new Float32Array(this.analyser.fftSize);
    } catch {
      this.release();
      return false;
    }
    if (this.ctx.state === 'suspended') void this.ctx.resume().catch(() => undefined);
    this.timer = setInterval(() => this.tick(), this.intervalMs);
    return true;
  }

  stop(): void {
    if (this.timer) clearInterval(this.timer);
    this.timer = undefined;
    this.release();
    this.vad.reset();
  }

  private tick(): void {
    if (!this.analyser || !this.buffer) return;
    this.analyser.getFloatTimeDomainData(this.buffer);
    // A disabled (muted) track yields silence, so the meter naturally drops to 0.
    this.onSample(this.vad.process(rms(this.buffer), performance.now()));
  }

  private release(): void {
    this.source?.disconnect();
    this.analyser?.disconnect();
    this.source = undefined;
    this.analyser = undefined;
    this.buffer = undefined;
    if (this.ownsContext && this.ctx) {
      void this.ctx.close().catch(() => undefined);
      this.ctx = undefined;
    }
  }
}
