import {
  AURIX_SUBPROTOCOL,
  BEARER_SUBPROTOCOL_PREFIX,
  MAX_PARTICIPANT_VOLUME,
  RESUME_SUBPROTOCOL_PREFIX,
  parseServerMessage,
  SYSTEM_USER_ID,
  type ChatMessageWire,
  type ClientMessage,
  type JsonValue,
  type LocalMute,
  type ModerationAction,
  type ParticipantBrief,
  type ParticipantEnergy,
  type ParticipantVolume,
  type RecordingConsent,
  type ServerMessage,
  type TransmissionModeWire,
  type TurnCredentials,
  type UnknownMessage,
  type UserPosition,
} from './protocol.js';
import { AudioLevelMeter, type AudioLevelMeterOptions, type AudioLevelSample } from './audio.js';
import {
  InputPipeline,
  MAX_INPUT_GAIN,
  enumerateAudioDevices,
  supportsOutputSelection,
  type AudioDevices,
} from './devices.js';

export interface AurixClientOptions {
  /** REST base URL, e.g. `https://voice.example.com` (used for TURN credentials). */
  apiUrl: string;
  /** WebSocket URL of the control channel, e.g. `wss://voice.example.com/ws`. */
  wsUrl: string;
  /**
   * Player credential issued by your backend: a session JWT (`POST /v1/tokens`) or a one-time
   * `login` action token (`POST /v1/tokens/action`). Also used for `GET /v1/me/*` calls (TURN
   * credentials are fetched right after the session opens, within a login token's TTL).
   */
  token: string;
  /**
   * Called before every reconnect attempt; return a fresh credential for `token`. Required
   * when `token` is a one-time `login` action token: it is spent by the first connection (and
   * expires within its short TTL), so a fresh session after the resume window needs a new one.
   */
  refreshToken?: () => Promise<string>;
  /**
   * Called by `joinChannel` when no explicit join token is passed; return a one-time `join`
   * action token for `channelId`. Required when the server enforces
   * `auth.require_action_tokens`. Without it, `token` itself authorises the join.
   */
  joinToken?: (channelId: string) => Promise<string>;
  /**
   * Fetch TURN credentials from `GET /v1/me/turn-credentials` and add them to the ICE
   * configuration. Defaults to `true`; failures are non-fatal (host/STUN only).
   */
  useTurn?: boolean;
  /** Extra ICE servers (public STUN etc.). */
  iceServers?: RTCIceServer[];
  /** Constraints for `getUserMedia`; defaults to echo cancellation + noise suppression. */
  audioConstraints?: MediaTrackConstraints;
  /** Use this stream instead of calling `getUserMedia` (device management done by the app). */
  localStream?: MediaStream;
  /**
   * Microphone to open (`deviceId` from `enumerateAudioDevices()`); default device when unset.
   * Ignored when `localStream` is given. Change at runtime with `setInputDevice`.
   */
  inputDeviceId?: string;
  /** Software microphone gain `0..4` (`1` = unity, default). Needs Web Audio; see `setInputGain`. */
  inputGain?: number;
  /**
   * Meter the local microphone with Web Audio and emit `localEnergy` / `localSpeaking`
   * (default `true`). Pass an object to tune the VAD, `false` to disable.
   */
  localVoiceActivity?: boolean | AudioLevelMeterOptions;
  /** Application-level keepalive interval in ms (`Ping`/`Pong`). Default 15000, 0 disables. */
  pingIntervalMs?: number;
  /** Timeout for request/response exchanges (join, offer) in ms. Default 10000. */
  requestTimeoutMs?: number;
  /**
   * Reconnect automatically when the control connection drops (default `true`). The client
   * presents its resume token so the server hands the same session back (`recovered` with
   * `resumed: true`); once the server-side grace period is over it gets a fresh session and
   * re-joins the previous channels itself (`resumed: false`).
   */
  autoReconnect?: boolean;
  /** Exponential backoff for reconnect attempts. */
  reconnect?: Partial<ReconnectPolicy>;
}

export interface ReconnectPolicy {
  /** Delay before the first attempt (ms). Default 500. */
  initialDelayMs: number;
  /** Upper bound for the delay between attempts (ms). Default 8000. */
  maxDelayMs: number;
  /** Multiplier applied after every failed attempt. Default 2. */
  factor: number;
  /** Random jitter as a fraction of the delay (0–1). Default 0.3. */
  jitter: number;
  /** Give up (`failedToRecover`) after this many failed attempts. Default 10. */
  maxAttempts: number;
}

const DEFAULT_RECONNECT: ReconnectPolicy = {
  initialDelayMs: 500,
  maxDelayMs: 8_000,
  factor: 2,
  jitter: 0.3,
  maxAttempts: 10,
};

export interface SessionInfo {
  sessionId: string;
  userId: string;
  ssrc: number;
  /** `true` when this session was resumed after a dropped connection. */
  resumed: boolean;
}

export interface Participant {
  userId: string;
  displayName: string;
  ssrc: number;
  role: ParticipantBrief['role'] | 'unknown';
  muted: boolean;
  serverMuted: boolean;
  speaking: boolean;
  /** Last server-reported audio energy 0..1 (`0` when silent). */
  energy: number;
}

/** A text-chat message; see {@link AurixEvents.chatMessage}. */
export interface ChatMessage {
  id: string;
  /** Set for channel messages. */
  channelId?: string;
  fromUserId: string;
  displayName: string;
  /** Set for directed messages (the target is either this user or, on the echo, the peer). */
  toUserId?: string;
  text: string;
  /** Game payload (`/command` args, ping coordinates, item links, ...). */
  metadata?: JsonValue;
  sentAt: Date;
  /** `true` when this is the echo of a message this client sent. */
  own: boolean;
  /** Injected by the game server through the REST API rather than sent by a player. */
  system: boolean;
  /** The `clientRef` passed to `sendMessage`/`sendDirectMessage`; only on `own` echoes. */
  clientRef?: string;
}

export interface SendMessageOptions {
  /** Structured game payload delivered verbatim; counts toward the server size limit. */
  metadata?: JsonValue;
  /**
   * Correlation id the server echoes back on the accepted message or the rejection so the
   * UI can reconcile an optimistically rendered message. Generated when omitted.
   */
  clientRef?: string;
}

export type ConnectionState =
  | 'disconnected'
  | 'connecting'
  | 'connected'
  | 'media-connecting'
  | 'media-connected'
  /** Connection lost; automatic reconnect attempts are in progress. */
  | 'reconnecting'
  | 'failed';

export interface AurixEvents {
  connectionState: (state: ConnectionState) => void;
  sessionReady: (info: SessionInfo) => void;
  /** Remote mixed audio stream from the SFU; attach to an `<audio>` element. */
  remoteStream: (stream: MediaStream) => void;
  channelJoined: (channelId: string, participants: Participant[]) => void;
  channelLeft: (channelId: string) => void;
  participantJoined: (channelId: string, participant: Participant) => void;
  participantLeft: (channelId: string, userId: string) => void;
  participantUpdated: (channelId: string, participant: Participant) => void;
  speaking: (channelId: string, userId: string, speaking: boolean) => void;
  /**
   * Periodic audio levels (0..1) of channel members whose energy changed since the last
   * report; `Participant.energy` is updated before the event fires. A participant that went
   * silent is reported once with `0`, so meters should decay on their own between reports.
   */
  energy: (channelId: string, levels: ParticipantEnergy[]) => void;
  /** Local microphone level sample (see `localVoiceActivity`), every ~50 ms while media is up. */
  localEnergy: (sample: AudioLevelSample) => void;
  /** Local VAD edge: the user started (`true`) / stopped (`false`) speaking. */
  localSpeaking: (speaking: boolean) => void;
  /**
   * The set of audio devices changed (plugged/unplugged) while media is up; `devices` is the
   * fresh list. A removed microphone is replaced by the default one automatically.
   */
  devicesChanged: (devices: AudioDevices) => void;
  /** The microphone in use changed (`setInputDevice`, or a fallback after the device vanished). */
  inputDeviceChanged: (deviceId: string | undefined) => void;
  positions: (channelId: string, positions: UserPosition[]) => void;
  recording: (channelId: string, recordingId: string, active: boolean, initiatedBy: string) => void;
  bitrate: (targetKbps: number, reason: string) => void;
  kicked: (channelId: string, reason: string) => void;
  /**
   * Server-side snapshot of this user's receiver preferences, sent right after the session
   * is established: persistent cross-mutes plus (on a resumed session) local mutes/volumes.
   */
  receiverPreferences: (prefs: ReceiverPreferences) => void;
  /** A cross-mute placed or lifted by this user (from this or any other device/REST). */
  userBlockChanged: (userId: string, blocked: boolean) => void;
  /**
   * The server acknowledged a new transmission policy — after `setTransmission`, or reset to
   * `none` because the single target channel was left.
   */
  transmissionChanged: (mode: TransmissionMode) => void;
  /**
   * The server acknowledged a new focus — after `setChannelFocus`, or cleared because the
   * focused channel was left.
   */
  channelFocusChanged: (channelId: string | undefined) => void;
  /** Connection lost unexpectedly; attempt `attempt` (1-based) is scheduled in `delayMs`. */
  recovering: (attempt: number, delayMs: number, cause: string) => void;
  /**
   * Reconnected. `info.resumed` tells whether the server handed back the same session
   * (peers never noticed) or a fresh one that the client re-joined to its channels.
   */
  recovered: (info: SessionInfo) => void;
  /** Reconnect attempts exhausted or a definite refusal; the client is now `failed`. */
  failedToRecover: (error: Error) => void;
  /** The server ended the session (kick from the platform, ban, shutdown). No reconnect. */
  sessionClosed: (reason: string) => void;
  /**
   * A text message for this client: channel message of a joined channel, directed message
   * addressed to this user, or the echo (`own: true`) of a message this client sent.
   */
  chatMessage: (message: ChatMessage) => void;
  /** Another member of `channelId` started/stopped typing (never this client's own state). */
  participantTyping: (channelId: string, userId: string, typing: boolean) => void;
  serverError: (code: string, message: string) => void;
  error: (error: Error) => void;
  message: (message: ServerMessage | UnknownMessage) => void;
}

type Listener<K extends keyof AurixEvents> = AurixEvents[K];
type AnyListener = (...args: unknown[]) => void;

export interface ReceiverPreferences {
  /** Users this user has cross-muted; the server drops their audio in every channel. */
  blockedUsers: string[];
  localMutes: LocalMute[];
  volumes: ParticipantVolume[];
  transmission: TransmissionMode;
  focusChannel: string | undefined;
}

/**
 * Where this session's audio is sent. `none` mutes the uplink at the server without touching
 * the microphone, `single` limits it to one joined channel (e.g. talk to the party while
 * still hearing the team), `all` (default) fans out to every joined channel.
 */
export type TransmissionMode =
  | { type: 'none' }
  | { type: 'single'; channelId: string }
  | { type: 'all' };

export function transmissionToWire(mode: TransmissionMode): TransmissionModeWire {
  switch (mode.type) {
    case 'none':
      return { mode: 'none' };
    case 'single':
      return { mode: 'single', channel_id: mode.channelId };
    case 'all':
      return { mode: 'all' };
  }
}

export function transmissionFromWire(mode: TransmissionModeWire | undefined): TransmissionMode {
  if (!mode) return { type: 'all' };
  switch (mode.mode) {
    case 'none':
      return { type: 'none' };
    case 'single':
      return { type: 'single', channelId: mode.channel_id };
    default:
      return { type: 'all' };
  }
}

/** Marker for "in every channel" in the local-mute table. */
const ALL_CHANNELS = '*';

interface Pending<T> {
  resolve: (value: T) => void;
  reject: (error: Error) => void;
  timer: ReturnType<typeof setTimeout>;
}

function checkInputGain(gain: number): number {
  if (!(gain >= 0 && gain <= MAX_INPUT_GAIN)) {
    throw new RangeError(`input gain must be within 0..${MAX_INPUT_GAIN}`);
  }
  return gain;
}

function decodeJwtSubject(token: string): string | undefined {
  const parts = token.split('.');
  if (parts.length < 2 || parts[1] === undefined) return undefined;
  try {
    const b64 = parts[1].replace(/-/g, '+').replace(/_/g, '/');
    const json = atob(b64.padEnd(b64.length + ((4 - (b64.length % 4)) % 4), '='));
    const claims: unknown = JSON.parse(json);
    if (typeof claims === 'object' && claims !== null) {
      const c = claims as { user_id?: unknown; sub?: unknown };
      if (typeof c.user_id === 'string') return c.user_id;
      if (typeof c.sub === 'string') return c.sub;
    }
  } catch {
    /* opaque token: user id becomes known from ChannelJoinAck */
  }
  return undefined;
}

/**
 * Ask the browser to *decode* Opus in stereo (`stereo=1` = our receive preference, RFC 7587).
 * libwebrtc sizes its decoder from that parameter of the local description, not from the
 * packets, so without it the server's stereo downlink (directional positional channels) is
 * downmixed to mono before playout. The microphone uplink stays mono.
 */
function preferStereoOpus(sdp: string): string {
  const lines = sdp.split(/\r?\n/);
  const opusPts = new Set<string>();
  for (const line of lines) {
    const m = /^a=rtpmap:(\d+) opus\/48000\/2/i.exec(line);
    if (m?.[1] !== undefined) opusPts.add(m[1]);
  }
  if (opusPts.size === 0) return sdp;
  const seen = new Set<string>();
  const out: string[] = [];
  for (const line of lines) {
    const m = /^a=fmtp:(\d+) (.*)$/.exec(line);
    if (m?.[1] !== undefined && m[2] !== undefined && opusPts.has(m[1])) {
      seen.add(m[1]);
      const params = m[2].split(';').filter((p) => !/^\s*stereo=/.test(p));
      params.push('stereo=1');
      out.push(`a=fmtp:${m[1]} ${params.join(';')}`);
      continue;
    }
    out.push(line);
  }
  // No fmtp line yet: append one right after the rtpmap.
  const result: string[] = [];
  for (const line of out) {
    result.push(line);
    const m = /^a=rtpmap:(\d+) opus\/48000\/2/i.exec(line);
    if (m?.[1] !== undefined && !seen.has(m[1])) result.push(`a=fmtp:${m[1]} stereo=1`);
  }
  return result.join(sdp.includes('\r\n') ? '\r\n' : '\n');
}

/**
 * Browser client for Aurix: authenticated WebSocket control channel plus one WebRTC
 * peer connection carrying the microphone uplink and the server-mixed downlink.
 *
 * ```ts
 * const client = new AurixClient({ apiUrl, wsUrl, token });
 * client.on('remoteStream', (s) => { audioEl.srcObject = s; });
 * await client.connect();
 * await client.joinChannel(channelId);
 * ```
 */
export class AurixClient {
  private readonly opts: Required<
    Pick<AurixClientOptions, 'useTurn' | 'pingIntervalMs' | 'requestTimeoutMs'>
  > &
    AurixClientOptions;
  private ws: WebSocket | undefined;
  private pc: RTCPeerConnection | undefined;
  private localStream: MediaStream | undefined;
  private localMeter: AudioLevelMeter | undefined;
  private inputPipeline: InputPipeline | undefined;
  private inputGainValue = 1;
  private inputDeviceIdValue: string | undefined;
  private remoteStream: MediaStream | undefined;
  private readonly outputElements = new Set<HTMLMediaElement>();
  private outputVolumeValue = 1;
  private outputMutedValue = false;
  private outputDeviceIdValue: string | undefined;
  private readonly onDeviceChange = (): void => {
    void enumerateAudioDevices().then((d) => this.emit('devicesChanged', d));
  };
  private listeners = new Map<keyof AurixEvents, Set<AnyListener>>();
  private pendingJoins = new Map<string, Pending<Participant[]>>();
  private pendingModerations = new Map<string, Pending<void>>();
  /** client_ref → pending `sendMessage`/`sendDirectMessage`. */
  private pendingChat = new Map<string, Pending<ChatMessage>>();
  private chatRefCounter = 0;
  /** channel id → last `ChatTyping { typing: true }` sent (ms, `performance.now()` clock). */
  private typingSentAt = new Map<string, number>();
  private pendingAnswer: Pending<string> | undefined;
  private pendingInit: Pending<SessionInfo> | undefined;
  private pingTimer: ReturnType<typeof setInterval> | undefined;
  private pingNonce = 1;
  private lastPingSentAt = 0;
  private rttMs = 0;
  private state: ConnectionState = 'disconnected';
  private session: SessionInfo | undefined;
  private userId: string | undefined;
  private channels = new Map<string, Map<string, Participant>>();
  private muted = false;
  /** user_id → channel ids (or `ALL_CHANNELS`) this client has locally muted. */
  private localMutes = new Map<string, Set<string>>();
  /** user_id → receiver-local gain (unity entries are not stored). */
  private volumes = new Map<string, number>();
  private blockedUsers = new Set<string>();
  private transmission: TransmissionMode = { type: 'all' };
  private focusChannel: string | undefined;
  private closedByUser = false;
  private readonly reconnectPolicy: ReconnectPolicy;
  private resumeToken: string | undefined;
  private resumeGraceMs = 0;
  private reconnectTimer: ReturnType<typeof setTimeout> | undefined;
  private reconnectAttempt = 0;
  private lastPongAt = 0;

  constructor(options: AurixClientOptions) {
    this.opts = {
      useTurn: true,
      pingIntervalMs: 15_000,
      requestTimeoutMs: 10_000,
      autoReconnect: true,
      ...options,
    };
    this.reconnectPolicy = { ...DEFAULT_RECONNECT, ...(options.reconnect ?? {}) };
    this.userId = decodeJwtSubject(options.token);
    if (options.inputGain !== undefined) this.inputGainValue = checkInputGain(options.inputGain);
    this.inputDeviceIdValue = options.inputDeviceId;
  }

  // ── Events ──

  on<K extends keyof AurixEvents>(event: K, listener: Listener<K>): () => void {
    let set = this.listeners.get(event);
    if (!set) {
      set = new Set();
      this.listeners.set(event, set);
    }
    set.add(listener as unknown as AnyListener);
    return () => this.off(event, listener);
  }

  off<K extends keyof AurixEvents>(event: K, listener: Listener<K>): void {
    this.listeners.get(event)?.delete(listener as unknown as AnyListener);
  }

  private emit<K extends keyof AurixEvents>(event: K, ...args: Parameters<AurixEvents[K]>): void {
    const set = this.listeners.get(event);
    if (!set) return;
    for (const l of set) {
      try {
        l(...args);
      } catch (e) {
        if (event !== 'error') this.emit('error', e instanceof Error ? e : new Error(String(e)));
      }
    }
  }

  // ── Public state ──

  get connectionState(): ConnectionState {
    return this.state;
  }

  /** Server-side resume window for this session (ms); `0` when resume is disabled. */
  get resumeGrace(): number {
    return this.resumeGraceMs;
  }

  get sessionInfo(): SessionInfo | undefined {
    return this.session;
  }

  get isMuted(): boolean {
    return this.muted;
  }

  /** Last measured application-level round-trip time (ms), from `Ping`/`Pong`. */
  get roundTripMs(): number {
    return this.rttMs;
  }

  participants(channelId: string): Participant[] {
    return Array.from(this.channels.get(channelId)?.values() ?? []);
  }

  joinedChannels(): string[] {
    return Array.from(this.channels.keys());
  }

  get localMediaStream(): MediaStream | undefined {
    return this.localStream;
  }

  // ── Devices, input gain, speaker ──

  /** Microphones and speakers; labels appear once microphone permission was granted. */
  static enumerateAudioDevices(): Promise<AudioDevices> {
    return enumerateAudioDevices();
  }

  /** `deviceId` of the microphone currently captured (`undefined` before media is up). */
  get inputDeviceId(): string | undefined {
    const track = this.localStream?.getAudioTracks()[0];
    return track?.getSettings().deviceId ?? this.inputDeviceIdValue;
  }

  /**
   * Switch the microphone (`undefined` = system default). Takes effect immediately when
   * media is up (`replaceTrack`, no renegotiation, mute/gain/meter preserved), otherwise on
   * the next `connect()`. Rejects when the device cannot be opened; the old one keeps going.
   */
  async setInputDevice(deviceId: string | undefined): Promise<void> {
    this.inputDeviceIdValue = deviceId;
    if (!this.localStream || !this.pc) return;
    const stream = await this.openMicrophone();
    await this.adoptLocalStream(stream);
  }

  /** Current software microphone gain (`1` = unity). */
  get inputGain(): number {
    return this.inputGainValue;
  }

  /**
   * Software gain on the outgoing microphone signal, `0..4` (`1` = unity, `2` ≈ +6 dB). The
   * first non-unity value routes the microphone through Web Audio; throws when that is not
   * available. Independent of `setMuted` and of the browser's own AGC.
   */
  setInputGain(gain: number): void {
    this.inputGainValue = checkInputGain(gain);
    if (!this.localStream) return;
    if (!this.inputPipeline) {
      if (gain === 1) return;
      this.ensureInputPipeline(this.localStream);
      return;
    }
    this.inputPipeline.setGain(gain);
  }

  /**
   * Let the client drive an `<audio>` element: it receives the remote stream (now and after
   * every reconnect), the output volume/mute and the selected speaker. Several elements may be
   * attached; `detachAudioOutput` releases one.
   */
  attachAudioOutput(element: HTMLMediaElement): void {
    this.outputElements.add(element);
    this.applyOutput(element);
    if (this.remoteStream && element.paused) void element.play().catch(() => undefined);
  }

  detachAudioOutput(element: HTMLMediaElement): void {
    if (!this.outputElements.delete(element)) return;
    element.srcObject = null;
  }

  /** `true` when this browser lets `setOutputDevice` pick the speaker. */
  static get supportsOutputSelection(): boolean {
    return supportsOutputSelection();
  }

  get outputDeviceId(): string | undefined {
    return this.outputDeviceIdValue;
  }

  /**
   * Play remote audio on a specific speaker (`deviceId` of an `audiooutput`, `undefined` =
   * system default). Applies to attached elements; rejects on browsers without `setSinkId`.
   */
  async setOutputDevice(deviceId: string | undefined): Promise<void> {
    if (!supportsOutputSelection()) {
      throw new Error('output device selection is not supported by this browser');
    }
    const previous = this.outputDeviceIdValue;
    this.outputDeviceIdValue = deviceId;
    try {
      await Promise.all(Array.from(this.outputElements, (el) => el.setSinkId(deviceId ?? '')));
    } catch (e) {
      this.outputDeviceIdValue = previous;
      throw e;
    }
  }

  get outputVolume(): number {
    return this.outputVolumeValue;
  }

  /** Master volume of everything you hear, `0..1` (attached elements). */
  setOutputVolume(volume: number): void {
    if (!(volume >= 0 && volume <= 1)) throw new RangeError('output volume must be within 0..1');
    this.outputVolumeValue = volume;
    for (const el of this.outputElements) el.volume = volume;
  }

  get isOutputMuted(): boolean {
    return this.outputMutedValue;
  }

  /**
   * Speaker mute: silence all remote audio locally (the remote track is disabled, so it also
   * works for apps that play the stream themselves). Nobody else is told; your microphone is
   * unaffected.
   */
  setOutputMuted(muted: boolean): void {
    this.outputMutedValue = muted;
    this.remoteStream?.getAudioTracks().forEach((t) => {
      t.enabled = !muted;
    });
    for (const el of this.outputElements) el.muted = muted;
  }

  /** Smoothed local microphone energy 0..1 (0 when metering is off or media is down). */
  get localEnergy(): number {
    return this.localMeter?.energy ?? 0;
  }

  /** Local VAD state (false when metering is off or media is down). */
  get localSpeaking(): boolean {
    return this.localMeter?.speaking ?? false;
  }

  get peerConnection(): RTCPeerConnection | undefined {
    return this.pc;
  }

  // ── Receiver-side controls (affect only what *this* client hears) ──

  /**
   * Stop hearing `userId` in `channelId`, or in every channel when `channelId` is omitted.
   * The other participant is not told. Survives reconnects (re-applied by the client).
   */
  setParticipantMuted(userId: string, muted: boolean, channelId?: string): void {
    const key = channelId ?? ALL_CHANNELS;
    const scopes = this.localMutes.get(userId) ?? new Set<string>();
    if (muted) {
      scopes.add(key);
    } else if (channelId === undefined) {
      scopes.clear();
    } else {
      scopes.delete(key);
    }
    if (scopes.size === 0) this.localMutes.delete(userId);
    else this.localMutes.set(userId, scopes);
    if (channelId !== undefined && !this.channels.has(channelId)) return;
    this.trySend({
      type: 'SetParticipantMute',
      data: { user_id: userId, channel_id: channelId ?? null, muted },
    });
  }

  /** `true` when this client muted `userId` in `channelId` (or everywhere). */
  isParticipantMuted(userId: string, channelId?: string): boolean {
    const scopes = this.localMutes.get(userId);
    if (!scopes) return false;
    if (scopes.has(ALL_CHANNELS)) return true;
    return channelId !== undefined && scopes.has(channelId);
  }

  /**
   * Receiver-local gain for `userId`: `0` silence, `1` as sent, up to `2` (≈ +6 dB).
   * Multiplies positional attenuation; applied by the server before mixing.
   */
  setParticipantVolume(userId: string, volume: number): void {
    if (!Number.isFinite(volume) || volume < 0 || volume > MAX_PARTICIPANT_VOLUME) {
      throw new RangeError(`volume must be within 0..${MAX_PARTICIPANT_VOLUME}`);
    }
    if (volume === 1) this.volumes.delete(userId);
    else this.volumes.set(userId, volume);
    this.trySend({ type: 'SetParticipantVolume', data: { user_id: userId, volume } });
  }

  getParticipantVolume(userId: string): number {
    return this.volumes.get(userId) ?? 1;
  }

  /**
   * Persistent, mutual cross-mute: neither side hears the other in any channel, on any
   * device, until lifted. Stored server-side; confirmed via `userBlockChanged`.
   */
  setUserBlocked(userId: string, blocked: boolean): void {
    this.requireOpen();
    this.send({ type: 'SetUserBlock', data: { user_id: userId, blocked } });
  }

  isUserBlocked(userId: string): boolean {
    return this.blockedUsers.has(userId);
  }

  getBlockedUsers(): string[] {
    return Array.from(this.blockedUsers);
  }

  /**
   * Choose where the microphone goes: `{ type: 'all' }` (default), `{ type: 'single',
   * channelId }` for exactly one joined channel or `{ type: 'none' }` to send nowhere while
   * still receiving. Applied by the server; confirmed via `transmissionChanged`. A `single`
   * target must be a joined channel — when it is left, the server resets the policy to `none`.
   */
  setTransmission(mode: TransmissionMode): void {
    if (mode.type === 'single' && !mode.channelId) {
      throw new RangeError('single transmission requires a channelId');
    }
    this.transmission = mode;
    if (mode.type === 'single' && !this.channels.has(mode.channelId)) return; // sent on join
    this.trySend({ type: 'SetTransmission', data: { mode: transmissionToWire(mode) } });
  }

  /** Shorthand for `setTransmission({ type: 'single', channelId })`. */
  transmitToChannel(channelId: string): void {
    this.setTransmission({ type: 'single', channelId });
  }

  getTransmission(): TransmissionMode {
    return this.transmission;
  }

  /** `true` when audio sent now would be delivered to `channelId`. */
  transmitsTo(channelId: string): boolean {
    const t = this.transmission;
    return t.type === 'all' || (t.type === 'single' && t.channelId === channelId);
  }

  /**
   * Receiver-local focus: audio from `channelId` stays at full volume while every other
   * joined channel is attenuated by the server's `media.unfocused_channel_gain` (0.5 by
   * default). `undefined` clears the focus. Confirmed via `channelFocusChanged`; cleared by
   * the server when the focused channel is left.
   */
  setChannelFocus(channelId: string | undefined): void {
    this.focusChannel = channelId;
    if (channelId !== undefined && !this.channels.has(channelId)) return; // sent on join
    this.trySend({ type: 'SetChannelFocus', data: { channel_id: channelId ?? null } });
  }

  getChannelFocus(): string | undefined {
    return this.focusChannel;
  }

  /** Re-send the client-held local mutes/volumes after a fresh (non-resumed) session. */
  private replayReceiverPrefs(): void {
    for (const [userId, scopes] of this.localMutes) {
      if (scopes.has(ALL_CHANNELS)) {
        this.trySend({
          type: 'SetParticipantMute',
          data: { user_id: userId, channel_id: null, muted: true },
        });
      }
    }
    for (const [userId, volume] of this.volumes) {
      this.trySend({ type: 'SetParticipantVolume', data: { user_id: userId, volume } });
    }
    if (this.transmission.type === 'none') {
      this.trySend({ type: 'SetTransmission', data: { mode: { mode: 'none' } } });
    }
  }

  /**
   * Channel-scoped mutes, a `single` transmission target and the focus need membership, so
   * they are re-sent per `ChannelJoinAck`.
   */
  private replayChannelMutes(channelId: string): void {
    for (const [userId, scopes] of this.localMutes) {
      if (scopes.has(channelId) && !scopes.has(ALL_CHANNELS)) {
        this.trySend({
          type: 'SetParticipantMute',
          data: { user_id: userId, channel_id: channelId, muted: true },
        });
      }
    }
    if (this.transmission.type === 'single' && this.transmission.channelId === channelId) {
      this.trySend({
        type: 'SetTransmission',
        data: { mode: transmissionToWire(this.transmission) },
      });
    }
    if (this.focusChannel === channelId) {
      this.trySend({ type: 'SetChannelFocus', data: { channel_id: channelId } });
    }
  }

  // ── Lifecycle ──

  /**
   * Open the control channel, wait for `SessionInitAck`, capture the microphone and
   * negotiate WebRTC. Resolves once the server acknowledged the session; media connects
   * asynchronously (`connectionState` → `media-connected`).
   */
  async connect(): Promise<SessionInfo> {
    if (this.ws || this.reconnectTimer !== undefined) throw new Error('already connected');
    this.closedByUser = false;
    this.resumeToken = undefined;
    this.session = undefined;
    this.setState('connecting');

    const info = await this.openControlChannel();
    this.session = info;
    this.setState('connected');
    this.emit('sessionReady', info);
    this.startPing();
    this.replayReceiverPrefs();

    try {
      await this.startMedia();
    } catch (e) {
      const err = e instanceof Error ? e : new Error(String(e));
      this.emit('error', err);
      this.teardown('media setup failed', 'failed');
      throw err;
    }
    return info;
  }

  /** Leave all channels, close media and the control channel. Cancels any reconnect. */
  disconnect(reason = 'client disconnect'): void {
    this.teardown(reason, 'disconnected');
  }

  private teardown(reason: string, finalState: 'disconnected' | 'failed'): void {
    this.closedByUser = true;
    this.cancelReconnect();
    this.stopPing();
    this.failPending('disconnected');

    if (this.ws && this.ws.readyState === WebSocket.OPEN && this.session) {
      this.trySend({
        type: 'SessionClose',
        data: { session_id: this.session.sessionId, reason },
      });
    }
    this.ws?.close(1000, reason);
    this.ws = undefined;
    this.stopLocalMeter();
    this.pc?.close();
    this.pc = undefined;
    this.releaseLocalStream(this.localStream);
    this.localStream = undefined;
    this.inputPipeline?.close();
    this.inputPipeline = undefined;
    this.remoteStream = undefined;
    for (const el of this.outputElements) el.srcObject = null;
    if (typeof navigator !== 'undefined') {
      navigator.mediaDevices?.removeEventListener?.('devicechange', this.onDeviceChange);
    }
    for (const channelId of this.channels.keys()) this.emit('channelLeft', channelId);
    this.channels.clear();
    this.session = undefined;
    this.resumeToken = undefined;
    this.setState(finalState);
  }

  /**
   * Reconnect now instead of waiting for the next scheduled attempt (e.g. when the app
   * observes that the network is back). No-op unless the client is `reconnecting`.
   */
  reconnectNow(): void {
    if (this.state !== 'reconnecting' || this.reconnectTimer === undefined) return;
    clearTimeout(this.reconnectTimer);
    this.reconnectTimer = undefined;
    void this.attemptReconnect();
  }

  private failPending(reason: string): void {
    for (const p of this.pendingJoins.values()) {
      clearTimeout(p.timer);
      p.reject(new Error(reason));
    }
    this.pendingJoins.clear();
    for (const p of this.pendingModerations.values()) {
      clearTimeout(p.timer);
      p.reject(new Error(reason));
    }
    this.pendingModerations.clear();
    for (const p of this.pendingChat.values()) {
      clearTimeout(p.timer);
      p.reject(new Error(reason));
    }
    this.pendingChat.clear();
    this.typingSentAt.clear();
    this.rejectPending(this.pendingAnswer, reason);
    this.pendingAnswer = undefined;
    this.rejectPending(this.pendingInit, reason);
    this.pendingInit = undefined;
  }

  // ── Channels ──

  /**
   * Join a channel. Resolves with the current participant list. `joinToken` is a one-time
   * `join` action token for this channel; when omitted the `joinToken` option is consulted
   * and finally the session credential itself is presented.
   */
  async joinChannel(channelId: string, joinToken?: string): Promise<Participant[]> {
    this.requireOpen();
    if (this.pendingJoins.has(channelId)) throw new Error('join already pending');
    const token = joinToken ?? (this.opts.joinToken ? await this.opts.joinToken(channelId) : this.opts.token);
    this.requireOpen();
    if (this.pendingJoins.has(channelId)) throw new Error('join already pending');
    return new Promise<Participant[]>((resolve, reject) => {
      const timer = setTimeout(() => {
        this.pendingJoins.delete(channelId);
        reject(new Error(`join ${channelId} timed out`));
      }, this.opts.requestTimeoutMs);
      this.pendingJoins.set(channelId, { resolve, reject, timer });
      this.send({ type: 'ChannelJoin', data: { channel_id: channelId, token } });
    });
  }

  /**
   * Kick or server-mute/unmute `userId` in `channelId` with a one-time `kick`/`mute`/`unmute`
   * action token minted for this user (`POST /v1/tokens/action`). The token is spent whether
   * or not the operation succeeds after the server accepted it; a mismatch (other actor,
   * target, channel or action) leaves it unused and rejects with `AUTH_DENIED`.
   */
  moderate(channelId: string, userId: string, action: ModerationAction, token: string, reason?: string): Promise<void> {
    this.requireOpen();
    const key = `${channelId}/${userId}/${action}`;
    if (this.pendingModerations.has(key)) return Promise.reject(new Error('moderation already pending'));
    return new Promise<void>((resolve, reject) => {
      const timer = setTimeout(() => {
        this.pendingModerations.delete(key);
        reject(new Error(`${action} ${userId} timed out`));
      }, this.opts.requestTimeoutMs);
      this.pendingModerations.set(key, { resolve, reject, timer });
      this.send({
        type: 'ModerateParticipant',
        data: { channel_id: channelId, user_id: userId, action, token, reason: reason ?? null },
      });
    });
  }

  leaveChannel(channelId: string): void {
    this.requireOpen();
    this.send({ type: 'ChannelLeave', data: { channel_id: channelId } });
    this.typingSentAt.delete(channelId);
    if (this.channels.delete(channelId)) this.emit('channelLeft', channelId);
  }

  /**
   * Send a text message to every member of a joined channel. Resolves with the server's copy
   * (id, timestamp) once accepted, which is also emitted as `chatMessage` with `own: true`.
   * Rejects with `<CODE>: <message>` on refusal (`AUTH_DENIED` not a member, `USER_MUTED`,
   * `RATE_LIMIT_EXCEEDED`, `MESSAGE_BLOCKED` by the content filter, `VALIDATION_ERROR`).
   */
  sendMessage(channelId: string, text: string, options: SendMessageOptions = {}): Promise<ChatMessage> {
    return this.sendChat((clientRef) => ({
      type: 'ChatSend',
      data: {
        channel_id: channelId,
        text,
        ...(options.metadata !== undefined ? { metadata: options.metadata } : {}),
        client_ref: clientRef,
      },
    }), options.clientRef);
  }

  /**
   * Send a text message to one user of the same app. Live-only: the target must currently
   * have a session (`USER_OFFLINE` otherwise) and neither side may have blocked the other.
   */
  sendDirectMessage(userId: string, text: string, options: SendMessageOptions = {}): Promise<ChatMessage> {
    return this.sendChat((clientRef) => ({
      type: 'ChatSendDirect',
      data: {
        user_id: userId,
        text,
        ...(options.metadata !== undefined ? { metadata: options.metadata } : {}),
        client_ref: clientRef,
      },
    }), options.clientRef);
  }

  /**
   * Announce that this user is (not) typing in `channelId`. Best-effort: `typing: true` is
   * coalesced client-side to at most one message per `intervalMs`, so it can be called on
   * every keystroke; `typing: false` is always sent (and clears the throttle).
   */
  setTyping(channelId: string, typing: boolean, intervalMs = 1500): void {
    if (!this.ws || this.ws.readyState !== WebSocket.OPEN) return;
    const now = performance.now();
    if (typing) {
      const last = this.typingSentAt.get(channelId);
      if (last !== undefined && now - last < intervalMs) return;
      this.typingSentAt.set(channelId, now);
    } else {
      this.typingSentAt.delete(channelId);
    }
    this.send({ type: 'ChatTyping', data: { channel_id: channelId, typing } });
  }

  private sendChat(build: (clientRef: string) => ClientMessage, ref?: string): Promise<ChatMessage> {
    this.requireOpen();
    const clientRef = ref ?? `m${++this.chatRefCounter}-${Date.now().toString(36)}`;
    if (this.pendingChat.has(clientRef)) return Promise.reject(new Error(`clientRef ${clientRef} already pending`));
    return new Promise<ChatMessage>((resolve, reject) => {
      const timer = setTimeout(() => {
        this.pendingChat.delete(clientRef);
        reject(new Error('message send timed out'));
      }, this.opts.requestTimeoutMs);
      this.pendingChat.set(clientRef, { resolve, reject, timer });
      this.send(build(clientRef));
    });
  }

  /** Mute/unmute the microphone locally and announce the state to channel members. */
  setMuted(muted: boolean): void {
    this.muted = muted;
    this.localStream?.getAudioTracks().forEach((t) => {
      t.enabled = !muted;
    });
    if (!this.ws || this.ws.readyState !== WebSocket.OPEN || !this.userId) return;
    for (const channelId of this.channels.keys()) {
      this.send({
        type: 'MuteStateChanged',
        data: { channel_id: channelId, user_id: this.userId, muted, server_muted: false },
      });
    }
  }

  /** Publish this user's 3D position/orientation for spatial audio in a channel. */
  updatePosition(channelId: string, position: UserPosition['position'], orientation: UserPosition['orientation']): void {
    this.requireOpen();
    if (!this.userId) throw new Error('user id unknown until a channel is joined');
    this.send({
      type: 'PositionUpdate',
      data: { channel_id: channelId, positions: [{ user_id: this.userId, position, orientation }] },
    });
  }

  respondToRecording(recordingId: string, consent: RecordingConsent): void {
    this.requireOpen();
    this.send({ type: 'RecordingConsentResponse', data: { recording_id: recordingId, consent } });
  }

  /** Send WebRTC receiver statistics to the server so it can adapt the downlink bitrate. */
  async reportQuality(): Promise<void> {
    if (!this.pc || !this.ws || this.ws.readyState !== WebSocket.OPEN) return;
    const stats = await this.pc.getStats();
    let jitter = 0;
    let lost = 0;
    let received = 0;
    stats.forEach((report) => {
      if (report.type === 'inbound-rtp') {
        const r = report as RTCInboundRtpStreamStats;
        jitter = (r.jitter ?? 0) * 1000;
        lost = r.packetsLost ?? 0;
        received = r.packetsReceived ?? 0;
      }
    });
    const total = lost + received;
    this.send({
      type: 'QualityReport',
      data: { rtt_ms: this.rttMs, jitter_ms: jitter, packet_loss: total > 0 ? lost / total : 0 },
    });
  }

  // ── Internals: control channel ──

  private openControlChannel(): Promise<SessionInfo> {
    return new Promise<SessionInfo>((resolve, reject) => {
      // Browsers cannot set the Authorization header on a WebSocket upgrade; the server
      // accepts the JWT as a `bearer.<jwt>` sub-protocol and echoes `aurix` back. The
      // resume credential travels the same way.
      const protocols = [AURIX_SUBPROTOCOL, `${BEARER_SUBPROTOCOL_PREFIX}${this.opts.token}`];
      if (this.session && this.resumeToken) {
        protocols.push(`${RESUME_SUBPROTOCOL_PREFIX}${this.session.sessionId}.${this.resumeToken}`);
      }
      const ws = new WebSocket(this.opts.wsUrl, protocols);
      this.ws = ws;
      const timer = setTimeout(() => {
        this.pendingInit = undefined;
        ws.close();
        reject(new Error('session init timed out'));
      }, this.opts.requestTimeoutMs);
      this.pendingInit = { resolve, reject, timer };

      ws.onmessage = (ev) => {
        if (typeof ev.data !== 'string') return;
        let msg: ServerMessage | UnknownMessage;
        try {
          msg = parseServerMessage(ev.data);
        } catch (e) {
          this.emit('error', e instanceof Error ? e : new Error(String(e)));
          return;
        }
        this.handleMessage(msg);
      };
      ws.onerror = () => {
        this.emit('error', new Error('websocket error'));
        // A failed handshake is definitive; do not wait for the init timeout.
        if (this.pendingInit && this.ws === ws) {
          this.ws = undefined;
          this.rejectPending(this.pendingInit, 'websocket error');
          this.pendingInit = undefined;
          ws.close();
        }
      };
      ws.onclose = (ev) => {
        const wasCurrent = this.ws === ws;
        if (wasCurrent) this.ws = undefined;
        const cause = `websocket closed (${ev.code}${ev.reason ? `: ${ev.reason}` : ''})`;
        const initWasPending = this.pendingInit !== undefined;
        this.rejectPending(this.pendingInit, cause);
        this.pendingInit = undefined;
        // A drop before the first SessionInitAck fails `connect()` itself; a reconnect
        // attempt that fails is handled by `attemptReconnect`.
        if (!wasCurrent || this.closedByUser || initWasPending) return;
        this.onConnectionLost(cause);
      };
    });
  }

  // ── Internals: reconnect ──

  private onConnectionLost(cause: string): void {
    this.stopPing();
    this.failPending('connection lost');
    if (!this.opts.autoReconnect || !this.session) {
      this.emit('error', new Error(`connection lost (${cause})`));
      this.teardown('connection lost', 'failed');
      return;
    }
    this.reconnectAttempt = 0;
    this.setState('reconnecting');
    this.scheduleReconnect(cause);
  }

  private scheduleReconnect(cause: string): void {
    const policy = this.reconnectPolicy;
    if (this.reconnectAttempt >= policy.maxAttempts) {
      this.giveUp(new Error(`reconnect failed after ${this.reconnectAttempt} attempts (${cause})`));
      return;
    }
    const attempt = this.reconnectAttempt + 1;
    const base = Math.min(policy.maxDelayMs, policy.initialDelayMs * policy.factor ** (attempt - 1));
    const delay = Math.round(base * (1 + policy.jitter * (Math.random() * 2 - 1)));
    this.emit('recovering', attempt, delay, cause);
    this.reconnectTimer = setTimeout(() => {
      this.reconnectTimer = undefined;
      void this.attemptReconnect();
    }, delay);
  }

  private async attemptReconnect(): Promise<void> {
    if (this.closedByUser) return;
    this.reconnectAttempt += 1;
    const previous = this.session;
    const wanted = Array.from(this.channels.keys());
    let info: SessionInfo;
    try {
      if (this.opts.refreshToken) {
        this.opts.token = await this.opts.refreshToken();
        if (this.closedByUser) return;
      }
      info = await this.openControlChannel();
    } catch (e) {
      if (this.closedByUser) return;
      this.scheduleReconnect(e instanceof Error ? e.message : String(e));
      return;
    }
    if (this.closedByUser) return;

    this.session = info;
    this.reconnectAttempt = 0;
    this.startPing();
    if (!info.resumed) {
      // Fresh session: the server forgot our channels, so the roster below is stale.
      for (const channelId of wanted) {
        if (this.channels.delete(channelId)) this.emit('channelLeft', channelId);
      }
      this.replayReceiverPrefs();
    }
    this.setState('connected');
    try {
      await this.restoreMedia(info.resumed && previous?.ssrc === info.ssrc);
      if (!info.resumed) {
        for (const channelId of wanted) await this.joinChannel(channelId);
      }
    } catch (e) {
      if (this.closedByUser) return;
      const err = e instanceof Error ? e : new Error(String(e));
      this.emit('error', err);
      if (this.ws && this.ws.readyState === WebSocket.OPEN) {
        // Control plane is back but media/rejoin failed: retry the whole cycle.
        this.ws.close(4000, 'restore failed');
        this.ws = undefined;
        this.stopPing();
        this.setState('reconnecting');
        this.scheduleReconnect(err.message);
      }
      return;
    }
    this.emit('recovered', info);
  }

  /** After a reconnect: keep a still-connected peer connection, otherwise renegotiate. */
  private async restoreMedia(sameSession: boolean): Promise<void> {
    if (sameSession && this.pc && this.pc.connectionState === 'connected') {
      this.setState('media-connected');
      return;
    }
    this.pc?.close();
    this.pc = undefined;
    await this.startMedia();
  }

  private giveUp(error: Error): void {
    this.cancelReconnect();
    this.emit('failedToRecover', error);
    this.teardown('reconnect failed', 'failed');
  }

  private startLocalMeter(stream: MediaStream): void {
    const cfg = this.opts.localVoiceActivity ?? true;
    if (cfg === false || this.localMeter) return;
    const meter = new AudioLevelMeter(
      stream,
      (sample) => {
        this.emit('localEnergy', sample);
        if (sample.changed) this.emit('localSpeaking', sample.speaking);
      },
      cfg === true ? {} : cfg,
    );
    if (meter.start()) this.localMeter = meter;
  }

  private stopLocalMeter(): void {
    if (!this.localMeter) return;
    const wasSpeaking = this.localMeter.speaking;
    this.localMeter.stop();
    this.localMeter = undefined;
    if (wasSpeaking) this.emit('localSpeaking', false);
  }

  /** Stream the peer connection sends: the gain pipeline's output or the raw microphone. */
  private sentStream(): MediaStream | undefined {
    return this.inputPipeline?.stream ?? this.localStream;
  }

  private async openMicrophone(): Promise<MediaStream> {
    const base: MediaTrackConstraints = this.opts.audioConstraints ?? {
      echoCancellation: true,
      noiseSuppression: true,
      autoGainControl: true,
    };
    const deviceId = this.inputDeviceIdValue;
    const audio: MediaTrackConstraints = deviceId ? { ...base, deviceId: { exact: deviceId } } : base;
    const stream = await navigator.mediaDevices.getUserMedia({ audio, video: false });
    return stream;
  }

  /** Stop tracks of a stream this client opened itself (never one supplied by the app). */
  private releaseLocalStream(stream: MediaStream | undefined): void {
    if (!stream || stream === this.opts.localStream) return;
    stream.getTracks().forEach((t) => t.stop());
  }

  private watchInputTrack(stream: MediaStream): void {
    const track = stream.getAudioTracks()[0];
    if (!track) return;
    track.addEventListener('ended', () => {
      // Device unplugged / revoked: fall back to the default microphone while media is up.
      if (this.localStream !== stream || !this.pc || stream === this.opts.localStream) return;
      this.inputDeviceIdValue = undefined;
      void this.openMicrophone()
        .then((s) => this.adoptLocalStream(s))
        .catch((e: unknown) => this.emit('error', e instanceof Error ? e : new Error(String(e))));
    });
  }

  /** Make `stream` the microphone: mute state, gain pipeline, uplink track and meter follow. */
  private async adoptLocalStream(stream: MediaStream): Promise<void> {
    const previous = this.localStream;
    this.localStream = stream;
    stream.getAudioTracks().forEach((t) => {
      t.enabled = !this.muted;
    });
    this.watchInputTrack(stream);
    if (this.inputPipeline) {
      this.inputPipeline.setSource(stream);
    } else {
      const track = stream.getAudioTracks()[0];
      const sender = this.pc?.getSenders().find((s) => s.track?.kind === 'audio' || s.track === null);
      if (track && sender) await sender.replaceTrack(track);
      this.stopLocalMeter();
      this.startLocalMeter(stream);
    }
    if (previous && previous !== stream) this.releaseLocalStream(previous);
    this.emit('inputDeviceChanged', this.inputDeviceId);
  }

  /** Route `raw` through the gain pipeline and send its output instead of the raw track. */
  private ensureInputPipeline(raw: MediaStream): void {
    if (this.inputPipeline) return;
    const pipeline = new InputPipeline();
    if (!pipeline.open(raw, this.inputGainValue)) {
      throw new Error('Web Audio is unavailable: input gain is not supported here');
    }
    this.inputPipeline = pipeline;
    const processed = pipeline.stream;
    const track = processed?.getAudioTracks()[0];
    const sender = this.pc?.getSenders().find((s) => s.track?.kind === 'audio' || s.track === null);
    if (track && sender) {
      void sender.replaceTrack(track).catch((e: unknown) => {
        this.emit('error', e instanceof Error ? e : new Error(String(e)));
      });
    }
    if (processed) {
      this.stopLocalMeter();
      this.startLocalMeter(processed);
    }
  }

  private applyOutput(element: HTMLMediaElement): void {
    element.autoplay = true;
    element.volume = this.outputVolumeValue;
    element.muted = this.outputMutedValue;
    if (element.srcObject !== (this.remoteStream ?? null)) element.srcObject = this.remoteStream ?? null;
    if (this.outputDeviceIdValue !== undefined && supportsOutputSelection()) {
      void element.setSinkId(this.outputDeviceIdValue).catch((e: unknown) => {
        this.emit('error', e instanceof Error ? e : new Error(String(e)));
      });
    }
  }

  private cancelReconnect(): void {
    if (this.reconnectTimer !== undefined) clearTimeout(this.reconnectTimer);
    this.reconnectTimer = undefined;
  }

  private handleMessage(msg: ServerMessage | UnknownMessage): void {
    this.emit('message', msg);
    switch (msg.type) {
      case 'SessionInitAck': {
        const d = (msg as Extract<ServerMessage, { type: 'SessionInitAck' }>).data;
        const info: SessionInfo = {
          sessionId: d.session_id,
          userId: this.userId ?? '',
          ssrc: d.ssrc,
          resumed: d.resumed === true,
        };
        this.resumeToken = d.resume_token || undefined;
        this.resumeGraceMs = d.resume_grace_ms ?? 0;
        const pending = this.pendingInit;
        this.pendingInit = undefined;
        if (pending) {
          clearTimeout(pending.timer);
          pending.resolve(info);
        }
        return;
      }
      case 'ChannelJoinAck': {
        const d = (msg as Extract<ServerMessage, { type: 'ChannelJoinAck' }>).data;
        const roster = new Map<string, Participant>();
        for (const p of d.participants) {
          roster.set(p.user_id, {
            userId: p.user_id,
            displayName: p.display_name,
            ssrc: p.ssrc,
            role: p.role,
            muted: p.is_muted,
            serverMuted: false,
            speaking: p.is_speaking,
            energy: 0,
          });
        }
        if (!this.userId && this.session) {
          const me = d.participants.find((p) => p.ssrc === this.session?.ssrc);
          if (me) this.userId = me.user_id;
        }
        this.channels.set(d.channel_id, roster);
        const list = Array.from(roster.values());
        const pending = this.pendingJoins.get(d.channel_id);
        if (pending) {
          clearTimeout(pending.timer);
          this.pendingJoins.delete(d.channel_id);
          pending.resolve(list);
        }
        this.emit('channelJoined', d.channel_id, list);
        if (this.muted) this.setMuted(true);
        this.replayChannelMutes(d.channel_id);
        return;
      }
      case 'ParticipantJoined': {
        const d = (msg as Extract<ServerMessage, { type: 'ParticipantJoined' }>).data;
        const roster = this.channels.get(d.channel_id);
        if (!roster) return;
        const p: Participant = {
          userId: d.user_id,
          displayName: d.display_name,
          ssrc: d.ssrc,
          role: 'unknown',
          muted: false,
          serverMuted: false,
          speaking: false,
          energy: 0,
        };
        roster.set(d.user_id, p);
        this.emit('participantJoined', d.channel_id, p);
        return;
      }
      case 'ParticipantLeft': {
        const d = (msg as Extract<ServerMessage, { type: 'ParticipantLeft' }>).data;
        this.channels.get(d.channel_id)?.delete(d.user_id);
        this.emit('participantLeft', d.channel_id, d.user_id);
        return;
      }
      case 'MuteStateChanged': {
        const d = (msg as Extract<ServerMessage, { type: 'MuteStateChanged' }>).data;
        const p = this.channels.get(d.channel_id)?.get(d.user_id);
        if (!p) return;
        p.muted = d.muted;
        p.serverMuted = d.server_muted;
        this.emit('participantUpdated', d.channel_id, p);
        return;
      }
      case 'SpeakingStateChanged': {
        const d = (msg as Extract<ServerMessage, { type: 'SpeakingStateChanged' }>).data;
        const p = this.channels.get(d.channel_id)?.get(d.user_id);
        if (p) {
          p.speaking = d.speaking;
          if (!d.speaking) p.energy = 0;
          this.emit('participantUpdated', d.channel_id, p);
        }
        this.emit('speaking', d.channel_id, d.user_id, d.speaking);
        return;
      }
      case 'ChannelEnergy': {
        const d = (msg as Extract<ServerMessage, { type: 'ChannelEnergy' }>).data;
        const roster = this.channels.get(d.channel_id);
        if (!roster) return;
        for (const l of d.levels) {
          const p = roster.get(l.user_id);
          if (p) p.energy = Math.min(1, Math.max(0, l.energy));
        }
        this.emit('energy', d.channel_id, d.levels);
        return;
      }
      case 'PositionUpdate': {
        const d = (msg as Extract<ServerMessage, { type: 'PositionUpdate' }>).data;
        this.emit('positions', d.channel_id, d.positions);
        return;
      }
      case 'RecordingNotification': {
        const d = (msg as Extract<ServerMessage, { type: 'RecordingNotification' }>).data;
        this.emit('recording', d.channel_id, d.recording_id, d.active, d.initiated_by);
        return;
      }
      case 'BitrateCommand': {
        const d = (msg as Extract<ServerMessage, { type: 'BitrateCommand' }>).data;
        void this.applyBitrate(d.target_bitrate_kbps);
        this.emit('bitrate', d.target_bitrate_kbps, d.reason);
        return;
      }
      case 'Kick': {
        const d = (msg as Extract<ServerMessage, { type: 'Kick' }>).data;
        if (this.channels.delete(d.channel_id)) this.emit('channelLeft', d.channel_id);
        this.emit('kicked', d.channel_id, d.reason);
        return;
      }
      case 'UserBlockChanged': {
        const d = (msg as Extract<ServerMessage, { type: 'UserBlockChanged' }>).data;
        if (d.blocked) this.blockedUsers.add(d.user_id);
        else this.blockedUsers.delete(d.user_id);
        this.emit('userBlockChanged', d.user_id, d.blocked);
        return;
      }
      case 'TransmissionChanged': {
        const d = (msg as Extract<ServerMessage, { type: 'TransmissionChanged' }>).data;
        this.transmission = transmissionFromWire(d.mode);
        this.emit('transmissionChanged', this.transmission);
        return;
      }
      case 'ChannelFocusChanged': {
        const d = (msg as Extract<ServerMessage, { type: 'ChannelFocusChanged' }>).data;
        this.focusChannel = d.channel_id ?? undefined;
        this.emit('channelFocusChanged', this.focusChannel);
        return;
      }
      case 'ReceiverPreferences': {
        const d = (msg as Extract<ServerMessage, { type: 'ReceiverPreferences' }>).data;
        this.blockedUsers = new Set(d.blocked_users);
        // A resumed session already holds our mutes/volumes; a fresh one is replayed by
        // `replayReceiverPrefs`, so only merge what the server reports on top.
        for (const m of d.local_mutes) {
          const scopes = this.localMutes.get(m.user_id) ?? new Set<string>();
          scopes.add(m.channel_id ?? ALL_CHANNELS);
          this.localMutes.set(m.user_id, scopes);
        }
        for (const v of d.volumes) {
          if (v.volume === 1) this.volumes.delete(v.user_id);
          else this.volumes.set(v.user_id, v.volume);
        }
        // A resumed session reports the policy it still holds; a fresh one reports the
        // defaults, which `replayReceiverPrefs`/`replayChannelMutes` then override.
        const transmission = transmissionFromWire(d.transmission);
        const focusChannel = d.focus_channel ?? undefined;
        if (this.session?.resumed) {
          this.transmission = transmission;
          this.focusChannel = focusChannel;
        }
        this.emit('receiverPreferences', {
          blockedUsers: d.blocked_users,
          localMutes: d.local_mutes,
          volumes: d.volumes,
          transmission,
          focusChannel,
        });
        return;
      }
      case 'WebRtcAnswer': {
        const d = (msg as Extract<ServerMessage, { type: 'WebRtcAnswer' }>).data;
        const pending = this.pendingAnswer;
        this.pendingAnswer = undefined;
        if (pending) {
          clearTimeout(pending.timer);
          pending.resolve(d.sdp);
        }
        return;
      }
      case 'Pong': {
        const d = (msg as Extract<ServerMessage, { type: 'Pong' }>).data;
        this.lastPongAt = performance.now();
        if (d.nonce === this.pingNonce - 1 && this.lastPingSentAt > 0) {
          this.rttMs = this.lastPongAt - this.lastPingSentAt;
        }
        return;
      }
      case 'Error': {
        const d = (msg as Extract<ServerMessage, { type: 'Error' }>).data;
        if (d.client_ref !== undefined) {
          const pending = this.pendingChat.get(d.client_ref);
          if (pending) {
            clearTimeout(pending.timer);
            this.pendingChat.delete(d.client_ref);
            pending.reject(new Error(`${d.code}: ${d.message}`));
          }
          this.emit('serverError', d.code, d.message);
          return;
        }
        // A failed join/moderation/offer is reported as a generic Error; fail the oldest
        // pending request.
        const join = this.pendingJoins.entries().next();
        const moderation = this.pendingModerations.entries().next();
        if (!join.done) {
          const [channelId, pending] = join.value;
          clearTimeout(pending.timer);
          this.pendingJoins.delete(channelId);
          pending.reject(new Error(`${d.code}: ${d.message}`));
        } else if (!moderation.done) {
          const [key, pending] = moderation.value;
          clearTimeout(pending.timer);
          this.pendingModerations.delete(key);
          pending.reject(new Error(`${d.code}: ${d.message}`));
        } else if (this.pendingAnswer) {
          const pending = this.pendingAnswer;
          this.pendingAnswer = undefined;
          clearTimeout(pending.timer);
          pending.reject(new Error(`${d.code}: ${d.message}`));
        }
        this.emit('serverError', d.code, d.message);
        return;
      }
      case 'ChatMessageReceived': {
        const d = (msg as Extract<ServerMessage, { type: 'ChatMessageReceived' }>).data;
        const message = this.toChatMessage(d.message);
        if (message.own && message.clientRef !== undefined) {
          const pending = this.pendingChat.get(message.clientRef);
          if (pending) {
            clearTimeout(pending.timer);
            this.pendingChat.delete(message.clientRef);
            pending.resolve(message);
          }
        }
        this.emit('chatMessage', message);
        return;
      }
      case 'ParticipantTyping': {
        const d = (msg as Extract<ServerMessage, { type: 'ParticipantTyping' }>).data;
        this.emit('participantTyping', d.channel_id, d.user_id, d.typing);
        return;
      }
      case 'ModerateParticipantAck': {
        const d = (msg as Extract<ServerMessage, { type: 'ModerateParticipantAck' }>).data;
        const key = `${d.channel_id}/${d.user_id}/${d.action}`;
        const pending = this.pendingModerations.get(key);
        if (pending) {
          clearTimeout(pending.timer);
          this.pendingModerations.delete(key);
          pending.resolve();
        }
        return;
      }
      case 'SessionClose': {
        const d = (msg as Extract<ServerMessage, { type: 'SessionClose' }>).data;
        this.emit('sessionClosed', d.reason);
        this.disconnect('closed by server');
        return;
      }
      case 'MediaBound':
      default:
        return;
    }
  }

  // ── Internals: media ──

  private async startMedia(): Promise<void> {
    this.setState('media-connecting');
    const iceServers: RTCIceServer[] = [...(this.opts.iceServers ?? [])];
    if (this.opts.useTurn) {
      const turn = await this.fetchTurnCredentials();
      if (turn) {
        iceServers.push({ urls: turn.uris, username: turn.username, credential: turn.password });
      }
    }

    if (!this.localStream) {
      this.localStream = this.opts.localStream ?? (await this.openMicrophone());
      this.watchInputTrack(this.localStream);
    }
    this.localStream.getAudioTracks().forEach((t) => {
      t.enabled = !this.muted;
    });
    if (this.inputGainValue !== 1 && !this.inputPipeline) {
      const pipeline = new InputPipeline();
      if (pipeline.open(this.localStream, this.inputGainValue)) this.inputPipeline = pipeline;
      else this.emit('error', new Error('Web Audio is unavailable: input gain ignored'));
    }
    const sent = this.sentStream() ?? this.localStream;
    this.startLocalMeter(sent);
    if (typeof navigator !== 'undefined') {
      navigator.mediaDevices?.addEventListener?.('devicechange', this.onDeviceChange);
    }

    const pc = new RTCPeerConnection({ iceServers, bundlePolicy: 'max-bundle', rtcpMuxPolicy: 'require' });
    this.pc = pc;
    // One sendrecv audio transceiver: uplink microphone, downlink server-side mix.
    const track = sent.getAudioTracks()[0];
    if (!track) throw new Error('no audio track');
    pc.addTransceiver(track, { direction: 'sendrecv', streams: [sent] });

    pc.ontrack = (ev) => {
      const stream = ev.streams[0] ?? new MediaStream([ev.track]);
      this.remoteStream = stream;
      stream.getAudioTracks().forEach((t) => {
        t.enabled = !this.outputMutedValue;
      });
      for (const el of this.outputElements) {
        this.applyOutput(el);
        if (el.paused) void el.play().catch(() => undefined);
      }
      this.emit('remoteStream', stream);
    };
    pc.onconnectionstatechange = () => {
      if (this.pc !== pc) return;
      switch (pc.connectionState) {
        case 'connected':
          if (this.state === 'connected' || this.state === 'media-connecting') {
            this.setState('media-connected');
          }
          break;
        case 'failed':
          // ICE gave up while the control channel may still be fine: negotiate a new
          // transport for the same session (the server replaces the old one).
          this.emit('error', new Error('WebRTC connection failed'));
          if (this.ws && this.ws.readyState === WebSocket.OPEN && this.opts.autoReconnect) {
            void this.restoreMedia(false).catch((e: unknown) => {
              this.emit('error', e instanceof Error ? e : new Error(String(e)));
              this.teardown('media renegotiation failed', 'failed');
            });
          } else if (this.state !== 'reconnecting') {
            this.setState('failed');
          }
          break;
        case 'disconnected':
          if (this.state === 'media-connected') this.setState('connected');
          break;
        default:
          break;
      }
    };

    const offer = await pc.createOffer();
    await pc.setLocalDescription({ type: 'offer', sdp: preferStereoOpus(offer.sdp ?? '') });
    // The server is ICE-lite and answers with its own host candidate, so no trickle needed:
    // wait for local gathering to finish so the offer carries our candidates.
    await this.waitForIceGathering(pc);
    const localSdp = pc.localDescription?.sdp;
    if (!localSdp) throw new Error('missing local description');

    const answerSdp = await this.requestAnswer(localSdp);
    await pc.setRemoteDescription({ type: 'answer', sdp: answerSdp });
  }

  private requestAnswer(sdp: string): Promise<string> {
    this.requireOpen();
    return new Promise<string>((resolve, reject) => {
      const timer = setTimeout(() => {
        this.pendingAnswer = undefined;
        reject(new Error('WebRTC offer timed out'));
      }, this.opts.requestTimeoutMs);
      this.pendingAnswer = { resolve, reject, timer };
      this.send({ type: 'WebRtcOffer', data: { sdp } });
    });
  }

  private waitForIceGathering(pc: RTCPeerConnection): Promise<void> {
    if (pc.iceGatheringState === 'complete') return Promise.resolve();
    return new Promise<void>((resolve) => {
      const done = () => {
        pc.removeEventListener('icegatheringstatechange', check);
        clearTimeout(timer);
        resolve();
      };
      const check = () => {
        if (pc.iceGatheringState === 'complete') done();
      };
      const timer = setTimeout(done, 2_000);
      pc.addEventListener('icegatheringstatechange', check);
    });
  }

  private async applyBitrate(kbps: number): Promise<void> {
    const sender = this.pc?.getSenders().find((s) => s.track?.kind === 'audio');
    if (!sender) return;
    const params = sender.getParameters();
    if (!params.encodings || params.encodings.length === 0) params.encodings = [{}];
    const first = params.encodings[0];
    if (first) first.maxBitrate = kbps * 1000;
    try {
      await sender.setParameters(params);
    } catch (e) {
      this.emit('error', e instanceof Error ? e : new Error(String(e)));
    }
  }

  private async fetchTurnCredentials(): Promise<TurnCredentials | undefined> {
    try {
      const res = await fetch(`${this.opts.apiUrl.replace(/\/$/, '')}/v1/me/turn-credentials`, {
        headers: { Authorization: `Bearer ${this.opts.token}` },
      });
      if (!res.ok) return undefined;
      const body: unknown = await res.json();
      if (
        typeof body === 'object' &&
        body !== null &&
        'username' in body &&
        'password' in body &&
        'uris' in body
      ) {
        return body as TurnCredentials;
      }
      return undefined;
    } catch {
      return undefined;
    }
  }

  // ── Internals: misc ──

  private startPing(): void {
    this.stopPing();
    if (this.opts.pingIntervalMs <= 0) return;
    this.lastPongAt = performance.now();
    this.pingTimer = setInterval(() => {
      const ws = this.ws;
      if (!ws || ws.readyState !== WebSocket.OPEN) return;
      // Two missed pongs: the TCP connection is probably dead without the browser noticing.
      if (performance.now() - this.lastPongAt > 2.5 * this.opts.pingIntervalMs) {
        ws.close(4001, 'keepalive timeout');
        return;
      }
      const nonce = this.pingNonce++;
      this.lastPingSentAt = performance.now();
      this.trySend({ type: 'Ping', data: { nonce } });
    }, this.opts.pingIntervalMs);
  }

  private stopPing(): void {
    if (this.pingTimer !== undefined) clearInterval(this.pingTimer);
    this.pingTimer = undefined;
  }

  private setState(state: ConnectionState): void {
    if (this.state === state) return;
    this.state = state;
    this.emit('connectionState', state);
  }

  private requireOpen(): void {
    if (!this.ws || this.ws.readyState !== WebSocket.OPEN) throw new Error('not connected');
  }

  private toChatMessage(w: ChatMessageWire): ChatMessage {
    // `client_ref` is only echoed to the sender, so its presence (or a pending ref) marks
    // our own messages even before the user id is known from a channel join.
    const clientRef = w.client_ref ?? undefined;
    const own =
      (this.userId !== undefined && w.from_user_id === this.userId) ||
      (clientRef !== undefined && this.pendingChat.has(clientRef));
    const m: ChatMessage = {
      id: w.id,
      fromUserId: w.from_user_id,
      displayName: w.display_name,
      text: w.text,
      sentAt: new Date(w.sent_at),
      own,
      system: w.from_user_id === SYSTEM_USER_ID,
    };
    if (w.channel_id != null) m.channelId = w.channel_id;
    if (w.to_user_id != null) m.toUserId = w.to_user_id;
    if (w.metadata != null) m.metadata = w.metadata;
    if (clientRef !== undefined) m.clientRef = clientRef;
    return m;
  }

  private send(msg: ClientMessage): void {
    this.requireOpen();
    this.ws?.send(JSON.stringify(msg));
  }

  private trySend(msg: ClientMessage): void {
    try {
      this.send(msg);
    } catch {
      /* connection is going away */
    }
  }

  private rejectPending<T>(p: Pending<T> | undefined, reason: string): void {
    if (!p) return;
    clearTimeout(p.timer);
    p.reject(new Error(reason));
  }
}
