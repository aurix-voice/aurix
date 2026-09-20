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
}

/** Below this the server drops the frame, so the browser treats the voice as inaudible too. */
const AUDIBLE_GAIN = 0.001;

export function renderParams(inputs: RenderInputs): RenderParams {
  if (inputs.silenced || !(inputs.volume > 0)) return { gain: 0 };
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
  if (!(gain > AUDIBLE_GAIN)) return { gain: 0 };
  return direction ? { gain, direction } : { gain };
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
  disconnect(): void;
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
  stream: MediaStream;
  source: AudioNodeLike;
  gain: GainNodeLike;
  panner: PannerNodeLike | undefined;
  spatial: boolean;
  keepAlive: HTMLAudioElement | undefined;
}

/** Time constant of gain / position ramps (s): fast enough to follow movement, no zipper noise. */
const RAMP_TC = 0.02;

/**
 * One Web Audio graph per client: `source → gain → [panner] → master → destination`, a slot
 * per negotiated `mid`. Slots are keyed by `mid` because the participant on a track changes
 * while the track stays; the client re-applies the new participant's parameters.
 */
export class SpatialRenderer {
  private readonly master: GainNodeLike;
  private readonly slots = new Map<string, Slot>();
  private masterVolume = 1;
  private masterMuted = false;
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
    this.removeTrack(mid);
    const source = this.context.createMediaStreamSource(stream);
    const gain = this.context.createGain();
    gain.gain.value = 0;
    source.connect(gain);
    gain.connect(this.master);
    let keepAlive: HTMLAudioElement | undefined;
    if (this.document) {
      keepAlive = this.document.createElement('audio');
      keepAlive.muted = true;
      keepAlive.autoplay = true;
      keepAlive.srcObject = stream;
      void keepAlive.play().catch(() => undefined);
    }
    this.slots.set(mid, { stream, source, gain, panner: undefined, spatial: false, keepAlive });
  }

  removeTrack(mid: string): void {
    const slot = this.slots.get(mid);
    if (!slot) return;
    this.slots.delete(mid);
    slot.source.disconnect();
    slot.gain.disconnect();
    slot.panner?.disconnect();
    if (slot.keepAlive) {
      slot.keepAlive.srcObject = null;
    }
  }

  /** Drop every slot (media torn down); the context stays usable. */
  clear(): void {
    for (const mid of Array.from(this.slots.keys())) this.removeTrack(mid);
  }

  /** Apply gain and direction to slot `mid`; a missing direction routes the voice around the panner. */
  render(mid: string, params: RenderParams): void {
    const slot = this.slots.get(mid);
    if (!slot) return;
    const now = this.context.currentTime;
    slot.gain.gain.setTargetAtTime(params.gain, now, RAMP_TC);
    const wantSpatial = params.direction !== undefined && params.gain > 0;
    if (wantSpatial !== slot.spatial) {
      slot.gain.disconnect();
      if (wantSpatial) {
        slot.panner ??= this.createPanner();
        slot.gain.connect(slot.panner);
      } else {
        slot.gain.connect(this.master);
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
    const v = this.masterMuted ? 0 : this.masterVolume;
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
