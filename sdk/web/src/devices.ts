/**
 * Audio device helpers: enumeration, output-device support detection and the optional
 * Web Audio input pipeline (software gain) placed between the microphone and the uplink.
 */

export interface AudioDeviceInfo {
  deviceId: string;
  groupId: string;
  /** Empty until the user granted microphone permission (browser privacy rule). */
  label: string;
  kind: 'audioinput' | 'audiooutput';
}

export interface AudioDevices {
  inputs: AudioDeviceInfo[];
  outputs: AudioDeviceInfo[];
}

/**
 * List microphones and speakers. Labels are only available after `getUserMedia` succeeded
 * once (i.e. after `connect()`); outputs are empty on browsers without `selectAudioOutput` /
 * `setSinkId` support (Safari, Firefox without the pref).
 */
export async function enumerateAudioDevices(): Promise<AudioDevices> {
  const out: AudioDevices = { inputs: [], outputs: [] };
  if (typeof navigator === 'undefined' || !navigator.mediaDevices?.enumerateDevices) return out;
  const all = await navigator.mediaDevices.enumerateDevices();
  for (const d of all) {
    if (d.kind !== 'audioinput' && d.kind !== 'audiooutput') continue;
    const info: AudioDeviceInfo = { deviceId: d.deviceId, groupId: d.groupId, label: d.label, kind: d.kind };
    (d.kind === 'audioinput' ? out.inputs : out.outputs).push(info);
  }
  return out;
}

/** `true` when `HTMLMediaElement.setSinkId` exists, i.e. the speaker can be chosen per element. */
export function supportsOutputSelection(): boolean {
  return typeof HTMLMediaElement !== 'undefined' && 'setSinkId' in HTMLMediaElement.prototype;
}

/** Largest software input gain accepted by {@link InputPipeline} / `AurixClient.setInputGain`. */
export const MAX_INPUT_GAIN = 4;

/** What to play into the uplink besides (or instead of) the microphone. */
export type AudioInjectionSource = AudioBuffer | MediaStream;

export interface AudioInjectionOptions {
  /** Restart the buffer when it ends (buffers only; default `false`). */
  loop?: boolean;
  /** Linear gain of the injected signal, `0..MAX_INPUT_GAIN` (default `1`). */
  gain?: number;
  /**
   * Keep the microphone audible underneath the injected audio (default `true`). `false`
   * silences the microphone for as long as the injection plays (sound test, bot voice).
   */
  mixWithMicrophone?: boolean;
}

/**
 * Microphone → `GainNode` (input gain) → `GainNode` (mic switch) → `MediaStreamAudioDestinationNode`,
 * with an optional injected source → `GainNode` summed into the same destination. The
 * destination's track is what the peer connection sends, so the device can be swapped
 * ({@link setSource}) without renegotiating and gain changes take effect immediately.
 */
export class InputPipeline {
  private ctx: AudioContext | undefined;
  private source: MediaStreamAudioSourceNode | undefined;
  private gain: GainNode | undefined;
  private micSwitch: GainNode | undefined;
  private injectGain: GainNode | undefined;
  private injectNode: AudioBufferSourceNode | MediaStreamAudioSourceNode | undefined;
  private destination: MediaStreamAudioDestinationNode | undefined;
  private readonly ownsContext: boolean;
  private target = 1;
  /** Called when an injected buffer plays to its end (not on {@link stopInjection}). */
  onInjectionEnded: (() => void) | undefined;

  constructor(audioContext?: AudioContext) {
    this.ctx = audioContext;
    this.ownsContext = !audioContext;
  }

  /** The `AudioContext` in use once {@link open} succeeded (share it with the level meter). */
  get audioContext(): AudioContext | undefined {
    return this.ctx;
  }

  /** Processed stream to send; `undefined` before {@link open}. */
  get stream(): MediaStream | undefined {
    return this.destination?.stream;
  }

  get value(): number {
    return this.target;
  }

  get injecting(): boolean {
    return this.injectNode !== undefined;
  }

  /** Build the graph on `raw`. Returns `false` when Web Audio is unavailable. */
  open(raw: MediaStream, gain = 1): boolean {
    if (this.destination) {
      this.setSource(raw);
      this.setGain(gain);
      return true;
    }
    if (typeof AudioContext === 'undefined') return false;
    try {
      this.ctx ??= new AudioContext();
      this.gain = this.ctx.createGain();
      this.micSwitch = this.ctx.createGain();
      this.injectGain = this.ctx.createGain();
      this.destination = this.ctx.createMediaStreamDestination();
      this.gain.connect(this.micSwitch);
      this.micSwitch.connect(this.destination);
      this.injectGain.connect(this.destination);
      this.setSource(raw);
      this.setGain(gain);
    } catch {
      this.close();
      return false;
    }
    if (this.ctx.state === 'suspended') void this.ctx.resume().catch(() => undefined);
    return true;
  }

  /** Feed a different microphone stream into the same output track. */
  setSource(raw: MediaStream): void {
    if (!this.ctx || !this.gain) return;
    this.source?.disconnect();
    this.source = this.ctx.createMediaStreamSource(raw);
    this.source.connect(this.gain);
  }

  /** Linear gain `0..MAX_INPUT_GAIN` (`1` = unity), ramped over ~20 ms to avoid clicks. */
  setGain(gain: number): void {
    if (!this.gain || !this.ctx) return;
    const g = Math.min(MAX_INPUT_GAIN, Math.max(0, gain));
    this.target = g;
    const param = this.gain.gain;
    param.cancelScheduledValues(this.ctx.currentTime);
    param.setTargetAtTime(g, this.ctx.currentTime, 0.005);
  }

  /**
   * Play `source` into the uplink (replacing a previous injection). Throws when the pipeline
   * is not open.
   */
  inject(source: AudioInjectionSource, options: AudioInjectionOptions = {}): void {
    const ctx = this.ctx;
    if (!ctx || !this.injectGain || !this.micSwitch) throw new Error('input pipeline is not open');
    this.stopInjection();
    const g = Math.min(MAX_INPUT_GAIN, Math.max(0, options.gain ?? 1));
    this.injectGain.gain.cancelScheduledValues(ctx.currentTime);
    this.injectGain.gain.setValueAtTime(g, ctx.currentTime);
    let node: AudioBufferSourceNode | MediaStreamAudioSourceNode;
    if (source instanceof AudioBuffer) {
      const buffer = ctx.createBufferSource();
      buffer.buffer = source;
      buffer.loop = options.loop ?? false;
      buffer.onended = () => {
        if (this.injectNode !== buffer) return;
        this.teardownInjection();
        this.onInjectionEnded?.();
      };
      buffer.connect(this.injectGain);
      buffer.start();
      node = buffer;
    } else {
      node = ctx.createMediaStreamSource(source);
      node.connect(this.injectGain);
    }
    this.injectNode = node;
    this.setMicSwitch((options.mixWithMicrophone ?? true) ? 1 : 0);
    if (ctx.state === 'suspended') void ctx.resume().catch(() => undefined);
  }

  /** Stop the current injection and restore the microphone. Returns `false` when idle. */
  stopInjection(): boolean {
    if (!this.injectNode) return false;
    this.teardownInjection();
    return true;
  }

  private teardownInjection(): void {
    const node = this.injectNode;
    this.injectNode = undefined;
    if (!node) return;
    if (node instanceof AudioBufferSourceNode) {
      node.onended = null;
      try {
        node.stop();
      } catch {
        // already stopped
      }
    }
    node.disconnect();
    this.setMicSwitch(1);
  }

  private setMicSwitch(value: number): void {
    if (!this.micSwitch || !this.ctx) return;
    const param = this.micSwitch.gain;
    param.cancelScheduledValues(this.ctx.currentTime);
    param.setTargetAtTime(value, this.ctx.currentTime, 0.005);
  }

  close(): void {
    this.teardownInjection();
    this.source?.disconnect();
    this.gain?.disconnect();
    this.micSwitch?.disconnect();
    this.injectGain?.disconnect();
    this.destination?.stream.getTracks().forEach((t) => t.stop());
    this.source = undefined;
    this.gain = undefined;
    this.micSwitch = undefined;
    this.injectGain = undefined;
    this.destination = undefined;
    if (this.ownsContext && this.ctx) void this.ctx.close().catch(() => undefined);
    if (this.ownsContext) this.ctx = undefined;
  }
}
