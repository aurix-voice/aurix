/**
 * Browser-side rendering of per-participant downlink tracks.
 *
 * The SFU can hand a browser, next to the mixed track, one Opus track per audible participant
 * (`ParticipantStreams` tells which `mid` carries whom). Those frames arrive as sent — no
 * server gain — so the browser applies what the server would have: receiver volume, mute,
 * block, channel focus and, in positional channels, distance attenuation and direction. The
 * math mirrors the server's (`PositionalConfig`, `Direction::from_listener`) so a listener
 * hears the same loudness whether a voice comes on its own track or in the mix; direction is
 * rendered with a `PannerNode` (HRTF by default) instead of the mix's stereo pan.
 */
import type { Orientation3D, Position3D, PositionalConfigWire } from './protocol.js';

/** Where a source sits in the listener's frame: radians, `0` ahead, positive right / above. */
export interface Direction {
  azimuth: number;
  elevation: number;
}

/** Everything that decides how one participant's track is heard. */
export interface RenderInputs {
  /** Receiver-local gain `0..2` (`setParticipantVolume`). */
  volume: number;
  /** Locally muted in every shared channel, or cross-muted: silence. */
  silenced: boolean;
  /** `1`, or the node's unfocused-channel gain when another channel holds the focus. */
  focusFactor: number;
  /**
   * Priority-speaker ducking currently applying to this voice (`DuckingConfig.gain` of the
   * channel whose priority member speaks); absent / `1` = not ducked.
   */
  ducking?: DuckParams;
  /** Present when the speaker shares a positional channel with the listener and both positions are known. */
  positional?: {
    config: PositionalConfigWire;
    listener: Position3D;
    orientation: Orientation3D;
    source: Position3D;
  };
}

export interface RenderParams {
  /** Linear gain to apply; `0` = inaudible. */
  gain: number;
  /** Direction to pan towards; `undefined` = no spatialisation (centred, unprocessed). */
  direction?: Direction;
  /** Ducking stage: target gain plus the ramps (`undefined` = ramp back to unity with `releaseMs`). */
  ducking?: DuckParams;
}

/** Ducking applied on top of the receiver gains; kept a separate stage so its ramps are its own. */
export interface DuckParams {
  /** `0..1`; `1` = not ducked. */
  gain: number;
  attackMs: number;
  releaseMs: number;
}

/** Below this the server drops the frame, so the browser treats the voice as inaudible too. */
const AUDIBLE_GAIN = 0.001;

export function renderParams(inputs: RenderInputs): RenderParams {
  // Ducking survives silence so a voice unmuted / walking back into range mid-duck does not pump.
  const ducking = inputs.ducking && inputs.ducking.gain < 1 ? inputs.ducking : undefined;
  const silent = (): RenderParams => (ducking ? { gain: 0, ducking } : { gain: 0 });
  if (inputs.silenced || !(inputs.volume > 0)) return silent();
  let gain = inputs.volume * inputs.focusFactor;
  let direction: Direction | undefined;
  const p = inputs.positional;
  if (p) {
    const d = distance(p.source, p.listener);
    gain *= distanceGain(p.config, d);
    if (gain > AUDIBLE_GAIN && p.config.directional) {
      direction = directionFromListener(p.listener, p.orientation, p.source, p.config.coordinate_system) ?? {
        azimuth: 0,
        elevation: 0,
      };
    }
  }
  if (!(gain > AUDIBLE_GAIN)) return silent();
  const out: RenderParams = { gain };
  if (direction) out.direction = direction;
  if (ducking) out.ducking = ducking;
  return out;
}

export function distance(a: Position3D, b: Position3D): number {
  return Math.hypot(a.x - b.x, a.y - b.y, a.z - b.z);
}

/** The server's distance rolloff: unity within `near_distance`, silent from `far_distance` / beyond `max_radius`. */
export function distanceGain(cfg: PositionalConfigWire, d: number): number {
  if (!(d >= 0)) return 1;
  if (d > cfg.max_radius) return 0;
  if (d <= cfg.near_distance) return 1;
  if (d >= cfg.far_distance) return 0;
  const span = cfg.far_distance - cfg.near_distance;
  switch (cfg.rolloff) {
    case 'linear':
      return 1 - (d - cfg.near_distance) / span;
    case 'logarithmic':
      return Math.min(1, Math.max(0, cfg.near_distance / d));
    case 'custom_spline': {
      const t = Math.min(1, Math.max(0, (d - cfg.near_distance) / span));
      return 1 - t * t * t;
    }
    default:
      return Math.min(1, Math.max(0, cfg.near_distance / d));
  }
}

type Vec3 = [number, number, number];

const dot = (a: Vec3, b: Vec3): number => a[0] * b[0] + a[1] * b[1] + a[2] * b[2];
const cross = (a: Vec3, b: Vec3): Vec3 => [
  a[1] * b[2] - a[2] * b[1],
  a[2] * b[0] - a[0] * b[2],
  a[0] * b[1] - a[1] * b[0],
];
function normalize(v: Vec3): Vec3 | undefined {
  const len = Math.sqrt(dot(v, v));
  if (!Number.isFinite(len) || len <= 1e-6) return undefined;
  return [v[0] / len, v[1] / len, v[2] / len];
}

/**
 * Direction from `listener` (facing `orientation`) to `source` in the game's coordinate
 * system; `undefined` when they coincide or the orientation is degenerate.
 */
export function directionFromListener(
  listener: Position3D,
  orientation: Orientation3D,
  source: Position3D,
  coords: PositionalConfigWire['coordinate_system'],
): Direction | undefined {
  const f = normalize([orientation.forward_x, orientation.forward_y, orientation.forward_z]);
  if (!f) return undefined;
  const up: Vec3 = [orientation.up_x, orientation.up_y, orientation.up_z];
  const k = dot(up, f);
  const u = normalize([up[0] - k * f[0], up[1] - k * f[1], up[2] - k * f[2]]);
  if (!u) return undefined;
  const r = coords === 'right_handed' ? cross(f, u) : cross(u, f);
  const d = normalize([source.x - listener.x, source.y - listener.y, source.z - listener.z]);
  if (!d) return undefined;
  const azimuth = Math.atan2(dot(d, r), dot(d, f));
  const elevation = Math.asin(Math.min(1, Math.max(-1, dot(d, u))));
  return { azimuth, elevation };
}

/**
 * Unit position of a source for a `PannerNode` whose listener sits at the origin facing `-Z`
 * with `+Y` up (Web Audio's right-handed frame): `+X` is the listener's right.
 */
export function pannerPosition(dir: Direction): Vec3 {
  const ce = Math.cos(dir.elevation);
  return [Math.sin(dir.azimuth) * ce, Math.sin(dir.elevation), -Math.cos(dir.azimuth) * ce];
}

// ── Web Audio graph ──

/** The slice of Web Audio the renderer touches; a real `AudioContext` satisfies it, tests fake it. */
export interface AudioParamLike {
  value: number;
  setTargetAtTime(target: number, startTime: number, timeConstant: number): unknown;
}
export interface AudioNodeLike {
  connect(destination: AudioNodeLike): unknown;
  disconnect(destination?: AudioNodeLike): void;
}
export interface GainNodeLike extends AudioNodeLike {
  gain: AudioParamLike;
}
export interface PannerNodeLike extends AudioNodeLike {
  panningModel: PanningModelType;
  distanceModel: DistanceModelType;
  refDistance: number;
  maxDistance: number;
  rolloffFactor: number;
  positionX: AudioParamLike;
  positionY: AudioParamLike;
  positionZ: AudioParamLike;
}
export interface SpatialAudioContextLike {
  readonly currentTime: number;
  readonly state: AudioContextState;
  readonly destination: AudioNodeLike;
  createGain(): GainNodeLike;
  createPanner(): PannerNodeLike;
  createMediaStreamSource(stream: MediaStream): AudioNodeLike;
  createMediaStreamDestination?(): AudioNodeLike & { readonly stream: MediaStream };
  resume(): Promise<void>;
  close(): Promise<void>;
  setSinkId?(sinkId: string): Promise<void>;
}

export interface SpatialRendererOptions {
  /** `HRTF` (default) or `equalpower`. */
  panningModel?: PanningModelType;
  /**
   * Document used to create a hidden, muted `<audio>` per track: Chromium only decodes a
   * remote WebRTC track once some media element consumes it. `null` skips that.
   */
  document?: Document | null;
}

interface Slot {
  stream: MediaStream | undefined;
  source: AudioNodeLike;
  gain: GainNodeLike;
  duck: GainNodeLike;
  duckTarget: number;
  /** Release ramp of the ducking last applied, for the ramp back once ducking is gone. */
  lastRelease: number | undefined;
  panner: PannerNodeLike | undefined;
  spatial: boolean;
  keepAlive: HTMLAudioElement | undefined;
  taps: Set<AudioNodeLike>;
}

/** Time constant of gain / position ramps (s): fast enough to follow movement, no zipper noise. */
const RAMP_TC = 0.02;
/** `setTargetAtTime` reaches ~95 % of the target after three time constants. */
const RAMP_SETTLE = 3;

/**
 * One Web Audio graph per client: `source → gain → duck → [panner] → master → destination`, a slot
 * per negotiated `mid`. Slots are keyed by `mid` because the participant on a track changes
 * while the track stays; the client re-applies the new participant's parameters.
 */
export class SpatialRenderer {
  private readonly master: GainNodeLike;
  private readonly slots = new Map<string, Slot>();
  private masterVolume = 1;
  private masterMuted = false;
  private streamOut: (AudioNodeLike & { readonly stream: MediaStream }) | undefined;
  private readonly panningModel: PanningModelType;
  private readonly document: Document | null;

  constructor(
    readonly context: SpatialAudioContextLike,
    options: SpatialRendererOptions = {},
  ) {
    this.panningModel = options.panningModel ?? 'HRTF';
    this.document =
      options.document !== undefined ? options.document : typeof document === 'undefined' ? null : document;
    this.master = context.createGain();
    this.master.connect(context.destination);
  }

  /** `mid`s with a live track. */
  get mids(): string[] {
    return Array.from(this.slots.keys());
  }

  has(mid: string): boolean {
    return this.slots.has(mid);
  }

  /** Start rendering `stream` (one remote audio track) on slot `mid`, initially silent. */
  addTrack(mid: string, stream: MediaStream): void {
    this.addSlot(mid, this.context.createMediaStreamSource(stream), stream);
  }

  /** Start rendering an already-decoded voice (`source` node of this context) on slot `mid`. */
  addSource(mid: string, source: AudioNodeLike): void {
    this.addSlot(mid, source, undefined);
  }

  private addSlot(mid: string, source: AudioNodeLike, stream: MediaStream | undefined): void {
    this.removeTrack(mid);
    const gain = this.context.createGain();
    gain.gain.value = 0;
    const duck = this.context.createGain();
    duck.gain.value = 1;
    source.connect(gain);
    gain.connect(duck);
    duck.connect(this.master);
    let keepAlive: HTMLAudioElement | undefined;
    if (this.document && stream) {
      keepAlive = this.document.createElement('audio');
      keepAlive.muted = true;
      keepAlive.autoplay = true;
      keepAlive.srcObject = stream;
      void keepAlive.play().catch(() => undefined);
    }
    this.slots.set(mid, {
      stream,
      source,
      gain,
      duck,
      duckTarget: 1,
      lastRelease: undefined,
      panner: undefined,
      spatial: false,
      keepAlive,
      taps: new Set(),
    });
  }

  removeTrack(mid: string): void {
    const slot = this.slots.get(mid);
    if (!slot) return;
    this.slots.delete(mid);
    slot.source.disconnect();
    slot.gain.disconnect();
    slot.duck.disconnect();
    slot.panner?.disconnect();
    for (const tap of slot.taps) tap.disconnect();
    if (slot.keepAlive) {
      slot.keepAlive.srcObject = null;
    }
  }

  /**
   * Feed slot `mid`'s raw decoded audio (before any receiver gain) into `node` as well — a
   * viseme analyser, a meter. Removed with the slot or {@link removeTap}.
   */
  addTap(mid: string, node: AudioNodeLike): boolean {
    const slot = this.slots.get(mid);
    if (!slot) return false;
    slot.source.connect(node);
    slot.taps.add(node);
    return true;
  }

  removeTap(mid: string, node: AudioNodeLike): void {
    const slot = this.slots.get(mid);
    if (!slot || !slot.taps.delete(node)) return;
    slot.source.disconnect(node);
  }

  /** Drop every slot (media torn down); the context stays usable. */
  clear(): void {
    for (const mid of Array.from(this.slots.keys())) this.removeTrack(mid);
  }

  /**
   * Route the whole graph into a `MediaStream` instead of the context's output (the caller
   * plays it through media elements, which then own volume, mute and the output device); the
   * master gain stays at unity while this is active. `undefined` when the context cannot.
   */
  outputStream(): MediaStream | undefined {
    if (this.streamOut) return this.streamOut.stream;
    if (typeof this.context.createMediaStreamDestination !== 'function') return undefined;
    const dest = this.context.createMediaStreamDestination();
    this.master.disconnect();
    this.master.connect(dest);
    this.streamOut = dest;
    this.applyMaster();
    return dest.stream;
  }

  /** Back to the context's output after {@link outputStream}. */
  outputSpeakers(): void {
    if (!this.streamOut) return;
    this.master.disconnect();
    this.master.connect(this.context.destination);
    this.streamOut = undefined;
    this.applyMaster();
  }

  /** Apply gain and direction to slot `mid`; a missing direction routes the voice around the panner. */
  render(mid: string, params: RenderParams): void {
    const slot = this.slots.get(mid);
    if (!slot) return;
    const now = this.context.currentTime;
    slot.gain.gain.setTargetAtTime(params.gain, now, RAMP_TC);
    const duckTarget = params.ducking?.gain ?? 1;
    if (duckTarget !== slot.duckTarget) {
      const rampMs = params.ducking
        ? duckTarget < slot.duckTarget
          ? params.ducking.attackMs
          : params.ducking.releaseMs
        : slot.lastRelease ?? 0;
      slot.duckTarget = duckTarget;
      slot.duck.gain.setTargetAtTime(duckTarget, now, Math.max(RAMP_TC, rampMs / 1000 / RAMP_SETTLE));
    }
    if (params.ducking) slot.lastRelease = params.ducking.releaseMs;
    const wantSpatial = params.direction !== undefined && params.gain > 0;
    if (wantSpatial !== slot.spatial) {
      slot.duck.disconnect();
      if (wantSpatial) {
        slot.panner ??= this.createPanner();
        slot.duck.connect(slot.panner);
      } else {
        slot.duck.connect(this.master);
      }
      slot.spatial = wantSpatial;
    }
    if (wantSpatial && params.direction && slot.panner) {
      const [x, y, z] = pannerPosition(params.direction);
      slot.panner.positionX.setTargetAtTime(x, now, RAMP_TC);
      slot.panner.positionY.setTargetAtTime(y, now, RAMP_TC);
      slot.panner.positionZ.setTargetAtTime(z, now, RAMP_TC);
    }
  }

  /** Whether slot `mid` currently goes through the panner. */
  isSpatial(mid: string): boolean {
    return this.slots.get(mid)?.spatial === true;
  }

  setMasterVolume(volume: number): void {
    this.masterVolume = volume;
    this.applyMaster();
  }

  setMasterMuted(muted: boolean): void {
    this.masterMuted = muted;
    this.applyMaster();
  }

  /** Route the graph to an output device (`AudioContext.setSinkId`, where supported). */
  async setSinkId(deviceId: string | undefined): Promise<void> {
    if (typeof this.context.setSinkId !== 'function') return;
    await this.context.setSinkId(deviceId ?? '');
  }

  /** Autoplay policy: resume the context after a user gesture. */
  async resume(): Promise<boolean> {
    if (this.context.state === 'running') return true;
    try {
      await this.context.resume();
    } catch {
      return false;
    }
    return (this.context.state as AudioContextState) === 'running';
  }

  async close(): Promise<void> {
    this.clear();
    this.master.disconnect();
    try {
      await this.context.close();
    } catch {
      // already closed
    }
  }

  private applyMaster(): void {
    const v = this.streamOut ? 1 : this.masterMuted ? 0 : this.masterVolume;
    this.master.gain.setTargetAtTime(v, this.context.currentTime, RAMP_TC);
  }

  private createPanner(): PannerNodeLike {
    const p = this.context.createPanner();
    p.panningModel = this.panningModel;
    // Distance is applied by the gain node with the server's curve; the panner only pans.
    p.distanceModel = 'linear';
    p.refDistance = 1;
    p.maxDistance = 10_000;
    p.rolloffFactor = 0;
    p.connect(this.master);
    return p;
  }
}

/** `AudioContext` of the page when Web Audio exists; `undefined` otherwise (tests, workers). */
export function createAudioContext(): SpatialAudioContextLike | undefined {
  const Ctor = (globalThis as { AudioContext?: new () => AudioContext }).AudioContext;
  if (!Ctor) return undefined;
  try {
    return new Ctor() as unknown as SpatialAudioContextLike;
  } catch {
    return undefined;
  }
}
