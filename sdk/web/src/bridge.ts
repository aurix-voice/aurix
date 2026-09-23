import { AurixClient } from './client.js';
import type {
  AurixClientOptions,
  AurixEvents,
  ChatScope,
  E2eeOptions,
  ConnectionState,
  EditMessageOptions,
  HistoryOptions,
  ParticipantStreamInfo,
  ReconnectPolicy,
  SearchOptions,
  SendMessageOptions,
  SpeakOptions,
  TransmissionMode,
} from './client.js';
import { enumerateAudioDevices } from './devices.js';
import { base64ToBytes, bytesToBase64 } from './e2ee.js';
import type { E2eeTransformApi } from './e2ee.js';
import type { VoiceEffectParams, VoiceEffectPreset } from './effects.js';
import type { OpusBrowserOptions } from './opus.js';
import type { JsonValue, ModerationAction, Orientation3D, Position3D, RecordingConsent } from './protocol.js';

/**
 * Handle-based, JSON-in / JSON-out façade over {@link AurixClient} for hosts that cannot hold
 * JavaScript objects or callbacks: the Unity WebGL plugin (`AurixWebGL.jslib`), other
 * WebAssembly runtimes, or a `postMessage` boundary.
 *
 * - `create(optionsJson)` returns an integer handle; `invoke(handle, method, argsJson, rid)` calls a
 *   client method. Synchronous results come back as `{"ok":true,"value":…}`; a method that returns a
 *   promise answers `{"ok":true,"pending":true}` and settles later as a `result` event carrying `rid`.
 * - Every client event is queued as `{"type": <event name>, …named arguments}`; the host polls
 *   `drain(handle)` (e.g. once per frame) and receives them in order as a JSON array.
 * - Credential callbacks are inverted: with `refreshToken: true` / `joinToken: true` in the options,
 *   the bridge queues a `tokenRequest` event (`kind: "refresh" | "join"`, `channelId`) and waits for
 *   `provideToken {requestId, token}` (or `{requestId, error}`).
 * - The SFU's mixed audio is played through an `<audio>` element the bridge owns (the host has no
 *   media stack); browsers require a user gesture before it may start — see `remoteAudio` events.
 */
export interface BridgeEvent {
  type: string;
  [field: string]: unknown;
}

/** Wire form of the bridge's options (`create`). Everything but the endpoints and token is optional. */
export interface BridgeClientOptions {
  apiUrl: string;
  wsUrl: string;
  token: string;
  /** Ask the host for a fresh credential before every reconnect (`tokenRequest {kind: "refresh"}`). */
  refreshToken?: boolean;
  /** Ask the host for a join token when `joinChannel` is called without one (`tokenRequest {kind: "join"}`). */
  joinToken?: boolean;
  useTurn?: boolean;
  iceServers?: RTCIceServer[];
  audioConstraints?: MediaTrackConstraints;
  inputDeviceId?: string;
  inputGain?: number;
  opus?: OpusBrowserOptions;
  localVoiceActivity?: boolean;
  pingIntervalMs?: number;
  qualityReportIntervalMs?: number;
  requestTimeoutMs?: number;
  autoReconnect?: boolean;
  reconnect?: Partial<ReconnectPolicy>;
  /** Also queue the raw `message` event (every server message) — verbose; off by default. */
  rawMessages?: boolean;
  /** Queue `localEnergy` samples (~20/s while media is up); off by default, `localSpeaking` is always queued. */
  localEnergyEvents?: boolean;
  /** Per-participant downlink tracks to negotiate (see `AurixClientOptions.participantStreams`). */
  participantStreams?: number;
  /** `true` (default) HRTF, `'equalpower'`, or `false` for mixed-only style rendering by the host. */
  spatialAudio?: boolean | 'equalpower';
  /**
   * Group E2EE (see `AurixClientOptions.e2ee`): `true`/`false`, or an object whose `identity` is
   * the base64 32-byte secret exported earlier by `e2eeIdentitySecret`.
   */
  e2ee?: boolean | { identity?: string; transform?: 'auto' | E2eeTransformApi; workerUrl?: string };
  /** Stable installation id for the per-device chat delivery cursor (see `AurixClientOptions.deviceId`). */
  deviceId?: string;
  /** Local lip-sync analysis from the start (see `AurixClientOptions.visemes`). */
  visemes?: boolean;
  /**
   * Queue `participantVisemes` / `localVisemes` frames (50/s per analysed voice); off by default —
   * hosts usually poll `participantVisemes` / `localVisemes` once per rendered frame instead.
   */
  visemeEvents?: boolean;
  /** Microphone voice effects from the start: a preset name or explicit parameters. */
  voiceEffects?: VoiceEffectParams | VoiceEffectPreset;
}

export interface AurixBridgeOptions {
  /** Client factory (tests substitute a fake). */
  createClient?: (options: AurixClientOptions) => AurixClient;
  /** Document used to create the playback `<audio>` element; `null` disables playback (tests, workers). */
  document?: Document | null;
  /** Queue depth per client before the oldest events are dropped (an `overflow` event reports the count). */
  maxQueuedEvents?: number;
}

interface Entry {
  client: AurixClient;
  queue: BridgeEvent[];
  dropped: number;
  audio: HTMLAudioElement | undefined;
  nextTokenRequest: number;
  tokenRequests: Map<number, { resolve: (token: string) => void; reject: (error: Error) => void }>;
  unsubscribe: Array<() => void>;
}

type Args = Record<string, unknown>;

const DEFAULT_MAX_QUEUED = 4096;

export class AurixBridge {
  private readonly clients = new Map<number, Entry>();
  private nextHandle = 1;
  private readonly createClient: (options: AurixClientOptions) => AurixClient;
  private readonly document: Document | null;
  private readonly maxQueued: number;

  constructor(options: AurixBridgeOptions = {}) {
    this.createClient = options.createClient ?? ((o) => new AurixClient(o));
    this.document =
      options.document !== undefined ? options.document : typeof document === 'undefined' ? null : document;
    this.maxQueued = Math.max(16, options.maxQueuedEvents ?? DEFAULT_MAX_QUEUED);
  }

  /** Number of live clients (diagnostics). */
  get size(): number {
    return this.clients.size;
  }

  /** Create a client from JSON {@link BridgeClientOptions}; returns its handle (> 0). Throws on invalid options. */
  create(optionsJson: string): number {
    const raw = parseArgs(optionsJson) as Partial<BridgeClientOptions>;
    if (typeof raw.apiUrl !== 'string' || typeof raw.wsUrl !== 'string' || typeof raw.token !== 'string') {
      throw new Error('apiUrl, wsUrl and token are required');
    }
    const handle = this.nextHandle++;
    const entry: Entry = {
      client: undefined as unknown as AurixClient,
      queue: [],
      dropped: 0,
      audio: undefined,
      nextTokenRequest: 1,
      tokenRequests: new Map(),
      unsubscribe: [],
    };
    const options: AurixClientOptions = { apiUrl: raw.apiUrl, wsUrl: raw.wsUrl, token: raw.token };
    if (raw.useTurn !== undefined) options.useTurn = raw.useTurn;
    if (raw.iceServers !== undefined) options.iceServers = raw.iceServers;
    if (raw.audioConstraints !== undefined) options.audioConstraints = raw.audioConstraints;
    if (raw.inputDeviceId !== undefined) options.inputDeviceId = raw.inputDeviceId;
    if (raw.inputGain !== undefined) options.inputGain = raw.inputGain;
    if (raw.opus !== undefined) options.opus = raw.opus;
    if (raw.localVoiceActivity !== undefined) options.localVoiceActivity = raw.localVoiceActivity;
    if (raw.pingIntervalMs !== undefined) options.pingIntervalMs = raw.pingIntervalMs;
    if (raw.qualityReportIntervalMs !== undefined) options.qualityReportIntervalMs = raw.qualityReportIntervalMs;
    if (raw.requestTimeoutMs !== undefined) options.requestTimeoutMs = raw.requestTimeoutMs;
    if (raw.autoReconnect !== undefined) options.autoReconnect = raw.autoReconnect;
    if (raw.reconnect !== undefined) options.reconnect = raw.reconnect;
    if (raw.participantStreams !== undefined) options.participantStreams = raw.participantStreams;
    if (raw.spatialAudio !== undefined) options.spatialAudio = raw.spatialAudio;
    if (raw.e2ee !== undefined) options.e2ee = e2eeOptions(raw.e2ee);
    if (raw.deviceId !== undefined && raw.deviceId !== null && raw.deviceId !== '') options.deviceId = raw.deviceId;
    if (raw.visemes !== undefined) options.visemes = raw.visemes;
    if (raw.voiceEffects !== undefined) options.voiceEffects = raw.voiceEffects;
    if (raw.refreshToken) options.refreshToken = () => this.requestToken(entry, 'refresh', undefined);
    if (raw.joinToken) options.joinToken = (channelId) => this.requestToken(entry, 'join', channelId);

    entry.client = this.createClient(options);
    this.clients.set(handle, entry);
    this.subscribe(entry, raw.rawMessages === true, raw.localEnergyEvents === true, raw.visemeEvents === true);
    return handle;
  }

  /** Disconnect (if needed), drop the playback element and forget the handle. Unknown handles are ignored. */
  destroy(handle: number): void {
    const entry = this.clients.get(handle);
    if (!entry) return;
    this.clients.delete(handle);
    for (const off of entry.unsubscribe) off();
    for (const req of entry.tokenRequests.values()) req.reject(new Error('client destroyed'));
    entry.tokenRequests.clear();
    try {
      if (entry.client.connectionState !== 'disconnected') entry.client.disconnect('client destroyed');
    } catch {
      // already gone
    }
    if (entry.audio) {
      entry.client.detachAudioOutput(entry.audio);
      entry.audio.srcObject = null;
      entry.audio.remove();
      entry.audio = undefined;
    }
  }

  /**
   * Call `method` with the JSON `argsJson` object. Returns `{"ok":true,"value":…}` for synchronous
   * results, `{"ok":true,"pending":true}` for promises (settled as a `result` event with `rid`, or
   * — with `rid` 0 — reported only as an `error` event on failure) and `{"ok":false,"error":…}` on
   * a synchronous failure or an unknown method/handle.
   */
  invoke(handle: number, method: string, argsJson: string, rid = 0): string {
    const entry = this.clients.get(handle);
    if (!entry) return JSON.stringify({ ok: false, error: { message: `unknown handle ${handle}` } });
    let result: unknown;
    try {
      const args = parseArgs(argsJson);
      result = this.dispatch(entry, method, args);
    } catch (e) {
      return JSON.stringify({ ok: false, error: errorInfo(e) });
    }
    if (isPromise(result)) {
      result.then(
        (value) => {
          if (rid > 0) this.push(entry, { type: 'result', rid, ok: true, value: jsonSafe(value) });
        },
        (e: unknown) => {
          if (rid > 0) this.push(entry, { type: 'result', rid, ok: false, error: errorInfo(e) });
          else this.push(entry, { type: 'error', error: errorInfo(e), method });
        },
      );
      return JSON.stringify({ ok: true, pending: true });
    }
    return JSON.stringify({ ok: true, value: jsonSafe(result) });
  }

  /** Take every queued event (oldest first) as a JSON array; `"[]"` when nothing happened or the handle is unknown. */
  drain(handle: number): string {
    const entry = this.clients.get(handle);
    if (!entry || entry.queue.length === 0) return '[]';
    const events = entry.queue;
    entry.queue = [];
    if (entry.dropped > 0) {
      events.unshift({ type: 'overflow', dropped: entry.dropped });
      entry.dropped = 0;
    }
    return JSON.stringify(events);
  }

  /** Queued events not yet drained. */
  pending(handle: number): number {
    return this.clients.get(handle)?.queue.length ?? 0;
  }

  // ── Dispatch ──

  private dispatch(entry: Entry, method: string, a: Args): unknown {
    const c = entry.client;
    switch (method) {
      // Lifecycle
      case 'connect':
        return c.connect();
      case 'disconnect':
        c.disconnect(optString(a, 'reason') ?? 'client disconnect');
        return null;
      case 'reconnectNow':
        c.reconnectNow();
        return null;
      case 'connectionState':
        return c.connectionState;
      case 'sessionInfo':
        return c.sessionInfo ?? null;
      case 'endpoint':
        return c.endpoint;
      case 'failover':
        return c.failover;
      case 'resumeGrace':
        return c.resumeGrace;
      case 'provideToken':
        this.provideToken(entry, a);
        return null;

      // Channels
      case 'joinChannel': {
        const token = optString(a, 'joinToken');
        return token === undefined ? c.joinChannel(str(a, 'channelId')) : c.joinChannel(str(a, 'channelId'), token);
      }
      case 'leaveChannel':
        c.leaveChannel(str(a, 'channelId'));
        return null;
      case 'moderate':
        return c.moderate(
          str(a, 'channelId'),
          str(a, 'userId'),
          str(a, 'action') as ModerationAction,
          str(a, 'token'),
          optString(a, 'reason'),
        );
      case 'participants':
        return c.participants(str(a, 'channelId'));
      case 'joinedChannels':
        return c.joinedChannels();
      case 'channelInfo':
        return c.channelInfo(str(a, 'channelId')) ?? null;
      case 'channelScope':
        return c.channelScope(str(a, 'channelId')) ?? null;
      case 'canSpeakIn':
        return c.canSpeakIn(str(a, 'channelId'));
      case 'isChannelTranscribed':
        return c.isChannelTranscribed(str(a, 'channelId'));
      case 'isChannelMonitored':
        return c.isChannelMonitored(str(a, 'channelId'));
      case 'audioPolicy':
        return c.audioPolicy ?? null;

      // Microphone / receiver preferences
      case 'setMuted':
        c.setMuted(bool(a, 'muted'));
        return null;
      case 'isMuted':
        return c.isMuted;
      case 'setParticipantMuted':
        c.setParticipantMuted(str(a, 'userId'), bool(a, 'muted'), optString(a, 'channelId'));
        return null;
      case 'isParticipantMuted':
        return c.isParticipantMuted(str(a, 'userId'), optString(a, 'channelId'));
      case 'setParticipantVolume':
        c.setParticipantVolume(str(a, 'userId'), num(a, 'volume'));
        return null;
      case 'getParticipantVolume':
        return c.getParticipantVolume(str(a, 'userId'));
      case 'setUserBlocked':
        c.setUserBlocked(str(a, 'userId'), bool(a, 'blocked'));
        return null;
      case 'isUserBlocked':
        return c.isUserBlocked(str(a, 'userId'));
      case 'blockedUsers':
        return c.getBlockedUsers();
      case 'setTransmission':
        c.setTransmission(transmission(a['mode']));
        return null;
      case 'transmission':
        return c.getTransmission();
      case 'transmitsTo':
        return c.transmitsTo(str(a, 'channelId'));
      case 'setChannelFocus':
        c.setChannelFocus(optString(a, 'channelId'));
        return null;
      case 'setPinnedParticipants':
        c.setPinnedParticipants(strArray(a, 'userIds'));
        return null;
      case 'pinnedParticipants':
        return c.getPinnedParticipants();
      case 'participantStreamCap':
        return c.participantStreamCap;
      case 'participantStreams':
        return participantStreamsEvent(c.getParticipantStreams());
      case 'isParticipantSpatialized':
        return c.isParticipantSpatialized(str(a, 'userId'));
      case 'setPriority':
        c.setPriority(str(a, 'channelId'), bool(a, 'priority'), optString(a, 'userId'));
        return null;
      case 'isPriority':
        return c.isPriority(str(a, 'channelId'));
      case 'isWaitingToSpeak':
        return c.isWaitingToSpeak(str(a, 'channelId'));
      case 'channelDucking':
        return c.getChannelDucking(str(a, 'channelId')) ?? null;
      case 'isDuckingActive':
        return c.isDuckingActive(str(a, 'channelId'));
      case 'channelFocus':
        return c.getChannelFocus() ?? null;
      case 'setTranscripts':
        c.setTranscripts(bool(a, 'enabled'));
        return null;
      case 'transcriptsEnabled':
        return c.transcriptsEnabled;
      case 'setServerNoiseSuppression':
        c.setServerNoiseSuppression(bool(a, 'enabled'));
        return null;
      case 'serverNoiseSuppression':
        return c.serverNoiseSuppression;
      case 'setTranslation': {
        const spokenLanguage = optString(a, 'spokenLanguage');
        c.setTranslation(optString(a, 'language'), {
          ...(spokenLanguage !== undefined ? { spokenLanguage } : {}),
          speech: a['speech'] === true,
        });
        return null;
      }
      case 'translationPrefs':
        return c.translationPrefs;

      // Text
      case 'sendMessage':
        return c.sendMessage(str(a, 'channelId'), str(a, 'text'), messageOptions(a));
      case 'sendDirectMessage':
        return c.sendDirectMessage(str(a, 'userId'), str(a, 'text'), messageOptions(a));
      case 'setTyping':
        c.setTyping(str(a, 'channelId'), bool(a, 'typing'), optNumber(a, 'intervalMs') ?? 1500);
        return null;
      case 'history':
        return c.history(chatScope(a), historyOptions(a));
      case 'markRead':
        c.markRead(chatScope(a), str(a, 'messageId'));
        return null;
      case 'readMarkers':
        return c.readMarkers(chatScope(a));
      case 'editMessage': {
        const options: EditMessageOptions = {};
        if (a['metadata'] !== undefined) options.metadata = a['metadata'] as JsonValue;
        return c.editMessage(str(a, 'messageId'), str(a, 'text'), options);
      }
      case 'deleteMessage':
        return c.deleteMessage(str(a, 'messageId'));
      case 'react':
        c.react(str(a, 'messageId'), str(a, 'reaction'), a['add'] !== false);
        return null;
      case 'search': {
        const options: SearchOptions = {};
        const fromUserId = optString(a, 'fromUserId');
        const before = optString(a, 'before');
        const limit = optNumber(a, 'limit');
        if (fromUserId !== undefined) options.fromUserId = fromUserId;
        if (before !== undefined) options.before = before;
        if (limit !== undefined) options.limit = limit;
        return c.search(chatScope(a), str(a, 'query'), options);
      }

      // Speech
      case 'speak': {
        const options: SpeakOptions = {};
        const channelId = optString(a, 'channelId');
        const voice = optString(a, 'voice');
        const destination = optString(a, 'destination');
        const clientRef = optString(a, 'clientRef');
        if (channelId !== undefined) options.channelId = channelId;
        if (voice !== undefined) options.voice = voice;
        if (destination !== undefined) options.destination = destination as NonNullable<SpeakOptions['destination']>;
        if (clientRef !== undefined) options.clientRef = clientRef;
        return c.speak(str(a, 'text'), options).then((req) => ({ requestId: req.requestId, clientRef: req.clientRef }));
      }
      case 'cancelSpeech':
        c.cancelSpeech();
        return null;

      // Positional / recording / quality
      case 'updatePosition':
        c.updatePosition(str(a, 'channelId'), a['position'] as Position3D, a['orientation'] as Orientation3D);
        return null;
      case 'respondToRecording':
        c.respondToRecording(str(a, 'recordingId'), str(a, 'consent') as RecordingConsent);
        return null;
      case 'getStats':
        return c.getStats();
      case 'lastStats':
        return c.lastStats ?? null;
      case 'reportQuality':
        return c.reportQuality();
      case 'networkQuality':
        return c.networkQuality ?? null;
      case 'roundTripMs':
        return c.roundTripMs;

      // Devices / local audio
      case 'enumerateDevices':
        return enumerateAudioDevices();
      case 'setInputDevice':
        return c.setInputDevice(optString(a, 'deviceId'));
      case 'inputDeviceId':
        return c.inputDeviceId ?? null;
      case 'setInputGain':
        c.setInputGain(num(a, 'gain'));
        return null;
      case 'inputGain':
        return c.inputGain;
      case 'setOutputDevice':
        return c.setOutputDevice(optString(a, 'deviceId'));
      case 'outputDeviceId':
        return c.outputDeviceId ?? null;
      case 'setOutputVolume':
        c.setOutputVolume(num(a, 'volume'));
        return null;
      case 'outputVolume':
        return c.outputVolume;
      case 'setOutputMuted':
        c.setOutputMuted(bool(a, 'muted'));
        return null;
      case 'isOutputMuted':
        return c.isOutputMuted;
      case 'localEnergy':
        return c.localEnergy;
      case 'localSpeaking':
        return c.localSpeaking;
      case 'setOpusOptions':
        c.setOpusOptions(a['opus'] as OpusBrowserOptions | undefined);
        return null;
      case 'renegotiateMedia':
        return c.renegotiateMedia();
      case 'resumeAudio':
        return this.resumeAudio(entry);
      case 'supportsVoiceEffects':
        return AurixClient.supportsVoiceEffects();
      case 'setVoiceEffects':
        return c.setVoiceEffects(voiceEffects(a['effects']));
      case 'voiceEffects':
        return c.voiceEffects;
      case 'supportsVisemes':
        return AurixClient.supportsVisemes();
      case 'setVisemes':
        return c.setVisemes(bool(a, 'enabled'));
      case 'visemesEnabled':
        return c.visemesEnabled;
      case 'participantVisemes':
        return c.getParticipantVisemes(str(a, 'userId')) ?? null;
      case 'localVisemes':
        return c.getLocalVisemes() ?? null;
      case 'e2eeAvailable':
        return c.e2eeAvailable;
      case 'e2eeTransformApi':
        return c.e2eeTransformApi ?? null;
      case 'e2eeFingerprint':
        return c.e2eeFingerprint ?? null;
      case 'e2eeIdentitySecret': {
        const secret = c.e2eeIdentitySecret;
        return secret ? bytesToBase64(secret) : null;
      }
      case 'e2eePeerFingerprint':
        return c.e2eePeerFingerprint(str(a, 'userId')) ?? null;
      case 'isE2eePeerDecryptable':
        return c.isE2eePeerDecryptable(str(a, 'userId'));
      case 'e2eeDecryptablePeers':
        return c.e2eeDecryptablePeers();
      case 'isChannelEncrypted':
        return c.isChannelEncrypted(str(a, 'channelId'));
      case 'e2eeGeneration':
        return c.e2eeGeneration ?? null;
      case 'e2eeStats':
        return c.e2eeStats ?? null;
      case 'refreshE2eeStats':
        return c.refreshE2eeStats().then((s) => s ?? null);
      case 'rotateE2eeKey':
        return c.rotateE2eeKey().then((g) => g ?? null);

      default:
        throw new Error(`unknown method ${method}`);
    }
  }

  // ── Events ──

  private subscribe(entry: Entry, rawMessages: boolean, localEnergy: boolean, visemes: boolean): void {
    const c = entry.client;
    const on = <K extends keyof AurixEvents>(event: K, listener: AurixEvents[K]) => {
      entry.unsubscribe.push(c.on(event, listener));
    };
    const q = (event: BridgeEvent) => this.push(entry, event);

    on('connectionState', (state: ConnectionState) => q({ type: 'connectionState', state }));
    on('sessionReady', (info) => q({ type: 'sessionReady', info }));
    on('remoteStream', (stream) => this.playRemote(entry, stream));
    on('mediaTransport', (transport) => {
      q({ type: 'mediaTransport', transport });
      if (transport !== 'webrtc') void this.reportGraphPlayback(entry);
    });
    on('channelJoined', (channelId, participants) => q({ type: 'channelJoined', channelId, participants }));
    on('channelLeft', (channelId) => q({ type: 'channelLeft', channelId }));
    on('participantJoined', (channelId, participant) => q({ type: 'participantJoined', channelId, participant }));
    on('participantLeft', (channelId, userId) => q({ type: 'participantLeft', channelId, userId }));
    on('participantUpdated', (channelId, participant) => q({ type: 'participantUpdated', channelId, participant }));
    on('speaking', (channelId, userId, speaking) => q({ type: 'speaking', channelId, userId, speaking }));
    on('energy', (channelId, levels) => q({ type: 'energy', channelId, levels }));
    if (localEnergy) on('localEnergy', (sample) => q({ type: 'localEnergy', sample }));
    on('localSpeaking', (speaking) => q({ type: 'localSpeaking', speaking }));
    on('devicesChanged', (devices) => q({ type: 'devicesChanged', devices }));
    on('inputDeviceChanged', (deviceId) => q({ type: 'inputDeviceChanged', deviceId: deviceId ?? null }));
    on('audioInjection', (active) => q({ type: 'audioInjection', active }));
    on('positions', (channelId, positions) => q({ type: 'positions', channelId, positions }));
    on('recording', (channelId, recordingId, active, initiatedBy, live) =>
      q({ type: 'recording', channelId, recordingId, active, initiatedBy, live }),
    );
    on('bitrate', (targetKbps, reason, expectedLossPercent) =>
      q({ type: 'bitrate', targetKbps, reason, expectedLossPercent }),
    );
    on('audioPolicy', (policy) => q({ type: 'audioPolicy', policy }));
    on('networkQuality', (quality) => q({ type: 'networkQuality', quality }));
    on('stats', (stats) => q({ type: 'stats', stats }));
    on('kicked', (channelId, reason) => q({ type: 'kicked', channelId, reason }));
    on('receiverPreferences', (prefs) => q({ type: 'receiverPreferences', prefs }));
    on('userBlockChanged', (userId, blocked) => q({ type: 'userBlockChanged', userId, blocked }));
    on('transmissionChanged', (mode) => q({ type: 'transmissionChanged', mode }));
    on('channelFocusChanged', (channelId) => q({ type: 'channelFocusChanged', channelId: channelId ?? null }));
    on('participantStreams', (streams) => q({ type: 'participantStreams', streams: participantStreamsEvent(streams) }));
    on('participantPriorityChanged', (channelId, userId, priority) =>
      q({ type: 'participantPriorityChanged', channelId, userId, priority }),
    );
    on('participantRoleChanged', (channelId, userId, role, admitted) =>
      q({ type: 'participantRoleChanged', channelId, userId, role, admitted }),
    );
    on('duckingChanged', (channelId, active, config) => q({ type: 'duckingChanged', channelId, active, config }));
    if (visemes) {
      on('participantVisemes', (userId, frame) => q({ type: 'participantVisemes', userId, frame }));
      on('localVisemes', (frame) => q({ type: 'localVisemes', frame }));
    }
    on('e2eePeerKey', (userId, fingerprint, previousFingerprint) =>
      q({ type: 'e2eePeerKey', userId, fingerprint, previousFingerprint: previousFingerprint ?? null }),
    );
    on('e2eePeerDecryptable', (userId, decryptable) => q({ type: 'e2eePeerDecryptable', userId, decryptable }));
    on('e2eeKeyRotated', (generation) => q({ type: 'e2eeKeyRotated', generation }));
    on('recovering', (attempt, delayMs, cause) => q({ type: 'recovering', attempt, delayMs, cause }));
    on('recovered', (info) => q({ type: 'recovered', info }));
    on('endpointChanged', (url) => q({ type: 'endpointChanged', url }));
    on('failedToRecover', (error) => q({ type: 'failedToRecover', error: errorInfo(error) }));
    on('sessionClosed', (reason) => q({ type: 'sessionClosed', reason }));
    on('chatMessage', (message) => q({ type: 'chatMessage', message }));
    on('chatReadMarker', (marker) => q({ type: 'chatReadMarker', marker }));
    on('chatMessageUpdated', (message) => q({ type: 'chatMessageUpdated', message }));
    on('chatReactionChanged', (change) => q({ type: 'chatReactionChanged', change }));
    on('chatInboxSynced', (delivered, truncated, perDevice) =>
      q({ type: 'chatInboxSynced', delivered, truncated, perDevice }),
    );
    on('participantTyping', (channelId, userId, typing) => q({ type: 'participantTyping', channelId, userId, typing }));
    on('transcript', (transcript) => q({ type: 'transcript', transcript }));
    on('translationChanged', (prefs) => q({ type: 'translationChanged', prefs }));
    on('serverNoiseSuppressionChanged', (enabled) => q({ type: 'serverNoiseSuppressionChanged', enabled }));
    on('ttsStatus', (status) => q({ type: 'ttsStatus', status }));
    on('serverError', (code, message) => q({ type: 'serverError', code, message }));
    on('error', (error) => q({ type: 'error', error: errorInfo(error) }));
    if (rawMessages) on('message', (message) => q({ type: 'message', message }));
  }

  private push(entry: Entry, event: BridgeEvent): void {
    if (entry.queue.length >= this.maxQueued) {
      entry.queue.shift();
      entry.dropped++;
    }
    entry.queue.push(event);
  }

  // ── Remote audio ──

  private playRemote(entry: Entry, stream: MediaStream): void {
    if (!this.document) {
      this.push(entry, { type: 'remoteAudio', playing: false, reason: 'no document' });
      return;
    }
    if (!entry.audio) {
      const el = this.document.createElement('audio');
      el.autoplay = true;
      el.setAttribute('playsinline', '');
      el.style.display = 'none';
      this.document.body?.appendChild(el);
      entry.audio = el;
      entry.client.attachAudioOutput(el);
    }
    const el = entry.audio;
    el.srcObject = stream;
    const play = el.play();
    if (isPromise(play)) {
      play.then(
        () => this.push(entry, { type: 'remoteAudio', playing: true }),
        (e: unknown) => this.push(entry, { type: 'remoteAudio', playing: false, reason: errorInfo(e).message }),
      );
    } else {
      this.push(entry, { type: 'remoteAudio', playing: true });
    }
  }

  /**
   * WebTransport has no `MediaStream`: playback is the Web Audio graph (WebCodecs → worklet →
   * spatial renderer), so its autoplay state is reported the same way the `<audio>` element's is.
   */
  private async reportGraphPlayback(entry: Entry): Promise<void> {
    const playing = await entry.client.resumeAudio();
    this.push(
      entry,
      playing
        ? { type: 'remoteAudio', playing: true }
        : { type: 'remoteAudio', playing: false, reason: 'audio context suspended (autoplay policy)' },
    );
  }

  /**
   * Retry playback after a user gesture (autoplay policy): the mixed track's element and the
   * Web Audio graph of per-participant tracks. Resolves `true` when audio is playing.
   */
  private async resumeAudio(entry: Entry): Promise<boolean> {
    const graph = await entry.client.resumeAudio();
    const el = entry.audio;
    if (!el || !el.srcObject) return graph;
    try {
      await el.play();
      this.push(entry, { type: 'remoteAudio', playing: true });
      return graph;
    } catch (e) {
      this.push(entry, { type: 'remoteAudio', playing: false, reason: errorInfo(e).message });
      return false;
    }
  }

  // ── Token requests ──

  private requestToken(entry: Entry, kind: 'refresh' | 'join', channelId: string | undefined): Promise<string> {
    const requestId = entry.nextTokenRequest++;
    return new Promise<string>((resolve, reject) => {
      entry.tokenRequests.set(requestId, { resolve, reject });
      this.push(entry, { type: 'tokenRequest', requestId, kind, channelId: channelId ?? null });
    });
  }

  private provideToken(entry: Entry, a: Args): void {
    const requestId = num(a, 'requestId');
    const req = entry.tokenRequests.get(requestId);
    if (!req) throw new Error(`unknown token request ${requestId}`);
    entry.tokenRequests.delete(requestId);
    const token = optString(a, 'token');
    if (token !== undefined) req.resolve(token);
    else req.reject(new Error(optString(a, 'error') ?? 'token request declined'));
  }
}

// ── Argument helpers ──

function parseArgs(json: string): Args {
  if (json === undefined || json === null || json === '') return {};
  const value: unknown = JSON.parse(json);
  if (value === null || typeof value !== 'object' || Array.isArray(value)) throw new Error('arguments must be a JSON object');
  return value as Args;
}

function str(a: Args, key: string): string {
  const v = a[key];
  if (typeof v !== 'string') throw new Error(`${key} must be a string`);
  return v;
}

function optString(a: Args, key: string): string | undefined {
  const v = a[key];
  if (v === undefined || v === null) return undefined;
  if (typeof v !== 'string') throw new Error(`${key} must be a string`);
  return v;
}

function strArray(a: Args, key: string): string[] {
  const v = a[key];
  if (!Array.isArray(v) || v.some((x) => typeof x !== 'string')) throw new Error(`${key} must be an array of strings`);
  return v as string[];
}

/** Wire form of the track layout: a `MediaStream` cannot cross a string bridge, its presence can. */
function participantStreamsEvent(streams: ParticipantStreamInfo[]): Array<{ mid: string; userId: string | null; live: boolean }> {
  return streams.map((s) => ({ mid: s.mid, userId: s.userId ?? null, live: s.live }));
}

function e2eeOptions(raw: NonNullable<BridgeClientOptions['e2ee']>): boolean | E2eeOptions {
  if (typeof raw === 'boolean') return raw;
  const options: E2eeOptions = {};
  if (raw.identity !== undefined) {
    const secret = base64ToBytes(raw.identity);
    if (secret.length !== 32) throw new Error('e2ee.identity must be a base64 32-byte secret');
    options.identity = secret;
  }
  if (raw.transform !== undefined) options.transform = raw.transform;
  if (raw.workerUrl !== undefined) options.workerUrl = raw.workerUrl;
  return options;
}

/** `undefined` / `null` = bypass; a preset name; or a parameter object (sanitised by the client). */
function voiceEffects(v: unknown): VoiceEffectParams | VoiceEffectPreset | undefined {
  if (v === undefined || v === null) return undefined;
  if (typeof v === 'string') return v as VoiceEffectPreset;
  if (typeof v !== 'object') throw new Error('effects must be a preset name or an object');
  return v as VoiceEffectParams;
}

function num(a: Args, key: string): number {
  const v = a[key];
  if (typeof v !== 'number' || !Number.isFinite(v)) throw new Error(`${key} must be a number`);
  return v;
}

function optNumber(a: Args, key: string): number | undefined {
  const v = a[key];
  if (v === undefined || v === null) return undefined;
  if (typeof v !== 'number' || !Number.isFinite(v)) throw new Error(`${key} must be a number`);
  return v;
}

function bool(a: Args, key: string): boolean {
  const v = a[key];
  if (typeof v !== 'boolean') throw new Error(`${key} must be a boolean`);
  return v;
}

/** Accepts the SDK form `{type, channelId}` and the wire form `{mode, channel_id}`. */
function transmission(v: unknown): TransmissionMode {
  if (v === null || typeof v !== 'object') throw new Error('mode must be an object');
  const o = v as Record<string, unknown>;
  const kind = typeof o['type'] === 'string' ? o['type'] : o['mode'];
  switch (kind) {
    case 'none':
      return { type: 'none' };
    case 'all':
      return { type: 'all' };
    case 'single': {
      const channelId = o['channelId'] ?? o['channel_id'];
      if (typeof channelId !== 'string') throw new Error('single transmission needs a channelId');
      return { type: 'single', channelId };
    }
    default:
      throw new Error(`unknown transmission mode ${String(kind)}`);
  }
}

function chatScope(a: Args): ChatScope {
  const channelId = optString(a, 'channelId');
  if (channelId !== undefined) return { channelId };
  const userId = optString(a, 'userId');
  if (userId !== undefined) return { userId };
  throw new Error('channelId or userId is required');
}

function messageOptions(a: Args): SendMessageOptions {
  const options: SendMessageOptions = {};
  if (a['metadata'] !== undefined) options.metadata = a['metadata'] as JsonValue;
  const clientRef = optString(a, 'clientRef');
  if (clientRef !== undefined) options.clientRef = clientRef;
  return options;
}

function historyOptions(a: Args): HistoryOptions {
  const options: HistoryOptions = {};
  const before = optString(a, 'before');
  const after = optString(a, 'after');
  const limit = optNumber(a, 'limit');
  if (before !== undefined) options.before = before;
  if (after !== undefined) options.after = after;
  if (limit !== undefined) options.limit = limit;
  return options;
}

function isPromise(v: unknown): v is Promise<unknown> {
  return v !== null && typeof v === 'object' && typeof (v as { then?: unknown }).then === 'function';
}

/** `undefined` → `null` so the host sees an explicit value; everything else is left to `JSON.stringify`. */
function jsonSafe(v: unknown): unknown {
  return v === undefined ? null : v;
}

export interface BridgeErrorInfo {
  message: string;
  name?: string;
  code?: string;
}

export function errorInfo(e: unknown): BridgeErrorInfo {
  if (e instanceof Error) {
    const info: BridgeErrorInfo = { message: e.message, name: e.name };
    const code = (e as { code?: unknown }).code;
    if (typeof code === 'string') info.code = code;
    return info;
  }
  return { message: String(e) };
}
