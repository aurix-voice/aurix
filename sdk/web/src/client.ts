import {
  AURIX_SUBPROTOCOL,
  BEARER_SUBPROTOCOL_PREFIX,
  MAX_PARTICIPANT_VOLUME,
  RESUME_SUBPROTOCOL_PREFIX,
  parseServerMessage,
  SYSTEM_USER_ID,
  type ChannelRole,
  type ChatMessageWire,
  type ChatReadMarkerWire,
  type ClientMessage,
  type DuckingConfigWire,
  type JsonValue,
  type LocalMute,
  type ModerationAction,
  type ParticipantBrief,
  type ParticipantEnergy,
  type ParticipantStreamWire,
  type ParticipantVolume,
  type PositionalConfigWire,
  type RecordingConsent,
  type ServerMessage,
  type TransmissionModeWire,
  type TranscriptWire,
  type TtsDestinationWire,
  type TtsStateWire,
  type TurnCredentials,
  type UnknownMessage,
  type UserPosition,
  type WebTransportInfoWire,
} from './protocol.js';
import {
  E2eeGroup,
  E2eeIdentity,
  attachEncodedStreams,
  base64ToBytes,
  bytesToBase64,
  detectE2eeSupport,
  e2eeWorkerSource,
  type E2eeFrameStats,
  type E2eeKeySink,
  type E2eeMode,
  type E2eeOutgoing,
  type E2eeSupport,
  type E2eeTransformApi,
  type E2eeWorkerMessage,
  type E2eeWorkerReply,
} from './e2ee.js';
import { AudioLevelMeter, type AudioLevelMeterOptions, type AudioLevelSample } from './audio.js';
import {
  SpatialRenderer,
  createAudioContext,
  renderParams,
  type RenderInputs,
  type DuckParams,
  type SpatialAudioContextLike,
} from './spatial.js';
import {
  InputPipeline,
  MAX_INPUT_GAIN,
  enumerateAudioDevices,
  supportsOutputSelection,
  type AudioDevices,
  type AudioInjectionOptions,
  type AudioInjectionSource,
} from './devices.js';
import {
  VOICE_EFFECTS_BYPASS,
  createVoiceEffectsNode,
  isVoiceEffectsBypass,
  loadVoiceEffectsWorklet,
  sanitizeVoiceEffects,
  supportsVoiceEffects,
  voiceEffectPreset,
  type ResolvedVoiceEffectParams,
  type VoiceEffectParams,
  type VoiceEffectPreset,
  type VoiceEffectsWorkletMessage,
} from './effects.js';
import { createVisemeNode, loadVisemeWorklet, supportsVisemes, type VisemeFrame } from './visemes.js';
import {
  LossWindow,
  RttTracker,
  assembleClientStats,
  networkQualityFromWire,
  type ClientStats,
  type NetworkQuality,
  type RtcStatsInput,
} from './quality.js';
import {
  DEFAULT_AUDIO_POLICY,
  applyOpusSenderPreferences,
  audioPoliciesEqual,
  mergeAllAudioPolicies,
  negotiatedOpusPreferences,
  parseAudioPolicy,
  resolveOpusSenderPreferences,
  senderPreferencesEqual,
  type AudioPolicy,
  type OpusBrowserOptions,
  type OpusSenderPreferences,
} from './opus.js';
import { channelIdHash, opusPacketIsStereo, type AurxDirection, type DownlinkAudio } from './aurx.js';
import {
  AURX_FRAME_SAMPLES,
  AurxCapture,
  AurxPlayback,
  opusConfigFor,
  supportsAurxAudio,
  type AurxCaptureFrame,
  type AurxOpusConfig,
} from './aurx-audio.js';
import { AurxWebTransport, detectWebTransportSupport, type AurxWebTransportOptions } from './webtransport.js';

/** `SessionInitAck.media_key` length: the AURX master key. */
const AURX_MEDIA_KEY_BYTES = 32;

/** One downlink SSRC rendered through the spatial graph while on WebTransport. */
interface WebTransportSlot {
  userId: string | undefined;
  /** Carries E2EE frames: rendered from local preferences, not server metadata. */
  e2ee: boolean;
  /** Latched once a stereo frame is seen (decoders only upgrade). */
  stereo: boolean;
  lastPacketAt: number;
  /** Last server-applied gain/direction (skips redundant `render` calls). */
  gain: number;
  direction?: AurxDirection | undefined;
}

function wtSlotKey(ssrc: number): string {
  return `wt:${ssrc}`;
}

function directionsEqual(a: AurxDirection | undefined, b: AurxDirection | undefined): boolean {
  if (a === undefined || b === undefined) return a === b;
  return a.azimuth === b.azimuth && a.elevation === b.elevation;
}

type InboundRtpStats = NonNullable<RtcStatsInput['inbound']>;

/**
 * One `inbound-rtp` entry per downlink track (mixed + per-participant): counters add up, jitter
 * is the worst track that actually carried packets.
 */
function mergeInboundStats(acc: InboundRtpStats | undefined, s: InboundRtpStats): InboundRtpStats {
  if (!acc) return { ...s };
  const accJitter = (acc.packetsReceived ?? 0) > 0 ? (acc.jitter ?? 0) : 0;
  const jitter = (s.packetsReceived ?? 0) > 0 ? Math.max(accJitter, s.jitter ?? 0) : accJitter;
  return {
    jitter,
    packetsReceived: (acc.packetsReceived ?? 0) + (s.packetsReceived ?? 0),
    packetsLost: (acc.packetsLost ?? 0) + (s.packetsLost ?? 0),
    bytesReceived: (acc.bytesReceived ?? 0) + (s.bytesReceived ?? 0),
    concealedSamples: (acc.concealedSamples ?? 0) + (s.concealedSamples ?? 0),
    packetsDiscarded: (acc.packetsDiscarded ?? 0) + (s.packetsDiscarded ?? 0),
    jitterBufferDelay: (acc.jitterBufferDelay ?? 0) + (s.jitterBufferDelay ?? 0),
    jitterBufferEmittedCount: (acc.jitterBufferEmittedCount ?? 0) + (s.jitterBufferEmittedCount ?? 0),
  };
}

function webTransportAdvertised(info: WebTransportInfoWire | undefined): info is WebTransportInfoWire {
  return info !== undefined && Array.isArray(info.urls) && info.urls.some((u) => typeof u === 'string' && u.length > 0);
}

/** Why AURX over WebTransport cannot run in this browser (`undefined` = it can). */
function webTransportCapabilityBlocker(): string | undefined {
  const support = detectWebTransportSupport();
  if (!support.webTransport) return 'no WebTransport in this browser';
  if (!support.datagrams) return 'WebTransport datagrams are unsupported here';
  if (!support.crypto) return 'WebCrypto is unavailable (insecure context?)';
  if (!supportsAurxAudio()) return 'WebCodecs Opus / AudioWorklet are unavailable here';
  return undefined;
}

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
  /**
   * Constraints for `getUserMedia`; defaults to echo cancellation + noise suppression + AGC, or —
   * with `opus.stereo` — to a 2-channel track with that voice processing off.
   */
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
   * Opus preferences for the uplink. Browsers expose only bitrate (live), FEC, DTX, bandwidth
   * and CBR (at negotiation); complexity/signal/VBR are the browser's own. By default the
   * server's channel audio policy drives all of them — see `opus.ts` for the mapping.
   */
  opus?: OpusBrowserOptions;
  /**
   * Meter the local microphone with Web Audio and emit `localEnergy` / `localSpeaking`
   * (default `true`). Pass an object to tune the VAD, `false` to disable.
   */
  localVoiceActivity?: boolean | AudioLevelMeterOptions;
  /** Application-level keepalive interval in ms (`Ping`/`Pong`). Default 15000, 0 disables. */
  pingIntervalMs?: number;
  /**
   * How often to sample WebRTC statistics and send a `QualityReport` to the server (which
   * feeds adaptive bitrate and the server-side `networkQuality`). Default 5000, 0 disables.
   */
  qualityReportIntervalMs?: number;
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
  /**
   * How many per-participant downlink tracks to negotiate next to the mixed track (each
   * carries one participant's voice untouched; the client renders volume, mute, focus and
   * positional HRTF itself). Capped by the node's `webrtc_participant_streams`; defaults to
   * that cap. `0` = mixed track only (server-side spatialisation as stereo pan).
   */
  participantStreams?: number;
  /**
   * Rendering of per-participant tracks: `true` (default) builds a Web Audio graph
   * (`PannerNode`, HRTF); `'equalpower'` uses the cheaper panner; `false` leaves playback to
   * the app (`participantStreams` event / `getParticipantStream`), which then applies
   * volume, mute and position itself.
   */
  spatialAudio?: boolean | 'equalpower';
  /** Use this `AudioContext` for participant tracks instead of creating one. */
  audioContext?: AudioContext;
  /**
   * Start with local lip-sync analysis on (see {@link AurixClient.setVisemes}): a
   * `VisemeFrame` per 20 ms for every participant with a dedicated track and for our own
   * microphone. Analysed here, after decryption; nothing is sent.
   */
  visemes?: boolean;
  /** Voice effects on the microphone from the start (a preset name or parameters). */
  voiceEffects?: VoiceEffectParams | VoiceEffectPreset;
  /**
   * Group end-to-end encryption (`e2ee.ts`). `true` (default): announce the capability when
   * the browser has WebCrypto and an encoded-frame API, so channels created with
   * `e2ee: true` can be joined; without those the join fails with `E2EE_REQUIRED` (there is
   * no plaintext fallback). `false`: never join encrypted channels.
   */
  e2ee?: boolean | E2eeOptions;
  /**
   * Media path. `'auto'` (default) carries AURX over WebTransport (HTTP/3 datagrams sealed
   * with the session key, Opus via WebCodecs) when the node advertises it and the browser has
   * WebTransport + WebCodecs, and negotiates WebRTC otherwise or when the WebTransport
   * connection fails. `'webrtc'` never tries WebTransport; `'webtransport'` requires it
   * (`connect()` fails instead of falling back).
   */
  transport?: MediaTransportPreference;
  /** Tuning of the WebTransport media path (ignored on WebRTC). */
  webTransport?: WebTransportClientOptions;
}

/** The media path a session ended up on. */
export type MediaTransport = 'webrtc' | 'webtransport';
export type MediaTransportPreference = 'auto' | MediaTransport;

export interface WebTransportClientOptions {
  /** Per-URL connect + `SessionBind` budget (ms). Default 4000. */
  connectTimeoutMs?: number;
  /** Heartbeat period (ms), also the RTT probe; 0 disables heartbeats. Default 2000. */
  heartbeatIntervalMs?: number;
  /** Consecutive unanswered heartbeats before the path counts as dead. Default 5. */
  heartbeatLossLimit?: number;
  /**
   * Opus encoder settings layered over the channel audio policy — the full set browsers
   * lack on WebRTC: `complexity`, `signal`, `application`, `expectedLossPct`, `cbr`.
   */
  opus?: Partial<AurxOpusConfig>;
  /** Forget a downlink stream (and free its decoder) after this long without a packet (ms). Default 5000. */
  idleTimeoutMs?: number;
}

/** WebTransport media endpoint the node advertised in `SessionInitAck`. */
export interface WebTransportAdvertisement {
  /** `https://host:port/aurix` URLs, tried in order. */
  urls: string[];
  /** SHA-256 pins (hex) of the node's generated certificate (current, next); empty = WebPKI. */
  certSha256: string[];
}

export interface E2eeOptions {
  /**
   * 32-byte X25519 identity secret; a stored one keeps this client's fingerprint stable
   * across sessions (`e2eeIdentitySecret` exports it). Random per client when absent.
   */
  identity?: Uint8Array;
  /** Encoded-frame API: `'auto'` prefers `RTCRtpScriptTransform`, falls back to `createEncodedStreams()`. */
  transform?: 'auto' | E2eeTransformApi;
  /** Serve the transform worker from this URL instead of a `blob:` URL (CSP without `worker-src blob:`). */
  workerUrl?: string;
}

/**
 * One per-participant downlink slot and who is on it: a negotiated WebRTC track, or — on the
 * WebTransport path — a remote SSRC currently rendered (`mid` = `wt:<ssrc>`, no `MediaStream`).
 */
export interface ParticipantStreamInfo {
  /** SDP media id of the track (stable for the life of the peer connection) or `wt:<ssrc>`. */
  mid: string;
  /** Participant currently carried; `undefined` = idle (heard in the mix, if at all). */
  userId: string | undefined;
  /** The browser-side track, once it arrived (WebRTC only; WebCodecs playback has none). */
  stream: MediaStream | undefined;
  /** Audio is flowing on this slot: the track arrived, or datagrams for the SSRC keep coming. */
  live: boolean;
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
  /**
   * `true` when the resume landed on a different node than the one that created the session
   * (the previous node stopped answering): same session id and SSRC, media renegotiated.
   */
  migrated: boolean;
  /** WebSocket URL of the node serving this session. */
  endpoint: string;
  /** Other nodes advertised for failover, tried in order after `endpoint` on a reconnect. */
  failover: string[];
  /**
   * The node translates transcripts on request (`setTranslation`); `undefined` when the
   * operator has not configured translation.
   */
  translation?: TranslationInfo;
  /** Per-participant downlink tracks the node serves this browser at most (`0` = mixed only). */
  participantStreamCap: number;
  /** AURX-over-WebTransport endpoint of the node, when it offers one for this session. */
  webTransport?: WebTransportAdvertisement;
}

/** Live-translation capability of the node. */
export interface TranslationInfo {
  /** Translations can also be spoken privately to the listener (`setTranslation(..., { speech: true })`). */
  speech: boolean;
  /** Target languages listeners may request; empty = any BCP-47 tag. */
  languages: string[];
}

/** This client's translation preference as the server applied it (normalised tags). */
export interface TranslationPrefs {
  /** Target language, or `undefined` when receiving originals only. */
  language?: string;
  /** Language this participant declared it speaks. */
  spokenLanguage?: string;
  /** Translations are also spoken privately to this client. */
  speech: boolean;
}

export interface SetTranslationOptions {
  /** Tell peers' translators which language you speak when the recogniser cannot tell. */
  spokenLanguage?: string;
  /** Also have the translation spoken to you alone, on the channel's translator voice (needs `SessionInfo.translation.speech`). */
  speech?: boolean;
}

/**
 * Reconnect target for `attempt` (1-based): the node that holds the session first — a
 * same-node resume is the cheapest recovery — then each advertised alternate, round-robin.
 */
export function reconnectEndpoint(active: string, failover: readonly string[], attempt: number): string {
  const n = 1 + failover.length;
  const i = (Math.max(1, attempt) - 1) % n;
  return i === 0 ? active : (failover[i - 1] ?? active);
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
  /** Priority speaker: their speech ducks the other voices (see {@link ChannelInfo.ducking}). */
  priority: boolean;
}

/**
 * How far presence and text reach in a positional channel (`PositionalConfig.roster_radius` /
 * `text_radius`, from `ChannelJoinAck`). `undefined` = the whole channel.
 */
export interface ChannelScope {
  /**
   * With a roster radius the roster only lists members within this distance of you (once both
   * positions are known); `participantJoined` / `participantLeft` also fire when someone moves
   * in or out of range (leaving uses a 10 % wider radius so the edge does not flicker).
   */
  rosterRadius?: number;
  /** Channel chat, typing and transcripts reach only members within this distance. */
  textRadius?: number;
}

/**
 * What the server told us about a joined channel in `ChannelJoinAck` (audience / large-channel
 * metadata). Older servers omit the fields; the defaults below then apply.
 */
export interface ChannelInfo {
  /**
   * Our *effective* role; `listener` means receive-only — `unmute()` / speaking has no effect
   * there. Under `audience.speaker_admission` it may differ from the grant: see
   * {@link ChannelInfo.waitingToSpeak}.
   */
  role: ChannelRole;
  /**
   * We hold a speaking grant but every `audience.max_speakers` slot is taken (or an idle
   * speaker slot was taken from us); `role` is `listener` until the server admits us again,
   * which {@link AurixEvents.participantRoleChanged} reports.
   */
  waitingToSpeak: boolean;
  /**
   * Members across all nodes, including listeners hidden from the roster by
   * `ChannelConfig.audience.hide_listeners` (so it may exceed the roster size).
   */
  participantCount: number;
  /** Listeners are hidden from presence (roster and `participantJoined` / `participantLeft`). */
  hiddenListeners: boolean;
  /** The channel transcribes speech (same as {@link AurixClient.isChannelTranscribed}). */
  transcription: boolean;
  /** Speech is monitored by content safety (same as {@link AurixClient.isChannelMonitored}). */
  safetyVoice: boolean;
  /**
   * Priority-speaker ducking of the channel (`ChannelConfig.ducking`); `undefined` = off. The
   * server applies it to what it mixes, the SDK reproduces it on per-participant tracks and
   * exposes it to game audio through {@link AurixEvents.duckingChanged}.
   */
  ducking?: DuckingConfig;
  /** You are a priority speaker here (from the grant, or promoted at runtime). */
  priority: boolean;
}

/** Priority-speaker ducking parameters; see {@link ChannelInfo.ducking}. */
export interface DuckingConfig {
  /** Gain of non-priority voices while a priority speaker talks (`0` = silenced). */
  gain: number;
  attackMs: number;
  releaseMs: number;
  /** Ducking persists this long past the priority speaker's last audible frame. */
  holdMs: number;
  /** Moderators and administrators duck the channel as well. */
  moderators: boolean;
}

/** `DuckingConfig` defaults of the server, reported when a channel's config is already gone. */
export const DEFAULT_DUCKING: DuckingConfig = Object.freeze({
  gain: 0.25,
  attackMs: 60,
  releaseMs: 400,
  holdMs: 250,
  moderators: false,
});

function resolveVoiceEffects(effects: VoiceEffectParams | VoiceEffectPreset | undefined): ResolvedVoiceEffectParams {
  if (effects === undefined) return VOICE_EFFECTS_BYPASS;
  if (typeof effects === 'string') return voiceEffectPreset(effects);
  return sanitizeVoiceEffects(effects);
}

/** The renderer's context when it is a real `BaseAudioContext` with worklet support. */
function workletContext(ctx: SpatialAudioContextLike): BaseAudioContext | undefined {
  const candidate = ctx as unknown as { audioWorklet?: unknown };
  return typeof candidate.audioWorklet === 'object' && candidate.audioWorklet !== null
    ? (ctx as unknown as BaseAudioContext)
    : undefined;
}

export function parseDucking(d: DuckingConfigWire | null | undefined): DuckingConfig | undefined {
  if (!d) return undefined;
  return {
    gain: Math.min(1, Math.max(0, typeof d.gain === 'number' ? d.gain : 0.25)),
    attackMs: typeof d.attack_ms === 'number' ? d.attack_ms : 60,
    releaseMs: typeof d.release_ms === 'number' ? d.release_ms : 400,
    holdMs: typeof d.hold_ms === 'number' ? d.hold_ms : 250,
    moderators: d.moderators === true,
  };
}

/** `ChannelJoinAck` → {@link ChannelInfo}, filling the defaults older servers imply. */
export function channelInfoFromJoinAck(
  d: Extract<ServerMessage, { type: 'ChannelJoinAck' }>['data'],
): ChannelInfo {
  const info: ChannelInfo = {
    role: d.role ?? 'speaker',
    waitingToSpeak: d.waiting_to_speak === true,
    participantCount:
      typeof d.participant_count === 'number' ? d.participant_count : d.participants.length,
    hiddenListeners: d.hidden_listeners === true,
    transcription: d.transcription === true,
    safetyVoice: d.safety_voice === true,
    priority: d.priority === true,
  };
  const ducking = parseDucking(d.ducking);
  if (ducking) info.ducking = ducking;
  return info;
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
  /**
   * Directed message that waited for the recipient: replayed on connect (see
   * {@link AurixEvents.chatInboxSynced}), or — on the sender's `own` echo — accepted and
   * stored because the recipient is offline.
   */
  offline: boolean;
  /** Opaque history cursor of this message (`before` / `after` of {@link AurixClient.history}). */
  cursor: string;
  /** When the text was last edited; `undefined` for a never-edited message. */
  editedAt?: Date;
  /**
   * Tombstone of a deleted message: same `id`, `sentAt` and `cursor` as the original, empty
   * `text`, no `metadata`, no `reactions`. Replace the displayed copy with a placeholder.
   */
  deletedAt?: Date;
  /** Who deleted it: the author, a channel moderator or (`SYSTEM_USER_ID`) the operator. */
  deletedBy?: string;
  /** Reaction tallies of a stored message (history / search); live messages start with none. */
  reactions: ChatReaction[];
}

/** One reaction on a message and who carries it. */
export interface ChatReaction {
  /** Emoji or a short game-defined token (≤ 32 bytes). */
  reaction: string;
  count: number;
  /** At most the first 20 users; compare with `count` to know whether the list is complete. */
  userIds: string[];
}

/** A user added or removed a reaction; see {@link AurixEvents.chatReactionChanged}. */
export interface ChatReactionChange {
  messageId: string;
  /** Set for channel messages. */
  channelId?: string;
  /** Author and (direct messages) recipient of the message the reaction is on. */
  messageFromUserId: string;
  messageToUserId?: string;
  /** Who reacted (`own` when this user, possibly from another device). */
  userId: string;
  reaction: string;
  added: boolean;
  /** Users carrying `reaction` on the message after this change. */
  count: number;
  timestamp: Date;
}

/** A stored text conversation: a joined channel, or the direct exchange with one user. */
export type ChatScope = { channelId: string; userId?: undefined } | { userId: string; channelId?: undefined };

export interface HistoryOptions {
  /** Cursor: only messages older than it (`nextBefore` of a page, or `ChatMessage.cursor`). */
  before?: string;
  /** Cursor: only messages newer than it (`nextAfter` of a page, or `ChatMessage.cursor`). */
  after?: string;
  /** Page size; the server clamps it (`chat.history_page_max`, 200 by default). */
  limit?: number;
}

/** One page of stored chat history, newest first. */
export interface HistoryPage {
  messages: ChatMessage[];
  /** Cursor for the next older page; `undefined` when the beginning was reached. */
  nextBefore?: string;
  /** Cursor for the next newer page; `undefined` when the page is the most recent. */
  nextAfter?: string;
}

export interface SearchOptions {
  /** Only messages of this author. */
  fromUserId?: string;
  /** Cursor (`nextBefore` of a previous result page): only older matches. */
  before?: string;
  /** Page size; the server clamps it (`chat.history_page_max`). */
  limit?: number;
}

/** One page of search matches, newest first; page on with `{ before: nextBefore }`. */
export interface SearchPage {
  query: string;
  messages: ChatMessage[];
  nextBefore?: string;
}

export interface EditMessageOptions {
  /** New game payload. An edit replaces the whole message: omitting it clears the current one. */
  metadata?: JsonValue;
}

/** A user's reading position in a channel or a direct conversation. */
export interface ReadMarker {
  userId: string;
  /** Set for channel markers. */
  channelId?: string;
  /** The other party of a direct conversation. */
  peerUserId?: string;
  /** Last read message and its send time. */
  messageId: string;
  messageSentAt: Date;
  readAt: Date;
}

export interface ReadMarkers {
  /** This user's own marker (when it was ever set) and, with server-side read receipts, the other participants'. */
  markers: ReadMarker[];
  /** Messages after this user's marker (capped by the server, `chat.unread_count_cap`). */
  unreadCount: number;
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

/** A server-side speech-to-text result for a channel with transcription enabled. */
export interface Transcript {
  id: string;
  channelId: string;
  userId: string;
  text: string;
  /** BCP-47 / ISO language the provider detected, when known. */
  language?: string;
  /** When the transcribed audio started. */
  startedAt: Date;
  /** Length of the transcribed audio (ms). */
  durationMs: number;
  /** Word timings relative to `startedAt`; empty unless the server enables them. */
  words: TranscriptWord[];
  /** Present when `text` is a translation: the speaker's words as transcribed. */
  original?: { text: string; language?: string };
}

export interface TranscriptWord {
  word: string;
  startMs: number;
  endMs: number;
}

/**
 * Who hears a synthesized utterance: `channel` = the other members (the classic
 * "text-to-speech into voice chat"), `local` = only this client, `both` = everyone including
 * this client.
 */
export type TtsDestination = TtsDestinationWire;

export type TtsState = TtsStateWire;

export interface SpeakOptions {
  /**
   * Channel to speak into. Optional when this session transmits to exactly one channel
   * (single joined channel or `single` transmission mode).
   */
  channelId?: string;
  /** Provider voice; must be on the server's allowlist (`GET /v1/tts/voices`). */
  voice?: string;
  /** Defaults to `channel`. */
  destination?: TtsDestination;
  /** Correlation id echoed on every `ttsStatus` for this request. Generated when omitted. */
  clientRef?: string;
}

/** A queued TTS request; see {@link AurixClient.speak}. */
export interface SpeechRequest {
  requestId: string;
  clientRef: string;
  /** Resolves with the terminal status (`finished`, `cancelled` or `failed`). */
  done: Promise<TtsStatus>;
}

/** Lifecycle update of a TTS request; see {@link AurixEvents.ttsStatus}. */
export interface TtsStatus {
  requestId: string;
  clientRef?: string;
  state: TtsState;
  /** Audio length (ms), set once synthesis succeeded. */
  durationMs?: number;
  /** Sanitized failure reason for `failed`. */
  message?: string;
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
  /** Which media path came up (after `sessionReady`, again after every reconnect). */
  mediaTransport: (transport: MediaTransport) => void;
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
  /** Audio injection started (`true`) or ended (`false`: buffer finished, `stopAudioInjection`, media closed). */
  audioInjection: (active: boolean) => void;
  positions: (channelId: string, positions: UserPosition[]) => void;
  /**
   * The `mid → participant` layout of the per-participant downlink tracks changed (a speaker
   * got or lost a dedicated track, a track arrived or went away). Full snapshot.
   */
  participantStreams: (streams: ParticipantStreamInfo[]) => void;
  /** `live` — a real-time stream to an operator service rather than a stored file; same consent flow. */
  recording: (channelId: string, recordingId: string, active: boolean, initiatedBy: string, live: boolean) => void;
  /**
   * Adaptive-bitrate command from the server (applied by the SDK as the sender's `maxBitrate`,
   * bounded by the channel policy). `expectedLossPercent` is the loss estimate that triggered it.
   */
  bitrate: (targetKbps: number, reason: string, expectedLossPercent: number) => void;
  /**
   * Merged audio policy of the joined channels changed (join, leave, operator edit). The
   * bitrate part is applied live; FEC/DTX/bandwidth apply at the next media negotiation —
   * compare `negotiatedOpus` with `opusPreferences` and call `renegotiateMedia()` if it matters.
   */
  audioPolicy: (policy: AudioPolicy) => void;
  /**
   * Server-side view of this connection (downlink from our reports + uplink as measured by
   * the SFU): sent when the 1–5 `bars` change and periodically as a summary.
   */
  networkQuality: (quality: NetworkQuality) => void;
  /** Statistics snapshot taken before each periodic `QualityReport` (`qualityReportIntervalMs`). */
  stats: (stats: ClientStats) => void;
  kicked: (channelId: string, reason: string) => void;
  /**
   * Server-side snapshot of this user's receiver preferences, sent right after the session
   * is established: persistent cross-mutes plus (on a resumed session) local mutes/volumes.
   */
  receiverPreferences: (prefs: ReceiverPreferences) => void;
  /** A cross-mute placed or lifted by this user (from this or any other device/REST). */
  userBlockChanged: (userId: string, blocked: boolean) => void;
  /**
   * A member's priority-speaker state changed — including this user's (the ack of
   * `setPriority`, or a moderator's grant). `Participant.priority` / `ChannelInfo.priority`
   * are updated before the event fires.
   */
  participantPriorityChanged: (channelId: string, userId: string, priority: boolean) => void;
  /**
   * A member's *effective* role changed under `audience.speaker_admission` — including this
   * user's: `admitted` is `true` when it got its granted role (a speaker slot) back, `false`
   * when an idle speaker slot was taken from it (its audio is dropped meanwhile). The grant
   * itself is unchanged. `Participant.role` / `ChannelInfo.role` / `.waitingToSpeak` are
   * updated before the event fires.
   */
  participantRoleChanged: (
    channelId: string,
    userId: string,
    role: ChannelRole,
    admitted: boolean,
  ) => void;
  /**
   * Game-audio ducking hook: another member's priority speech started (`true`) or stopped
   * (`false`, after the channel's `holdMs`) ducking `channelId`. Fires on transitions only;
   * `config` is the channel's ducking so the game can match the ramps on its own mix.
   */
  duckingChanged: (channelId: string, active: boolean, config: DuckingConfig) => void;
  /**
   * Lip-sync frame for a participant's dedicated track (every 20 ms while visemes are on and
   * the participant holds a track; see {@link AurixClient.setVisemes}).
   */
  participantVisemes: (userId: string, frame: VisemeFrame) => void;
  /** Lip-sync frame for our own processed microphone (every 20 ms while visemes are on). */
  localVisemes: (frame: VisemeFrame) => void;
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
   * (peers never noticed) or a fresh one that the client re-joined to its channels;
   * `info.migrated` when another node took the session over.
   */
  recovered: (info: SessionInfo) => void;
  /**
   * A failover node answered while the previous one did not; the client now talks to `url`
   * (`endpoint` reflects it). Fires before that connection's `recovered`.
   */
  endpointChanged: (url: string) => void;
  /** Reconnect attempts exhausted or a definite refusal; the client is now `failed`. */
  failedToRecover: (error: Error) => void;
  /** The server ended the session (kick from the platform, ban, shutdown). No reconnect. */
  sessionClosed: (reason: string) => void;
  /**
   * A text message for this client: channel message of a joined channel, directed message
   * addressed to this user, or the echo (`own: true`) of a message this client sent.
   */
  chatMessage: (message: ChatMessage) => void;
  /**
   * A read marker moved: this user's own (from any device, including this one) or, when
   * the server has read receipts on, another participant's in a shared conversation.
   */
  chatReadMarker: (marker: ReadMarker) => void;
  /**
   * A message of one of this client's conversations was edited or deleted — the server's
   * full copy with the same `id`; replace the displayed one. The editing / deleting client
   * itself receives it too (also resolved from `editMessage` / `deleteMessage`).
   */
  chatMessageUpdated: (message: ChatMessage) => void;
  /** Someone (this user included) added or removed a reaction on a message this client can see. */
  chatReactionChanged: (change: ChatReactionChange) => void;
  /**
   * Once per connection, after the directed messages that arrived while this user was
   * offline were replayed as `chatMessage` events with `offline: true`. `truncated`: older
   * unread ones exist beyond the server's replay limit — page them with `history`.
   */
  chatInboxSynced: (delivered: number, truncated: boolean) => void;
  /** Another member of `channelId` started/stopped typing (never this client's own state). */
  participantTyping: (channelId: string, userId: string, typing: boolean) => void;
  /**
   * Speech-to-text of a channel member (or this user) in a channel with transcription
   * enabled. Ephemeral: the server does not store transcripts. Suppressed with
   * `setTranscripts(false)`; participants this client blocked/locally muted are not
   * transcribed for it either.
   */
  transcript: (transcript: Transcript) => void;
  /** The server applied (or a resumed session restored) this client's translation preference. */
  translationChanged: (prefs: TranslationPrefs) => void;
  /** Progress of a `speak()` request (queued → playing → finished/cancelled/failed). */
  ttsStatus: (status: TtsStatus) => void;
  /**
   * E2EE: a peer's identity key was learned (`previousFingerprint` undefined) or changed
   * (reconnect with a new key — or an impostor; compare fingerprints out of band).
   */
  e2eePeerKey: (userId: string, fingerprint: string, previousFingerprint: string | undefined) => void;
  /** E2EE: whether this client holds a sender key of `userId` (its frames are audible). */
  e2eePeerDecryptable: (userId: string, decryptable: boolean) => void;
  /** E2EE: this client's sender key rotated (a peer joined or left). */
  e2eeKeyRotated: (generation: number) => void;
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
/** `media.unfocused_channel_gain` of a node that does not advertise it. */
const DEFAULT_UNFOCUSED_GAIN = 0.5;
/** Join waves (a party entering together) collapse into one E2EE key rotation. */
const E2EE_ROTATE_DEBOUNCE_MS = 50;

interface Pending<T> {
  resolve: (value: T) => void;
  reject: (error: Error) => void;
  timer: ReturnType<typeof setTimeout> | undefined;
}

/** Lower-case, `_` → `-`, trimmed tag (the server validates further); `undefined` for empty input. */
function normalizeLanguageTag(tag: string | undefined): string | undefined {
  if (tag === undefined) return undefined;
  const t = tag.trim().toLowerCase().replace(/_/g, '-');
  return t === '' ? undefined : t;
}

function transcriptFromWire(t: TranscriptWire): Transcript {
  return {
    id: t.id,
    channelId: t.channel_id,
    userId: t.user_id,
    text: t.text,
    ...(t.language !== undefined && t.language !== null ? { language: t.language } : {}),
    startedAt: new Date(t.started_at),
    durationMs: t.duration_ms,
    words: (t.words ?? []).map((w) => ({ word: w.word, startMs: w.start_ms, endMs: w.end_ms })),
    ...(t.original
      ? {
          original: {
            text: t.original.text,
            ...(t.original.language ? { language: t.original.language } : {}),
          },
        }
      : {}),
  };
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
 * Voice: the browser's echo cancellation / noise suppression / AGC. Stereo uplink: two channels
 * with that processing off — browsers downmix to mono inside their voice processing chain.
 */
export function defaultAudioConstraints(opus: OpusBrowserOptions | undefined): MediaTrackConstraints {
  if (opus?.stereo === true) {
    return { channelCount: { ideal: 2 }, echoCancellation: false, noiseSuppression: false, autoGainControl: false };
  }
  return { echoCancellation: true, noiseSuppression: true, autoGainControl: true };
}

/**
 * Ask the browser to *decode* Opus in stereo (`stereo=1` = our receive preference, RFC 7587).
 * libwebrtc sizes its decoder from that parameter of the local description, not from the
 * packets, so without it the server's stereo downlink (directional positional channels) is
 * downmixed to mono before playout. The microphone uplink stays mono unless the server's answer
 * (rewritten with `opus.stereo`) asks for stereo.
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
    Pick<AurixClientOptions, 'useTurn' | 'pingIntervalMs' | 'requestTimeoutMs' | 'qualityReportIntervalMs'>
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
  /** Transceiver of the mixed downlink (the one that also carries the microphone). */
  private mixedTransceiver: RTCRtpTransceiver | undefined;
  /** mid → browser-side stream of a negotiated per-participant track. */
  private participantTracks = new Map<string, MediaStream>();
  /** mid → participant it carries (`ParticipantStreams`; `undefined` = idle). */
  private participantLayout = new Map<string, string | undefined>();
  /** Per-participant tracks the node serves at most (`SessionInitAck`). */
  private participantStreamCapValue = 0;
  /** Gain of unfocused channels' voices, mirrored from the node. */
  private unfocusedGain = DEFAULT_UNFOCUSED_GAIN;
  private pinnedParticipants: string[] = [];
  private renderer: SpatialRenderer | undefined;
  private visemesEnabledValue = false;
  /** mid → viseme analyser tapped off that track's decoded audio. */
  private visemeTaps = new Map<string, AudioWorkletNode>();
  /** mid → participant the tap's smoothing history belongs to (reset when the track is reassigned). */
  private visemeTapUsers = new Map<string, string | undefined>();
  /** user id → latest lip-sync frame from their track. */
  private participantVisemeFrames = new Map<string, VisemeFrame>();
  private localVisemeTap: AudioWorkletNode | undefined;
  private localVisemeFrame: VisemeFrame | undefined;
  private voiceEffectsValue: ResolvedVoiceEffectParams = VOICE_EFFECTS_BYPASS;
  private effectsNode: AudioWorkletNode | undefined;
  private effectsTask: Promise<void> = Promise.resolve();
  /** channel id → positional model of a positional channel (`ChannelJoinAck.positional`). */
  private channelPositional = new Map<string, PositionalConfigWire>();
  /** channel id → user id → last known position (ours from `updatePosition`, theirs from `PositionUpdate`). */
  private positions = new Map<string, Map<string, UserPosition>>();
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
  private pendingHistory = new Map<string, Pending<HistoryPage>>();
  private pendingSearch = new Map<string, Pending<SearchPage>>();
  private pendingReadMarkers = new Map<string, Pending<ReadMarkers>[]>();
  private chatRefCounter = 0;
  /** channel id → last `ChatTyping { typing: true }` sent (ms, `performance.now()` clock). */
  private typingSentAt = new Map<string, number>();
  /** client_ref → pending `speak()` (resolved by the `queued` status). */
  private pendingSpeak = new Map<string, Pending<SpeechRequest>>();
  /** client_ref → resolver of `SpeechRequest.done` for requests still in flight. */
  private speechDone = new Map<string, Pending<TtsStatus>>();
  private speakRefCounter = 0;
  private wantTranscripts = true;
  private translation: TranslationPrefs = { speech: false };
  /** Channels the server transcribes (from `ChannelJoinAck`). */
  private transcribedChannels = new Set<string>();
  private monitoredChannels = new Set<string>();
  /** channel id → presence / text scope (from `ChannelJoinAck`). */
  private channelScopes = new Map<string, ChannelScope>();
  /** channel id → role / participant count / audience flags (from `ChannelJoinAck`). */
  private channelInfos = new Map<string, ChannelInfo>();
  /** channel id → its audio policy (from `ChannelJoinAck.audio` / `ChannelAudioPolicy`). */
  private channelPolicies = new Map<string, AudioPolicy>();
  /** Channels a priority member is currently ducking (`duckingChanged` fired `true`). */
  private duckedChannels = new Set<string>();
  /** channel id → timer running the `holdMs` before ducking is released. */
  private duckHolds = new Map<string, ReturnType<typeof setTimeout>>();
  /** Merge of `channelPolicies`; kept after the last channel is left. */
  private audioPolicyValue: AudioPolicy | undefined;
  /** Last server `BitrateCommand` (bit/s); cleared when the policy or options change. */
  private transientBitrateBps: number | undefined;
  /** Sender preferences last pushed through `setParameters`. */
  private appliedSenderPrefs: OpusSenderPreferences | undefined;
  private pendingAnswer: Pending<string> | undefined;
  private pendingInit: (Pending<SessionInfo> & { url: string }) | undefined;
  private pingTimer: ReturnType<typeof setInterval> | undefined;
  private pingNonce = 1;
  private lastPingSentAt = 0;
  private rtt = new RttTracker();
  private qualityTimer: ReturnType<typeof setInterval> | undefined;
  private lossWindow = new LossWindow();
  private statsSnapshot: ClientStats | undefined;
  private serverQuality: NetworkQuality | undefined;
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
  private activeEndpoint: string;
  private failoverEndpoints: string[] = [];
  /** Group E2EE state; present once `connect()` found the browser capable (and `e2ee` not `false`). */
  private e2eeGroup: E2eeGroup | undefined;
  private e2eeApi: E2eeTransformApi | undefined;
  private e2eeWorker: Worker | undefined;
  private e2eeWorkerStats: E2eeFrameStats | undefined;
  /** Channels the server flagged `e2ee` in their join ack. */
  private e2eeChannels = new Set<string>();
  /** Serialises group operations (they await WebCrypto) in arrival order. */
  private e2eeQueue: Promise<unknown> = Promise.resolve();
  private e2eeRotateTimer: ReturnType<typeof setTimeout> | undefined;
  private e2eeModeValue: E2eeMode = 'plain';

  // ── AURX over WebTransport ──
  /** Session media key (`SessionInitAck.media_key`); seals AURX datagrams. */
  private mediaKey: Uint8Array | undefined;
  private webTransportInfo: WebTransportInfoWire | undefined;
  private mediaTransportValue: MediaTransport | undefined;
  private wt: AurxWebTransport | undefined;
  private wtCapture: AurxCapture | undefined;
  private wtPlayback: AurxPlayback | undefined;
  /** Downlink streams by SSRC: the participant they belong to and their last render inputs. */
  private readonly wtSlots = new Map<number, WebTransportSlot>();
  /** AURX channel hash → channel id for the channels we are in (rebuilt lazily). */
  private wtChannelByHash = new Map<number, string>();
  private wtIdleTimer: ReturnType<typeof setInterval> | undefined;
  /** Serialises E2EE frame decryption per SSRC (WebCrypto is async, playback wants order). */
  private wtDecryptQueue: Promise<void> = Promise.resolve();
  /** Uplink sequence carried across WebTransport reconnects (receivers keep their loss accounting). */
  private wtNextSequence: number | undefined;
  /** SSRC → user of the last WebTransport frame (E2EE frames need the sender to decrypt). */
  private wtUserBySsrc = new Map<number, string>();

  constructor(options: AurixClientOptions) {
    this.opts = {
      useTurn: true,
      pingIntervalMs: 15_000,
      requestTimeoutMs: 10_000,
      qualityReportIntervalMs: 5_000,
      autoReconnect: true,
      ...options,
    };
    this.reconnectPolicy = { ...DEFAULT_RECONNECT, ...(options.reconnect ?? {}) };
    this.activeEndpoint = options.wsUrl;
    this.userId = decodeJwtSubject(options.token);
    if (options.inputGain !== undefined) this.inputGainValue = checkInputGain(options.inputGain);
    this.inputDeviceIdValue = options.inputDeviceId;
    this.visemesEnabledValue = options.visemes === true;
    if (options.voiceEffects !== undefined) this.voiceEffectsValue = resolveVoiceEffects(options.voiceEffects);
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

  /** The media path in use once media is up (`undefined` before / between connections). */
  get mediaTransport(): MediaTransport | undefined {
    return this.mediaTransportValue;
  }

  /** WebSocket URL of the node the client talks to (`wsUrl` until a failover moved it). */
  get endpoint(): string {
    return this.activeEndpoint;
  }

  /** Alternate nodes advertised by the server for this session (see `endpointChanged`). */
  get failover(): readonly string[] {
    return this.failoverEndpoints;
  }

  get isMuted(): boolean {
    return this.muted;
  }

  /** Last measured application-level round-trip time (ms), from `Ping`/`Pong`. */
  get roundTripMs(): number {
    return this.rtt.last;
  }

  /**
   * Last server-side quality report (both directions; `bars` 1–5), or `undefined` until the
   * server has sent one (~2 s after media connects).
   */
  get networkQuality(): NetworkQuality | undefined {
    return this.serverQuality;
  }

  /** Last snapshot taken by `getStats()` (also refreshed by the periodic quality report). */
  get lastStats(): ClientStats | undefined {
    return this.statsSnapshot;
  }

  /**
   * Merged audio policy of the channels this session is in (widest bitrate/bandwidth, FEC if
   * any wants it, DTX only if all allow it), or `undefined` before the first `ChannelJoinAck`.
   */
  get audioPolicy(): AudioPolicy | undefined {
    return this.audioPolicyValue;
  }

  /** Policy of one joined channel, as sent by the server. */
  channelAudioPolicy(channelId: string): AudioPolicy | undefined {
    return this.channelPolicies.get(channelId);
  }

  /**
   * What the SDK currently wants from the browser's Opus encoder: `opus` options over the
   * merged channel policy, bitrate further capped by the last server `BitrateCommand`.
   */
  get opusPreferences(): OpusSenderPreferences {
    return resolveOpusSenderPreferences(this.opts.opus, this.audioPolicyValue, this.transientBitrateBps);
  }

  /**
   * Opus `fmtp` parameters of the current remote description — what the browser's encoder was
   * actually told at the last negotiation (`{}` without media).
   */
  get negotiatedOpus(): OpusSenderPreferences {
    return negotiatedOpusPreferences(this.pc?.remoteDescription?.sdp);
  }

  /**
   * Change the local Opus preferences at runtime. The bitrate ceiling is applied immediately;
   * FEC/DTX/bandwidth/CBR need `renegotiateMedia()` (or the next reconnect).
   */
  setOpusOptions(opus: OpusBrowserOptions | undefined): void {
    if (opus) this.opts.opus = opus;
    else delete this.opts.opus;
    this.transientBitrateBps = undefined;
    void this.applySenderPreferences();
  }

  /**
   * Negotiate a fresh media transport for the same session so that Opus `fmtp` preferences
   * (FEC/DTX/bandwidth/CBR) take effect. Audio is interrupted for roughly one ICE round trip;
   * channels, mutes and preferences are untouched.
   */
  async renegotiateMedia(): Promise<void> {
    this.requireOpen();
    if (!this.session) throw new Error('no session');
    await this.restoreMedia(false);
  }

  participants(channelId: string): Participant[] {
    return Array.from(this.channels.get(channelId)?.values() ?? []);
  }

  joinedChannels(): string[] {
    return Array.from(this.channels.keys());
  }

  // ── End-to-end encryption ──

  /** What this browser can do for E2EE (WebCrypto + an encoded-frame API). */
  static e2eeSupport(prefer: 'auto' | E2eeTransformApi = 'auto'): E2eeSupport {
    return detectE2eeSupport(prefer);
  }

  /**
   * `true` once `connect()` set E2EE up: the session announced the capability and encrypted
   * channels can be joined. `false` = the browser cannot (or `e2ee: false`): joining an
   * encrypted channel fails with `E2EE_REQUIRED`.
   */
  get e2eeAvailable(): boolean {
    return this.e2eeGroup !== undefined;
  }

  /** Encoded-frame API in use (`'script'` = `RTCRtpScriptTransform` in a worker, `'streams'` = `createEncodedStreams`). */
  get e2eeTransformApi(): E2eeTransformApi | undefined {
    return this.e2eeGroup ? this.e2eeApi : undefined;
  }

  /** Hex SHA-256 of this client's E2EE identity key — show it so peers can verify it out of band. */
  get e2eeFingerprint(): string | undefined {
    return this.e2eeGroup?.identity.fingerprint;
  }

  /** The identity secret to store and pass as `e2ee.identity` next time (keeps the fingerprint). */
  get e2eeIdentitySecret(): Uint8Array | undefined {
    return this.e2eeGroup?.identity.exportSecret();
  }

  /** Fingerprint of a peer's identity key, once it announced itself in a shared encrypted channel. */
  e2eePeerFingerprint(userId: string): string | undefined {
    return this.e2eeGroup?.peerFingerprint(userId);
  }

  /** Whether this client holds a sender key of `userId` (its encrypted frames are audible). */
  isE2eePeerDecryptable(userId: string): boolean {
    return this.e2eeGroup?.hasKeyFor(userId) ?? false;
  }

  /** Peers whose encrypted frames this client can decrypt. */
  e2eeDecryptablePeers(): string[] {
    return this.e2eeGroup?.decryptablePeers() ?? [];
  }

  /** The server flagged this joined channel as end-to-end encrypted. */
  isChannelEncrypted(channelId: string): boolean {
    return this.e2eeChannels.has(channelId);
  }

  /** Generation of this client's current sender key. */
  get e2eeGeneration(): number | undefined {
    return this.e2eeGroup?.generation;
  }

  /**
   * Frame counters of the encrypted path (worker path: as of the last snapshot, refreshed
   * with every `stats` event and by `refreshE2eeStats()`).
   */
  get e2eeStats(): E2eeFrameStats | undefined {
    if (!this.e2eeGroup) return undefined;
    return this.e2eeWorker ? this.e2eeWorkerStats ?? { framesE2ee: 0, undecryptable: 0, held: 0 } : { ...this.e2eeGroup.frames.stats };
  }

  /** Asks the transform worker for fresh frame counters (resolves with them). */
  refreshE2eeStats(): Promise<E2eeFrameStats | undefined> {
    const worker = this.e2eeWorker;
    if (!worker || !this.e2eeGroup) return Promise.resolve(this.e2eeStats);
    return new Promise((resolve) => {
      const done = (ev: MessageEvent<E2eeWorkerReply>) => {
        if (ev.data.type !== 'stats') return;
        worker.removeEventListener('message', done);
        resolve(ev.data.stats);
      };
      worker.addEventListener('message', done);
      this.postToE2eeWorker({ type: 'stats' });
    });
  }

  /** Rotates this client's sender key now (normally automatic on join/leave). */
  async rotateE2eeKey(): Promise<number | undefined> {
    const group = this.e2eeGroup;
    if (!group) return undefined;
    return this.e2eeTask(async () => {
      const r = await group.rotate(true);
      if (!r) return undefined;
      this.e2eeSend(r.out);
      this.emit('e2eeKeyRotated', r.generation);
      return r.generation;
    });
  }

  private e2eeEnabled(): boolean {
    return this.opts.e2ee !== false;
  }

  private e2eeOptions(): E2eeOptions {
    return typeof this.opts.e2ee === 'object' ? this.opts.e2ee : {};
  }

  /** Creates the group (identity + transform worker) on the first `connect()`, if the browser can. */
  private async ensureE2ee(): Promise<void> {
    if (this.e2eeGroup || !this.e2eeEnabled()) return;
    const o = this.e2eeOptions();
    const support = detectE2eeSupport(o.transform ?? 'auto');
    if (!support.ok) {
      if (support.crypto && this.webTransportPossible()) {
        // No encoded-frame API, but AURX over WebTransport encrypts frames itself: keep the
        // group and drop it at `SessionInitAck` if that path is not offered.
        const identity = await E2eeIdentity.create(o.identity);
        this.e2eeGroup = await E2eeGroup.create(identity);
        this.e2eeGroup.onRotateNeeded = () => this.scheduleE2eeRotate();
        this.e2eeApi = undefined;
        return;
      }
      if (this.opts.e2ee !== undefined) {
        const why = !support.crypto ? 'WebCrypto is unavailable (insecure context?)' : 'no encoded-frame API (RTCRtpScriptTransform / createEncodedStreams)';
        this.emit('error', new Error(`E2EE unavailable: ${why}; encrypted channels cannot be joined`));
      }
      return;
    }
    let api: E2eeTransformApi = support.transform ?? 'streams';
    let sink: E2eeKeySink | undefined;
    if (api === 'script') {
      try {
        const url = o.workerUrl ?? URL.createObjectURL(new Blob([e2eeWorkerSource()], { type: 'text/javascript' }));
        const worker = new Worker(url);
        worker.addEventListener('message', (ev: MessageEvent<E2eeWorkerReply>) => this.onE2eeWorkerMessage(ev.data));
        worker.addEventListener('error', (ev) => this.emit('error', new Error(`E2EE worker: ${ev.message}`)));
        this.e2eeWorker = worker;
        sink = { post: (msg) => this.postToE2eeWorker(msg) };
      } catch (e) {
        const fallback = detectE2eeSupport('streams');
        if (!fallback.ok) {
          if (this.opts.e2ee !== undefined) {
            this.emit('error', new Error(`E2EE unavailable: worker could not start (${e instanceof Error ? e.message : String(e)})`));
          }
          return;
        }
        api = 'streams';
      }
    }
    const identity = await E2eeIdentity.create(o.identity);
    this.e2eeGroup = await E2eeGroup.create(identity, sink);
    this.e2eeGroup.onRotateNeeded = () => this.scheduleE2eeRotate();
    this.e2eeApi = api;
  }

  private postToE2eeWorker(msg: E2eeWorkerMessage): void {
    this.e2eeWorker?.postMessage(msg);
  }

  private onE2eeWorkerMessage(msg: E2eeWorkerReply): void {
    switch (msg.type) {
      case 'rotate':
        void this.e2eeTask(async () => {
          const r = await this.e2eeGroup?.rotate(true);
          if (!r) return;
          this.e2eeSend(r.out);
          this.emit('e2eeKeyRotated', r.generation);
        });
        return;
      case 'stats':
        this.e2eeWorkerStats = msg.stats;
        return;
      default:
        return;
    }
  }

  private e2eeTask<T>(task: () => Promise<T>): Promise<T> {
    const run = this.e2eeQueue.then(task, task);
    this.e2eeQueue = run.catch((e: unknown) => {
      this.emit('error', e instanceof Error ? e : new Error(String(e)));
    });
    return run;
  }

  private e2eeSend(out: E2eeOutgoing[]): void {
    for (const msg of out) {
      if (msg.type === 'hello') {
        this.sendE2eeHello(msg.channelId);
      } else {
        this.send({
          type: 'E2eeSenderKey',
          data: {
            channel_id: msg.channelId,
            to: msg.to,
            public_key: bytesToBase64(this.e2eeGroup?.identity.publicKey ?? new Uint8Array(0)),
            generation: msg.generation,
            key: bytesToBase64(msg.wrapped),
          },
        });
      }
    }
  }

  private sendE2eeHello(channelId: string | undefined): void {
    const group = this.e2eeGroup;
    if (!group || !this.ws || this.ws.readyState !== WebSocket.OPEN) return;
    const data: { channel_id?: string; public_key: string } = { public_key: bytesToBase64(group.identity.publicKey) };
    if (channelId !== undefined) data.channel_id = channelId;
    this.send({ type: 'E2eeHello', data });
  }

  private setE2eeMode(mode: E2eeMode): void {
    if (this.e2eeModeValue === mode) return;
    this.e2eeModeValue = mode;
    if (this.e2eeGroup) this.e2eeGroup.frames.mode = mode;
    this.postToE2eeWorker({ type: 'mode', mode });
  }

  /** `encrypt` while an encrypted channel is joined, `hold` while a join is in flight, else `plain`. */
  private refreshE2eeMode(): void {
    if (!this.e2eeGroup) return;
    if (this.e2eeChannels.size > 0) this.setE2eeMode('encrypt');
    else if (this.pendingJoins.size > 0) this.setE2eeMode('hold');
    else this.setE2eeMode('plain');
  }

  private pushE2eeLayout(): void {
    if (!this.e2eeGroup) return;
    const entries: Array<[string, string | undefined]> = [...this.participantLayout.entries()];
    this.e2eeGroup.frames.setLayout(entries);
    this.postToE2eeWorker({ type: 'layout', entries });
  }

  private scheduleE2eeRotate(): void {
    if (!this.e2eeGroup || this.e2eeRotateTimer) return;
    this.e2eeRotateTimer = setTimeout(() => {
      this.e2eeRotateTimer = undefined;
      void this.e2eeTask(async () => {
        const group = this.e2eeGroup;
        if (!group || !group.isRotationPending || !group.active) return;
        const r = await group.rotate(false);
        if (!r) return;
        this.e2eeSend(r.out);
        this.emit('e2eeKeyRotated', r.generation);
      });
    }, E2EE_ROTATE_DEBOUNCE_MS);
  }

  private emitE2eeGone(gone: string[]): void {
    for (const userId of gone) this.emit('e2eePeerDecryptable', userId, false);
  }

  /** Join ack of an encrypted channel (`replayed`: a resumed session re-acknowledged it). */
  private e2eeChannelJoined(channelId: string, replayed: boolean, members: ReadonlySet<string>): void {
    const group = this.e2eeGroup;
    if (!group) return;
    this.e2eeChannels.add(channelId);
    if (replayed) {
      const { out, gone } = group.rejoined(channelId, members);
      this.emitE2eeGone(gone);
      this.e2eeSend(out);
    } else {
      this.e2eeSend(group.joined(channelId));
    }
    this.refreshE2eeMode();
    if (this.participantStreamsToOffer() === 0) {
      this.emit('error', new Error(`channel ${channelId} is end-to-end encrypted but no per-participant tracks are negotiated (participantStreams = 0): its members are inaudible`));
    }
  }

  private e2eeChannelLeft(channelId: string): void {
    if (!this.e2eeChannels.delete(channelId)) return;
    const group = this.e2eeGroup;
    if (group) {
      this.emitE2eeGone(group.left(channelId));
      this.refreshE2eeMode();
      if (group.active) this.scheduleE2eeRotate();
    }
  }

  private e2eePeerLeft(channelId: string, userId: string): void {
    const group = this.e2eeGroup;
    if (!group || !this.e2eeChannels.has(channelId)) return;
    this.emitE2eeGone(group.peerLeft(channelId, userId));
    this.scheduleE2eeRotate();
  }

  /** The session is gone (disconnect, fresh session after reconnect): forget channels and peers. */
  private e2eeSessionEnded(): void {
    this.e2eeChannels.clear();
    if (this.e2eeRotateTimer) {
      clearTimeout(this.e2eeRotateTimer);
      this.e2eeRotateTimer = undefined;
    }
    const group = this.e2eeGroup;
    if (group) this.emitE2eeGone(group.reset());
    this.refreshE2eeMode();
  }

  /**
   * Give up E2EE for this connection: the group is discarded and any encrypted channel we sit
   * in is left (we could neither read nor produce its frames).
   */
  private dropE2ee(why: string): void {
    if (!this.e2eeGroup) return;
    const channels = Array.from(this.e2eeChannels);
    this.e2eeSessionEnded();
    this.e2eeGroup = undefined;
    this.e2eeApi = undefined;
    if (this.opts.e2ee !== undefined || channels.length > 0) {
      this.emit('error', new Error(`E2EE unavailable: ${why}; encrypted channels cannot be joined`));
    }
    for (const channelId of channels) {
      if (!this.channels.has(channelId)) continue;
      this.trySend({ type: 'ChannelLeave', data: { channel_id: channelId } });
      this.forgetChannel(channelId);
    }
  }

  private onE2eeHello(d: { channel_id?: string; user_id?: string; public_key: string }): void {
    const group = this.e2eeGroup;
    const channelId = d.channel_id;
    const userId = d.user_id;
    if (!group || channelId === undefined || userId === undefined || !this.e2eeChannels.has(channelId)) return;
    if (userId === this.userId) return;
    void this.e2eeTask(async () => {
      const { out, change } = await group.onHello(channelId, userId, base64ToBytes(d.public_key));
      this.e2eeSend(out);
      if (change) {
        this.emit('e2eePeerKey', userId, group.peerFingerprint(userId) ?? '', change.kind === 'keyChanged' ? change.previousFingerprint : undefined);
        this.scheduleE2eeRotate();
      }
    });
  }

  private onE2eeSenderKey(d: { channel_id: string; from?: string; to: string; public_key: string; generation: number; key: string }): void {
    const group = this.e2eeGroup;
    const from = d.from;
    if (!group || from === undefined || from === this.userId || !this.e2eeChannels.has(d.channel_id)) return;
    void this.e2eeTask(async () => {
      let result;
      try {
        result = await group.onSenderKey(d.channel_id, from, base64ToBytes(d.public_key), d.generation, base64ToBytes(d.key));
      } catch (e) {
        this.emit('error', new Error(`E2EE key from ${from} rejected: ${e instanceof Error ? e.message : String(e)}`));
        return;
      }
      const { out, change, decryptable } = result;
      if (change) {
        this.emit('e2eePeerKey', from, group.peerFingerprint(from) ?? '', change.kind === 'keyChanged' ? change.previousFingerprint : undefined);
        this.scheduleE2eeRotate();
      }
      this.e2eeSend(out);
      if (decryptable) this.emit('e2eePeerDecryptable', from, true);
    });
  }

  /** Installs the encrypted-frame transforms on every sender/receiver of `pc`. */
  private attachE2eeTransforms(pc: RTCPeerConnection, mixed: RTCRtpTransceiver): void {
    const group = this.e2eeGroup;
    if (!group) return;
    const worker = this.e2eeWorker;
    for (const t of pc.getTransceivers()) {
      const midOf = () => t.mid;
      if (t === mixed) {
        if (worker && this.e2eeApi === 'script') {
          t.sender.transform = new RTCRtpScriptTransform(worker, { kind: 'sender', mid: t.mid });
        } else {
          attachEncodedStreams(t.sender, group.frames, 'sender', midOf);
        }
      }
      if (worker && this.e2eeApi === 'script') {
        t.receiver.transform = new RTCRtpScriptTransform(worker, { kind: 'receiver', mid: t.mid });
      } else {
        attachEncodedStreams(t.receiver, group.frames, 'receiver', midOf);
      }
    }
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

  /** `true` while {@link injectAudio} is playing into the uplink. */
  get injectingAudio(): boolean {
    return this.inputPipeline?.injecting ?? false;
  }

  // ── Voice effects (uplink) ──

  /** Whether this browser can run the effects worklet (`AudioWorklet` + `blob:` modules). */
  static supportsVoiceEffects(): boolean {
    return supportsVoiceEffects();
  }

  /**
   * Voice effects on the outgoing microphone: a preset (`robot`, `monster`, `radio`,
   * `helium`, `ghost`), explicit {@link VoiceEffectParams}, or `undefined` / all-zero for
   * bypass. Runs in an `AudioWorklet` on the microphone path only — injected audio and the
   * downlink are untouched — after the input gain and before the encoder, so the server and
   * the other participants only ever hear the effected voice. Applied to the current
   * microphone at once and to every later one; rejects when Web Audio / worklets are missing.
   */
  async setVoiceEffects(effects: VoiceEffectParams | VoiceEffectPreset | undefined): Promise<void> {
    const resolved = resolveVoiceEffects(effects);
    this.voiceEffectsValue = resolved;
    if (!this.localStream) return;
    if (isVoiceEffectsBypass(resolved) && !this.effectsNode) return;
    if (!supportsVoiceEffects()) throw new Error('AudioWorklet is unavailable: voice effects are not supported here');
    this.ensureInputPipeline(this.localStream);
    await this.applyVoiceEffects();
  }

  /** Effects currently applied to the microphone (all zero = bypass). */
  get voiceEffects(): ResolvedVoiceEffectParams {
    return { ...this.voiceEffectsValue };
  }

  // ── Visemes (lip-sync) ──

  /** Whether this browser can run the viseme worklet. */
  static supportsVisemes(): boolean {
    return supportsVisemes();
  }

  /**
   * Turn local lip-sync analysis on or off. On: every participant with a dedicated track
   * (per-participant streams + Web Audio rendering; the mixed track cannot be split) and our
   * own processed microphone get a {@link VisemeFrame} per 20 ms — `participantVisemes` /
   * `localVisemes` events plus {@link getParticipantVisemes} / {@link getLocalVisemes}.
   * The analysis runs here on decoded (decrypted) audio; no phoneme data leaves the browser.
   * Rejects when Web Audio / worklets are missing.
   */
  async setVisemes(enabled: boolean): Promise<void> {
    if (enabled && !supportsVisemes()) throw new Error('AudioWorklet is unavailable: visemes are not supported here');
    this.visemesEnabledValue = enabled;
    if (!enabled) {
      this.detachAllVisemeTaps();
      return;
    }
    if (this.localStream) this.ensureInputPipeline(this.localStream);
    await this.syncVisemeTaps();
  }

  get visemesEnabled(): boolean {
    return this.visemesEnabledValue;
  }

  /** Latest lip-sync frame of `userId` (`undefined` without visemes or a dedicated track for them). */
  getParticipantVisemes(userId: string): VisemeFrame | undefined {
    return this.participantVisemeFrames.get(userId);
  }

  /** Latest lip-sync frame of our own microphone (`undefined` before any audio was analysed). */
  getLocalVisemes(): VisemeFrame | undefined {
    return this.localVisemeFrame;
  }

  /** Whether gain, effects or visemes need the microphone routed through Web Audio. */
  private needsInputPipeline(): boolean {
    return this.inputGainValue !== 1 || !isVoiceEffectsBypass(this.voiceEffectsValue) || this.visemesEnabledValue;
  }

  /** (Re)attach effects and the local viseme tap to the current input pipeline. */
  private syncMicrophoneWorklets(): void {
    if (!this.inputPipeline) return;
    if (!isVoiceEffectsBypass(this.voiceEffectsValue) || this.effectsNode) {
      void this.applyVoiceEffects().catch((e: unknown) => {
        this.emit('error', e instanceof Error ? e : new Error(String(e)));
      });
    }
    if (this.visemesEnabledValue) {
      void this.syncVisemeTaps().catch((e: unknown) => {
        this.emit('error', e instanceof Error ? e : new Error(String(e)));
      });
    }
  }

  /** Serialised: loading the worklet is async and two callers must not both create a node. */
  private applyVoiceEffects(): Promise<void> {
    const run = (): Promise<void> => this.applyVoiceEffectsNow();
    this.effectsTask = this.effectsTask.then(run, run);
    return this.effectsTask;
  }

  private async applyVoiceEffectsNow(): Promise<void> {
    const pipeline = this.inputPipeline;
    const ctx = pipeline?.audioContext;
    if (!pipeline || !ctx) return;
    const params = this.voiceEffectsValue;
    if (isVoiceEffectsBypass(params)) {
      if (this.effectsNode) {
        pipeline.setEffects(undefined);
        this.effectsNode.port.close();
        this.effectsNode = undefined;
      }
      return;
    }
    if (this.effectsNode && pipeline.hasEffects) {
      const msg: VoiceEffectsWorkletMessage = { type: 'params', params };
      this.effectsNode.port.postMessage(msg);
      return;
    }
    await loadVoiceEffectsWorklet(ctx);
    if (this.inputPipeline !== pipeline) return;
    // Parameters may have changed while the module loaded.
    const node = createVoiceEffectsNode(ctx, this.voiceEffectsValue);
    this.effectsNode = node;
    pipeline.setEffects(node);
  }

  /** Attach viseme taps to every participant track and the microphone that lack one. */
  private async syncVisemeTaps(): Promise<void> {
    if (!this.visemesEnabledValue) return;
    const pipeline = this.inputPipeline;
    const micCtx = pipeline?.audioContext;
    if (pipeline && micCtx && !this.localVisemeTap) {
      await loadVisemeWorklet(micCtx);
      if (this.visemesEnabledValue && this.inputPipeline === pipeline && !this.localVisemeTap) {
        const tap = createVisemeNode(micCtx, (frame) => {
          this.localVisemeFrame = frame;
          this.emit('localVisemes', frame);
        });
        this.localVisemeTap = tap;
        pipeline.addTap(tap);
      }
    }
    const renderer = this.renderer;
    const ctx = renderer ? workletContext(renderer.context) : undefined;
    if (!renderer || !ctx) return;
    await loadVisemeWorklet(ctx);
    if (!this.visemesEnabledValue || this.renderer !== renderer) return;
    for (const mid of this.participantTracks.keys()) this.attachVisemeTap(mid);
    for (const ssrc of this.wtSlots.keys()) this.attachVisemeTap(wtSlotKey(ssrc));
  }

  private attachVisemeTap(mid: string): void {
    const renderer = this.renderer;
    if (!renderer || this.visemeTaps.has(mid)) return;
    const ctx = workletContext(renderer.context);
    if (!ctx) return;
    const tap = createVisemeNode(ctx, (frame) => {
      const userId = this.slotUser(mid);
      if (userId === undefined || this.visemeTaps.get(mid) !== tap) return;
      this.participantVisemeFrames.set(userId, frame);
      this.emit('participantVisemes', userId, frame);
    });
    if (!renderer.addTap(mid, tap)) {
      tap.port.close();
      return;
    }
    this.visemeTaps.set(mid, tap);
    this.visemeTapUsers.set(mid, this.slotUser(mid));
  }

  /** The participant heard on renderer slot `mid` (a WebRTC track or a WebTransport SSRC). */
  private slotUser(mid: string): string | undefined {
    const fromLayout = this.participantLayout.get(mid);
    if (fromLayout !== undefined) return fromLayout;
    for (const [ssrc, slot] of this.wtSlots) if (wtSlotKey(ssrc) === mid) return slot.userId;
    return undefined;
  }

  private detachVisemeTap(mid: string): void {
    const tap = this.visemeTaps.get(mid);
    if (!tap) return;
    this.visemeTaps.delete(mid);
    this.renderer?.removeTap(mid, tap);
    tap.port.close();
    const user = this.visemeTapUsers.get(mid);
    this.visemeTapUsers.delete(mid);
    if (user !== undefined) this.participantVisemeFrames.delete(user);
  }

  private detachAllVisemeTaps(): void {
    for (const mid of Array.from(this.visemeTaps.keys())) this.detachVisemeTap(mid);
    this.participantVisemeFrames.clear();
    if (this.localVisemeTap) {
      this.inputPipeline?.removeTap(this.localVisemeTap);
      this.localVisemeTap.port.close();
      this.localVisemeTap = undefined;
    }
    this.localVisemeFrame = undefined;
  }

  /** The track behind `mid` now carries someone else: their frames start clean. */
  private visemeLayoutChanged(): void {
    for (const [mid, tap] of this.visemeTaps) {
      const now = this.participantLayout.get(mid);
      const before = this.visemeTapUsers.get(mid);
      if (now === before) continue;
      this.visemeTapUsers.set(mid, now);
      if (before !== undefined) this.participantVisemeFrames.delete(before);
      tap.port.postMessage({ type: 'reset' });
    }
  }

  /**
   * Decode an encoded audio file (wav/ogg/mp3/… — whatever the browser decodes) for
   * {@link injectAudio}. Usable before media is up.
   */
  async decodeAudio(data: ArrayBuffer): Promise<AudioBuffer> {
    if (typeof AudioContext === 'undefined') throw new Error('Web Audio is unavailable');
    const shared = this.inputPipeline?.audioContext;
    const ctx = shared ?? new AudioContext();
    try {
      return await ctx.decodeAudioData(data.slice(0));
    } finally {
      if (!shared) void ctx.close().catch(() => undefined);
    }
  }

  /**
   * Play a decoded buffer or a live `MediaStream` (e.g. `HTMLMediaElement.captureStream()`)
   * into every channel the microphone goes to — a sound test in an `echo` channel, a bot
   * voice, an in-game radio. Mixed over the microphone unless `mixWithMicrophone: false`;
   * `setMuted(true)` silences both. Requires media (`connect()` first). Replaces a previous
   * injection; fires `audioInjection(true)` now and `audioInjection(false)` when it ends.
   */
  injectAudio(source: AudioInjectionSource, options: AudioInjectionOptions = {}): void {
    if (!this.localStream) throw new Error('injectAudio needs media: call connect() first');
    this.ensureInputPipeline(this.localStream);
    const pipeline = this.inputPipeline;
    if (!pipeline) throw new Error('Web Audio is unavailable: audio injection is not supported here');
    pipeline.onInjectionEnded = () => this.emit('audioInjection', false);
    const wasInjecting = pipeline.injecting;
    pipeline.inject(source, options);
    if (!wasInjecting) this.emit('audioInjection', true);
  }

  /** Stop {@link injectAudio}; the microphone is audible again immediately. */
  stopAudioInjection(): void {
    if (this.inputPipeline?.stopInjection()) this.emit('audioInjection', false);
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
      await Promise.all([
        ...Array.from(this.outputElements, (el) => el.setSinkId(deviceId ?? '')),
        this.renderer?.setSinkId(deviceId) ?? Promise.resolve(),
      ]);
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
    this.renderer?.setMasterVolume(volume);
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
    this.renderer?.setMasterMuted(muted);
  }

  /**
   * Autoplay policy: call from a user gesture (click/tap) to start playback that the browser
   * held back — attached output elements and the Web Audio graph of per-participant tracks.
   * Resolves to `true` when audio can play.
   */
  async resumeAudio(): Promise<boolean> {
    let ok = true;
    if (this.renderer) ok = await this.renderer.resume();
    if (this.wtCapture && !(await this.wtCapture.resume())) ok = false;
    for (const el of this.outputElements) {
      if (el.paused && el.srcObject) {
        try {
          await el.play();
        } catch {
          ok = false;
        }
      }
    }
    return ok;
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
    this.renderParticipant(userId);
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
   * Multiplies positional attenuation; applied by the server before mixing and by this
   * client on the participant's own downlink track.
   */
  setParticipantVolume(userId: string, volume: number): void {
    if (!Number.isFinite(volume) || volume < 0 || volume > MAX_PARTICIPANT_VOLUME) {
      throw new RangeError(`volume must be within 0..${MAX_PARTICIPANT_VOLUME}`);
    }
    if (volume === 1) this.volumes.delete(userId);
    else this.volumes.set(userId, volume);
    this.renderParticipant(userId);
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
   * Make `userId` (this user when omitted) a priority speaker of `channelId` — or take it
   * back. Promoting others needs a moderator role; a member whose grant carries `priority`
   * may toggle their own state. Confirmed via `participantPriorityChanged`; rejected in
   * channels without ducking (`VALIDATION_ERROR`).
   */
  setPriority(channelId: string, priority: boolean, userId?: string): void {
    this.requireOpen();
    this.send({
      type: 'SetPriority',
      data: { channel_id: channelId, user_id: userId ?? null, priority },
    });
  }

  /** Whether this user is a priority speaker of `channelId`. */
  isPriority(channelId: string): boolean {
    return this.channelInfos.get(channelId)?.priority === true;
  }

  /** Priority-speaker ducking of a joined channel (`undefined` = off / not joined). */
  getChannelDucking(channelId: string): DuckingConfig | undefined {
    return this.channelInfos.get(channelId)?.ducking;
  }

  /**
   * Whether another member's priority speech is ducking `channelId` right now (what
   * `duckingChanged` last reported).
   */
  isDuckingActive(channelId: string): boolean {
    return this.duckedChannels.has(channelId);
  }

  /** Whether `p`'s speech ducks `channelId` for this listener (never our own speech). */
  private ducks(channelId: string, p: Participant): boolean {
    return p.userId !== this.userId && p.speaking && this.isPriorityMember(channelId, p);
  }

  /** Whether `p` counts as a priority member of `channelId` (never ducked; ducks the others when speaking). */
  private isPriorityMember(channelId: string, p: Participant): boolean {
    const cfg = this.channelInfos.get(channelId)?.ducking;
    if (!cfg) return false;
    return p.priority || (cfg.moderators && (p.role === 'moderator' || p.role === 'administrator'));
  }

  /**
   * Whether the other voices of `channelId` are ducked on their tracks right now: another
   * priority member holds it (as reported by `duckingChanged`), or our own priority speech
   * does — the server ducks the mix for the speaker too, so the tracks follow.
   */
  private isChannelDucked(channelId: string): boolean {
    if (this.duckedChannels.has(channelId)) return true;
    if (!this.userId) return false;
    const me = this.channels.get(channelId)?.get(this.userId);
    return me !== undefined && me.speaking && this.isPriorityMember(channelId, me);
  }

  /**
   * Re-evaluate ducking of `channelId` from its members. Activation is immediate; release
   * waits the channel's `holdMs` so pauses between words do not pump the game mix.
   */
  private refreshDucking(channelId: string): void {
    const info = this.channelInfos.get(channelId);
    const roster = this.channels.get(channelId);
    let active = false;
    if (info?.ducking && roster) {
      for (const p of roster.values()) {
        if (this.ducks(channelId, p)) {
          active = true;
          break;
        }
      }
    }
    const hold = this.duckHolds.get(channelId);
    if (active) {
      if (hold !== undefined) {
        clearTimeout(hold);
        this.duckHolds.delete(channelId);
      }
      if (!this.duckedChannels.has(channelId) && info?.ducking) {
        this.duckedChannels.add(channelId);
        this.renderParticipants();
        this.emit('duckingChanged', channelId, true, info.ducking);
      }
      return;
    }
    if (!this.duckedChannels.has(channelId) || hold !== undefined) return;
    const release = (): void => {
      this.duckHolds.delete(channelId);
      if (!this.duckedChannels.delete(channelId)) return;
      this.renderParticipants();
      const cfg = this.channelInfos.get(channelId)?.ducking ?? info?.ducking;
      this.emit('duckingChanged', channelId, false, cfg ?? DEFAULT_DUCKING);
    };
    const holdMs = info?.ducking?.holdMs ?? 0;
    if (holdMs > 0) this.duckHolds.set(channelId, setTimeout(release, holdMs));
    else release();
  }

  /** Channel left / kicked / session gone: release its ducking at once. */
  private dropDucking(channelId: string): void {
    const hold = this.duckHolds.get(channelId);
    if (hold !== undefined) {
      clearTimeout(hold);
      this.duckHolds.delete(channelId);
    }
    if (!this.duckedChannels.delete(channelId)) return;
    this.renderParticipants();
    this.emit('duckingChanged', channelId, false, this.channelInfos.get(channelId)?.ducking ?? DEFAULT_DUCKING);
  }

  private dropAllDucking(): void {
    for (const channelId of Array.from(this.duckedChannels)) this.dropDucking(channelId);
    for (const timer of this.duckHolds.values()) clearTimeout(timer);
    this.duckHolds.clear();
  }

  /** Ducking to apply to `userId`'s own track: the strongest of the ducked shared channels they do not lead. */
  private duckingFor(userId: string, shared: readonly string[]): DuckParams | undefined {
    let out: DuckParams | undefined;
    for (const ch of shared) {
      if (!this.isChannelDucked(ch)) continue;
      const cfg = this.channelInfos.get(ch)?.ducking;
      const p = this.channels.get(ch)?.get(userId);
      if (!cfg || !p) continue;
      // A priority member's own voice is never ducked (they may be silent between words).
      if (this.isPriorityMember(ch, p)) continue;
      if (!out || cfg.gain < out.gain) out = { gain: cfg.gain, attackMs: cfg.attackMs, releaseMs: cfg.releaseMs };
    }
    return out;
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
    this.renderParticipants();
    if (channelId !== undefined && !this.channels.has(channelId)) return; // sent on join
    this.trySend({ type: 'SetChannelFocus', data: { channel_id: channelId ?? null } });
  }

  getChannelFocus(): string | undefined {
    return this.focusChannel;
  }

  // ── Per-participant downlink tracks ──

  /** Per-participant downlink tracks the node serves this browser at most (`0` = mixed only). */
  get participantStreamCap(): number {
    return this.participantStreamCapValue;
  }

  /** Per-participant tracks actually negotiated with the current peer connection. */
  get negotiatedParticipantStreams(): number {
    return this.participantTracks.size;
  }

  /**
   * Keep these participants on their own downlink track whenever they are audible (a raid
   * leader, party members) — others share the remaining tracks by recent activity and fall
   * back to the mix. At most `participantStreamCap` ids; survives reconnects.
   */
  setPinnedParticipants(userIds: readonly string[]): void {
    const pinned = Array.from(new Set(userIds));
    if (this.session && pinned.length > this.participantStreamCapValue) {
      throw new RangeError(`at most ${this.participantStreamCapValue} participants can be pinned`);
    }
    this.pinnedParticipants = pinned;
    if (this.pc) this.trySend({ type: 'SetParticipantStreams', data: { pinned } });
  }

  getPinnedParticipants(): string[] {
    return [...this.pinnedParticipants];
  }

  /** Current layout of the per-participant tracks (see the `participantStreams` event). */
  getParticipantStreams(): ParticipantStreamInfo[] {
    if (this.wt) {
      return Array.from(this.wtSlots, ([ssrc, slot]) => ({ mid: wtSlotKey(ssrc), userId: slot.userId, stream: undefined, live: true }));
    }
    const mids = new Set([...this.participantLayout.keys(), ...this.participantTracks.keys()]);
    return Array.from(mids, (mid) => {
      const stream = this.participantTracks.get(mid);
      return { mid, userId: this.participantLayout.get(mid), stream, live: stream !== undefined };
    });
  }

  /** The dedicated downlink stream carrying `userId` right now, if any. */
  getParticipantStream(userId: string): MediaStream | undefined {
    for (const [mid, user] of this.participantLayout) {
      if (user === userId) return this.participantTracks.get(mid);
    }
    return undefined;
  }

  /** `true` when `userId`'s voice is rendered through the HRTF panner right now. */
  isParticipantSpatialized(userId: string): boolean {
    if (this.wt) {
      for (const [ssrc, slot] of this.wtSlots) {
        if (slot.userId === userId) return this.renderer?.isSpatial(wtSlotKey(ssrc)) === true;
      }
      return false;
    }
    for (const [mid, user] of this.participantLayout) {
      if (user === userId) return this.renderer?.isSpatial(mid) === true;
    }
    return false;
  }

  /** Whether this client builds the Web Audio graph for participant tracks. */
  private rendersParticipantTracks(): boolean {
    return this.opts.spatialAudio !== false;
  }

  /** Per-participant tracks to offer: the app's wish, capped by the node, `0` without Web Audio rendering. */
  private participantStreamsToOffer(): number {
    const cap = this.participantStreamCapValue;
    const wanted = this.opts.participantStreams ?? cap;
    const n = Math.max(0, Math.min(cap, Math.floor(wanted)));
    if (n === 0) return 0;
    if (!this.rendersParticipantTracks()) return n;
    return this.ensureRenderer() ? n : 0;
  }

  /** The Web Audio graph remote voices are rendered through (created on first use). */
  private ensureRenderer(): SpatialRenderer | undefined {
    if (this.renderer) return this.renderer;
    const ctx = (this.opts.audioContext as SpatialAudioContextLike | undefined) ?? createAudioContext();
    if (!ctx) return undefined;
    const renderer = new SpatialRenderer(ctx, {
      panningModel: this.opts.spatialAudio === 'equalpower' ? 'equalpower' : 'HRTF',
    });
    renderer.setMasterVolume(this.outputVolumeValue);
    renderer.setMasterMuted(this.outputMutedValue);
    if (this.outputDeviceIdValue !== undefined) {
      void renderer.setSinkId(this.outputDeviceIdValue).catch(() => undefined);
    }
    this.renderer = renderer;
    return renderer;
  }

  private onParticipantTrack(mid: string, stream: MediaStream): void {
    this.participantTracks.set(mid, stream);
    this.renderer?.addTrack(mid, stream);
    if (this.visemesEnabledValue) {
      void this.syncVisemeTaps().catch((e: unknown) => {
        this.emit('error', e instanceof Error ? e : new Error(String(e)));
      });
    }
    const track = stream.getAudioTracks()[0];
    track?.addEventListener('ended', () => {
      if (this.participantTracks.get(mid) !== stream) return;
      this.participantTracks.delete(mid);
      this.detachVisemeTap(mid);
      this.renderer?.removeTrack(mid);
      this.emitParticipantStreams();
    });
    this.renderMid(mid);
    this.emitParticipantStreams();
  }

  private applyParticipantLayout(streams: ParticipantStreamWire[]): void {
    this.participantLayout = new Map(streams.map((s) => [s.mid, s.user_id ?? undefined]));
    this.visemeLayoutChanged();
    this.pushE2eeLayout();
    this.renderParticipants();
    this.emitParticipantStreams();
  }

  private emitParticipantStreams(): void {
    this.emit('participantStreams', this.getParticipantStreams());
  }

  /** Channels this client shares with `userId` (where they are in the roster). */
  private sharedChannels(userId: string): string[] {
    const out: string[] = [];
    for (const [channelId, roster] of this.channels) if (roster.has(userId)) out.push(channelId);
    return out;
  }

  /** What the server would have applied to `userId`'s voice, for the browser to apply instead. */
  private renderInputsFor(userId: string): RenderInputs {
    const shared = this.sharedChannels(userId);
    const silenced =
      this.blockedUsers.has(userId) ||
      (shared.length > 0
        ? shared.every((ch) => this.isParticipantMuted(userId, ch))
        : this.isParticipantMuted(userId));
    const focus = this.focusChannel;
    const focusFactor =
      focus === undefined || shared.length === 0 || shared.includes(focus) ? 1 : this.unfocusedGain;
    const inputs: RenderInputs = { volume: this.getParticipantVolume(userId), silenced, focusFactor };
    const ducking = this.duckingFor(userId, shared);
    if (ducking) inputs.ducking = ducking;
    for (const ch of shared) {
      const config = this.channelPositional.get(ch);
      const known = this.positions.get(ch);
      const me = this.userId ? known?.get(this.userId) : undefined;
      const them = known?.get(userId);
      if (config && me && them) {
        inputs.positional = { config, listener: me.position, orientation: me.orientation, source: them.position };
        break;
      }
    }
    return inputs;
  }

  private renderMid(mid: string): void {
    if (!this.renderer || !this.participantTracks.has(mid)) return;
    const userId = this.participantLayout.get(mid);
    this.renderer.render(mid, userId ? renderParams(this.renderInputsFor(userId)) : { gain: 0 });
  }

  private renderParticipant(userId: string): void {
    if (!this.renderer) return;
    for (const [mid, user] of this.participantLayout) if (user === userId) this.renderMid(mid);
    for (const [ssrc, slot] of this.wtSlots) {
      if (slot.e2ee && slot.userId === userId) this.renderWebTransportSlot(ssrc, slot, undefined);
    }
  }

  private renderParticipants(): void {
    if (!this.renderer) return;
    for (const mid of this.participantTracks.keys()) this.renderMid(mid);
    for (const [ssrc, slot] of this.wtSlots) if (slot.e2ee) this.renderWebTransportSlot(ssrc, slot, undefined);
  }

  /** Remember a position of `channelId`'s member for browser-side spatialisation. */
  private rememberPositions(channelId: string, positions: readonly UserPosition[]): void {
    if (!this.channelPositional.has(channelId)) return;
    let known = this.positions.get(channelId);
    if (!known) {
      known = new Map();
      this.positions.set(channelId, known);
    }
    for (const p of positions) known.set(p.user_id, p);
  }

  private forgetChannelSpatial(channelId: string): void {
    this.channelPositional.delete(channelId);
    this.positions.delete(channelId);
  }

  /** Drop every negotiated participant track (peer connection gone); the layout is the server's to resend. */
  private clearParticipantTracks(): void {
    const had = this.participantTracks.size > 0 || this.participantLayout.size > 0;
    for (const mid of Array.from(this.visemeTaps.keys())) this.detachVisemeTap(mid);
    this.participantVisemeFrames.clear();
    this.participantTracks.clear();
    this.participantLayout.clear();
    this.pushE2eeLayout();
    this.mixedTransceiver = undefined;
    this.renderer?.clear();
    if (had) this.emitParticipantStreams();
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
    if (!this.wantTranscripts) {
      this.trySend({ type: 'SetTranscripts', data: { enabled: false } });
    }
    if (this.translation.language !== undefined || this.translation.spokenLanguage !== undefined) {
      this.trySend({ type: 'SetTranslation', data: this.translationWire() });
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
    this.activeEndpoint = this.opts.wsUrl;
    this.failoverEndpoints = [];
    this.rtt.reset();
    this.serverQuality = undefined;
    this.statsSnapshot = undefined;
    this.setState('connecting');

    let info: SessionInfo;
    try {
      await this.ensureE2ee();
      if (this.closedByUser) throw new Error('disconnected');
      info = await this.openControlChannel(this.opts.wsUrl);
    } catch (e) {
      if (!this.closedByUser) this.teardown('connect failed', 'failed');
      throw e;
    }
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
    this.clearParticipantTracks();
    this.stopWebTransportMedia();
    this.wtNextSequence = undefined;
    this.mediaKey = undefined;
    this.webTransportInfo = undefined;
    if (this.renderer && this.opts.audioContext === undefined) {
      void this.renderer.close();
      this.renderer = undefined;
    }
    this.releaseLocalStream(this.localStream);
    this.localStream = undefined;
    const wasInjecting = this.inputPipeline?.injecting ?? false;
    if (this.localVisemeTap) {
      this.inputPipeline?.removeTap(this.localVisemeTap);
      this.localVisemeTap.port.close();
      this.localVisemeTap = undefined;
    }
    this.localVisemeFrame = undefined;
    if (this.effectsNode) {
      this.effectsNode.port.close();
      this.effectsNode = undefined;
    }
    this.inputPipeline?.close();
    this.inputPipeline = undefined;
    if (wasInjecting) this.emit('audioInjection', false);
    this.remoteStream = undefined;
    for (const el of this.outputElements) el.srcObject = null;
    if (typeof navigator !== 'undefined') {
      navigator.mediaDevices?.removeEventListener?.('devicechange', this.onDeviceChange);
    }
    for (const channelId of this.channels.keys()) this.emit('channelLeft', channelId);
    this.channels.clear();
    this.dropAllDucking();
    this.channelPositional.clear();
    this.positions.clear();
    this.e2eeSessionEnded();
    this.session = undefined;
    this.resumeToken = undefined;
    this.stopQualityTimer();
    this.serverQuality = undefined;
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
    for (const p of this.pendingHistory.values()) {
      clearTimeout(p.timer);
      p.reject(new Error(reason));
    }
    this.pendingHistory.clear();
    for (const p of this.pendingSearch.values()) {
      clearTimeout(p.timer);
      p.reject(new Error(reason));
    }
    this.pendingSearch.clear();
    for (const list of this.pendingReadMarkers.values()) {
      for (const p of list) {
        clearTimeout(p.timer);
        p.reject(new Error(reason));
      }
    }
    this.pendingReadMarkers.clear();
    this.typingSentAt.clear();
    for (const p of this.pendingSpeak.values()) {
      clearTimeout(p.timer);
      p.reject(new Error(reason));
    }
    this.pendingSpeak.clear();
    for (const p of this.speechDone.values()) {
      clearTimeout(p.timer);
      p.reject(new Error(reason));
    }
    this.speechDone.clear();
    this.transcribedChannels.clear();
    this.monitoredChannels.clear();
    this.channelScopes.clear();
    this.channelInfos.clear();
    this.channelPolicies.clear();
    this.transientBitrateBps = undefined;
    this.appliedSenderPrefs = undefined;
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
        this.refreshE2eeMode();
        reject(new Error(`join ${channelId} timed out`));
      }, this.opts.requestTimeoutMs);
      this.pendingJoins.set(channelId, { resolve, reject, timer });
      // Until the ack says whether the channel is encrypted, no plaintext may leave.
      this.refreshE2eeMode();
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
    this.forgetChannel(channelId);
  }

  /** Local bookkeeping of a channel we left (or were dropped from) on our own initiative. */
  private forgetChannel(channelId: string): void {
    this.dropDucking(channelId);
    this.typingSentAt.delete(channelId);
    this.transcribedChannels.delete(channelId);
    this.monitoredChannels.delete(channelId);
    this.channelScopes.delete(channelId);
    this.channelInfos.delete(channelId);
    this.forgetChannelSpatial(channelId);
    if (this.channels.delete(channelId)) this.emit('channelLeft', channelId);
    this.webTransportRosterChanged();
    if (this.channelPolicies.delete(channelId)) this.refreshAudioPolicy();
    this.e2eeChannelLeft(channelId);
    this.renderParticipants();
  }

  /** Whether the server transcribes `channelId` (speech-to-text is enabled for it). */
  isChannelTranscribed(channelId: string): boolean {
    return this.transcribedChannels.has(channelId);
  }

  /**
   * Whether speech in `channelId` is monitored by the operator's content-safety pipeline
   * (transcribed and classified server-side; `ChannelConfig.safety_voice`). Disclose this to
   * players — e.g. a "voice chat is moderated" badge.
   */
  isChannelMonitored(channelId: string): boolean {
    return this.monitoredChannels.has(channelId);
  }

  /**
   * Presence / text range of a joined positional channel (see {@link ChannelScope}); an empty
   * object for unscoped channels, `undefined` before the join is acknowledged.
   */
  channelScope(channelId: string): ChannelScope | undefined {
    return this.channelScopes.get(channelId);
  }

  /**
   * Role, participant count and audience flags of a joined channel (see {@link ChannelInfo});
   * `undefined` before the join is acknowledged.
   */
  channelInfo(channelId: string): ChannelInfo | undefined {
    return this.channelInfos.get(channelId);
  }

  /**
   * Whether we may transmit in `channelId`: `false` for `listener` grants (`speak: false`) and
   * for channels we have not joined. The server drops audio from listeners regardless.
   */
  canSpeakIn(channelId: string): boolean {
    const info = this.channelInfos.get(channelId);
    return info !== undefined && info.role !== 'listener';
  }

  /**
   * Whether we hold a speaking grant in `channelId` but wait for an `audience.max_speakers`
   * slot (see {@link ChannelInfo.waitingToSpeak}); `false` for channels we have not joined.
   */
  isWaitingToSpeak(channelId: string): boolean {
    return this.channelInfos.get(channelId)?.waitingToSpeak === true;
  }

  /** Whether this client currently receives `transcript` events (default `true`). */
  get transcriptsEnabled(): boolean {
    return this.wantTranscripts;
  }

  /**
   * Opt this client out of (or back into) transcript delivery. Client-held: survives
   * reconnects. Does not affect whether the channel is transcribed for others.
   */
  setTranscripts(enabled: boolean): void {
    this.wantTranscripts = enabled;
    this.trySend({ type: 'SetTranscripts', data: { enabled } });
  }

  /** Translation preference this client asked for (client-held; the server acks with `translationChanged`). */
  get translationPrefs(): TranslationPrefs {
    return { ...this.translation };
  }

  /**
   * Receive transcripts translated into `language` (BCP-47; `undefined` = originals only).
   * Segments already in that language arrive untranslated; translated ones carry
   * `Transcript.original`. Requires transcripts to be enabled and `SessionInfo.translation`.
   * Client-held: replayed on reconnect.
   */
  setTranslation(language: string | undefined, options: SetTranslationOptions = {}): void {
    const target = normalizeLanguageTag(language);
    const spoken = normalizeLanguageTag(options.spokenLanguage);
    this.translation = {
      ...(target !== undefined ? { language: target } : {}),
      ...(spoken !== undefined ? { spokenLanguage: spoken } : {}),
      speech: options.speech === true && target !== undefined,
    };
    this.trySend({ type: 'SetTranslation', data: this.translationWire() });
  }

  private translationWire(): { language?: string; spoken_language?: string; speech: boolean } {
    return {
      ...(this.translation.language !== undefined ? { language: this.translation.language } : {}),
      ...(this.translation.spokenLanguage !== undefined
        ? { spoken_language: this.translation.spokenLanguage }
        : {}),
      speech: this.translation.speech,
    };
  }
  /**
   * Have the server synthesize `text` and play it as this user's voice. Resolves once the
   * request is queued; `done` (and `ttsStatus` events) track playback. Rejects with
   * `<CODE>: <message>` on refusal (`FEATURE_DISABLED`, `AUTH_DENIED` not a member,
   * `USER_MUTED` when server-muted, `VALIDATION_ERROR` text too long / unknown voice,
   * `RATE_LIMIT_EXCEEDED`, `MESSAGE_BLOCKED` by the content filter).
   */
  speak(text: string, options: SpeakOptions = {}): Promise<SpeechRequest> {
    this.requireOpen();
    const clientRef = options.clientRef ?? `t${++this.speakRefCounter}-${Date.now().toString(36)}`;
    if (this.pendingSpeak.has(clientRef) || this.speechDone.has(clientRef)) {
      return Promise.reject(new Error(`clientRef ${clientRef} already pending`));
    }
    return new Promise<SpeechRequest>((resolve, reject) => {
      const timer = setTimeout(() => {
        this.pendingSpeak.delete(clientRef);
        reject(new Error('speak request timed out'));
      }, this.opts.requestTimeoutMs);
      this.pendingSpeak.set(clientRef, { resolve, reject, timer });
      this.send({
        type: 'TtsSpeak',
        data: {
          ...(options.channelId !== undefined ? { channel_id: options.channelId } : {}),
          text,
          ...(options.voice !== undefined ? { voice: options.voice } : {}),
          destination: options.destination ?? 'channel',
          client_ref: clientRef,
        },
      });
    });
  }

  /** Cancel every queued or playing `speak()` request of this session. */
  cancelSpeech(): void {
    this.trySend({ type: 'TtsCancel' });
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
   * Send a text message to one user of the same app. Neither side may have blocked the
   * other. When the server stores chat with offline delivery (`chat.persist` +
   * `chat.offline_delivery`), an offline recipient gets the message on their next connect
   * and the resolved echo carries `offline: true`; otherwise the target must currently have
   * a session (`USER_OFFLINE`).
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

  /**
   * One page of stored history (`chat.persist` on the server) of a joined channel or of the
   * direct conversation with a user, newest first. Page into the past with
   * `{ before: page.nextBefore }`, catch up after a gap with `{ after: lastSeen.cursor }`.
   * Rejects with `AUTH_DENIED` for a channel this client has not joined, `NOT_FOUND` when
   * the server does not store chat.
   */
  history(scope: ChatScope, options: HistoryOptions = {}): Promise<HistoryPage> {
    this.requireOpen();
    const clientRef = `h${++this.chatRefCounter}-${Date.now().toString(36)}`;
    return new Promise<HistoryPage>((resolve, reject) => {
      const timer = setTimeout(() => {
        this.pendingHistory.delete(clientRef);
        reject(new Error('history request timed out'));
      }, this.opts.requestTimeoutMs);
      this.pendingHistory.set(clientRef, { resolve, reject, timer });
      this.send({
        type: 'ChatHistory',
        data: {
          ...scopeWire(scope),
          ...(options.before !== undefined ? { before: options.before } : {}),
          ...(options.after !== undefined ? { after: options.after } : {}),
          ...(options.limit !== undefined ? { limit: options.limit } : {}),
          client_ref: clientRef,
        },
      });
    });
  }

  /**
   * Move this user's read marker in a conversation to `messageId` (the newest message the
   * user has seen). Idempotent and never moves backwards. Every device of the user — this
   * one included — receives the new position as `chatReadMarker`; with server-side read
   * receipts the other participants do too.
   */
  markRead(scope: ChatScope, messageId: string): void {
    this.requireOpen();
    this.send({ type: 'ChatMarkRead', data: { ...scopeWire(scope), message_id: messageId } });
  }

  /** Read markers and unread count of a joined channel or a direct conversation. */
  readMarkers(scope: ChatScope): Promise<ReadMarkers> {
    this.requireOpen();
    const key = scopeKey(scope);
    return new Promise<ReadMarkers>((resolve, reject) => {
      const pending: Pending<ReadMarkers> = {
        resolve,
        reject,
        timer: setTimeout(() => {
          const list = this.pendingReadMarkers.get(key);
          if (list) {
            const i = list.indexOf(pending);
            if (i >= 0) list.splice(i, 1);
            if (list.length === 0) this.pendingReadMarkers.delete(key);
          }
          reject(new Error('read markers request timed out'));
        }, this.opts.requestTimeoutMs),
      };
      const list = this.pendingReadMarkers.get(key);
      if (list) list.push(pending);
      else this.pendingReadMarkers.set(key, [pending]);
      this.send({ type: 'ChatReadMarkers', data: scopeWire(scope) });
    });
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

  /**
   * Replace the text (and optionally the metadata) of a message this user sent, within the
   * server's edit window (`chat.edit_window_secs`, 15 min by default). Resolves with the
   * updated copy (`editedAt` set), which every participant also receives as
   * `chatMessageUpdated`. Rejects with `AUTH_DENIED` (not the author / window over / edits
   * disabled), `NOT_FOUND`, `MESSAGE_BLOCKED`, `RATE_LIMIT_EXCEEDED` or `VALIDATION_ERROR`.
   */
  editMessage(messageId: string, text: string, options: EditMessageOptions = {}): Promise<ChatMessage> {
    return this.sendChat((clientRef) => ({
      type: 'ChatEdit',
      data: {
        message_id: messageId,
        text,
        ...(options.metadata !== undefined ? { metadata: options.metadata } : {}),
        client_ref: clientRef,
      },
    }));
  }

  /**
   * Delete a message: this user's own (within the edit window) or, as a moderator of the
   * channel it was sent in, anyone's. The message becomes a tombstone (same `id`, empty
   * `text`, `deletedAt` set) that every participant receives as `chatMessageUpdated` and
   * that stays in history in place. Deleting an already deleted message resolves with the
   * existing tombstone. Rejects with `AUTH_DENIED` or `NOT_FOUND`.
   */
  deleteMessage(messageId: string): Promise<ChatMessage> {
    return this.sendChat((clientRef) => ({
      type: 'ChatDelete',
      data: { message_id: messageId, client_ref: clientRef },
    }));
  }

  /**
   * Add (`add: true`) or remove this user's `reaction` (an emoji or a short token, ≤ 32
   * bytes) on a message this client can see. Idempotent: repeating a state is a silent
   * no-op; a change reaches everyone — this client included — as `chatReactionChanged`.
   * Refusals (`AUTH_DENIED`, `NOT_FOUND`, `RATE_LIMIT_EXCEEDED`, `VALIDATION_ERROR` — e.g.
   * the per-message cap on distinct reactions) arrive as `serverError`.
   */
  react(messageId: string, reaction: string, add = true): void {
    this.requireOpen();
    this.send({ type: 'ChatReact', data: { message_id: messageId, reaction, add } });
  }

  /**
   * Full-text search (`chat.search` on the server) in a joined channel or in the direct
   * conversation with a user, newest match first; deleted messages never match. Rate
   * limited per session (`chat.searches_per_minute`). Rejects with `AUTH_DENIED`,
   * `NOT_FOUND` (search / storage off), `RATE_LIMIT_EXCEEDED`, `VALIDATION_ERROR`.
   */
  search(scope: ChatScope, query: string, options: SearchOptions = {}): Promise<SearchPage> {
    this.requireOpen();
    const clientRef = `s${++this.chatRefCounter}-${Date.now().toString(36)}`;
    return new Promise<SearchPage>((resolve, reject) => {
      const timer = setTimeout(() => {
        this.pendingSearch.delete(clientRef);
        reject(new Error('search request timed out'));
      }, this.opts.requestTimeoutMs);
      this.pendingSearch.set(clientRef, { resolve, reject, timer });
      this.send({
        type: 'ChatSearch',
        data: {
          ...scopeWire(scope),
          query,
          ...(options.fromUserId !== undefined ? { from_user_id: options.fromUserId } : {}),
          ...(options.before !== undefined ? { before: options.before } : {}),
          ...(options.limit !== undefined ? { limit: options.limit } : {}),
          client_ref: clientRef,
        },
      });
    });
  }

  /** Mute/unmute the microphone locally and announce the state to channel members. */
  setMuted(muted: boolean): void {
    this.muted = muted;
    this.localStream?.getAudioTracks().forEach((t) => {
      t.enabled = !muted;
    });
    // The pipeline's output carries injected audio too: mute means nothing leaves the client.
    this.inputPipeline?.stream?.getAudioTracks().forEach((t) => {
      t.enabled = !muted;
    });
    this.wtCapture?.setPaused(muted);
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
    this.rememberPositions(channelId, [{ user_id: this.userId, position, orientation }]);
    this.renderParticipants();
  }

  respondToRecording(recordingId: string, consent: RecordingConsent): void {
    this.requireOpen();
    this.send({ type: 'RecordingConsentResponse', data: { recording_id: recordingId, consent } });
  }

  /**
   * Sample the peer connection (`RTCPeerConnection.getStats()`) and return a normalized
   * snapshot: cumulative packet/byte counters, jitter, RTT min/avg/max, concealment and
   * discards, downlink loss over the period since the previous call, and the derived
   * R-factor / MOS / 1–5 bars. Without media the snapshot carries only RTT and the last
   * server-side quality.
   */
  async getStats(): Promise<ClientStats> {
    const snapshot = await this.sampleStats();
    this.statsSnapshot = snapshot;
    return snapshot;
  }

  /**
   * Send the current receiver statistics to the server as a `QualityReport` so it can adapt
   * the downlink bitrate and compute `networkQuality`. Runs automatically every
   * `qualityReportIntervalMs`; call it directly for an out-of-band report.
   */
  async reportQuality(): Promise<void> {
    if (!(this.pc || this.wt) || !this.ws || this.ws.readyState !== WebSocket.OPEN) return;
    const stats = await this.getStats();
    this.emit('stats', stats);
    if (!this.ws || this.ws.readyState !== WebSocket.OPEN) return;
    this.send({
      type: 'QualityReport',
      data: { rtt_ms: stats.rttMs, jitter_ms: stats.jitterMs, packet_loss: stats.lossPercent },
    });
  }

  private async sampleStats(): Promise<ClientStats> {
    if (this.wt) return this.webTransportStats(this.wt);
    const input: RtcStatsInput = {};
    const pc = this.pc;
    if (pc) {
      let report: RTCStatsReport | undefined;
      try {
        report = await pc.getStats();
      } catch {
        report = undefined;
      }
      let nominatedRtt: number | undefined;
      let succeededRtt: number | undefined;
      report?.forEach((s) => {
        const entry = s as RTCStats & { kind?: string };
        if (entry.kind !== undefined && entry.kind !== 'audio') return;
        switch (entry.type) {
          case 'inbound-rtp':
            input.inbound = mergeInboundStats(input.inbound, s as NonNullable<RtcStatsInput['inbound']>);
            break;
          case 'outbound-rtp':
            input.outbound = s as NonNullable<RtcStatsInput['outbound']>;
            break;
          case 'remote-inbound-rtp':
            input.remoteInbound = s as NonNullable<RtcStatsInput['remoteInbound']>;
            break;
          case 'candidate-pair': {
            const pair = s as RTCIceCandidatePairStats & { nominated?: boolean };
            if (pair.state !== 'succeeded' || pair.currentRoundTripTime === undefined) break;
            if (pair.nominated) nominatedRtt = pair.currentRoundTripTime;
            else succeededRtt ??= pair.currentRoundTripTime;
            break;
          }
        }
      });
      const iceRtt = nominatedRtt ?? succeededRtt;
      if (iceRtt !== undefined) input.iceRttSeconds = iceRtt;
    }
    const out = assembleClientStats(this.rtt, input, this.lossWindow, this.serverQuality);
    if (pc) out.transport = 'webrtc';
    return out;
  }

  // ── Internals: control channel ──

  private openControlChannel(url: string): Promise<SessionInfo> {
    return new Promise<SessionInfo>((resolve, reject) => {
      // Browsers cannot set the Authorization header on a WebSocket upgrade; the server
      // accepts the JWT as a `bearer.<jwt>` sub-protocol and echoes `aurix` back. The
      // resume credential travels the same way.
      const protocols = [AURIX_SUBPROTOCOL, `${BEARER_SUBPROTOCOL_PREFIX}${this.opts.token}`];
      if (this.session && this.resumeToken) {
        protocols.push(`${RESUME_SUBPROTOCOL_PREFIX}${this.session.sessionId}.${this.resumeToken}`);
      }
      const ws = new WebSocket(url, protocols);
      this.ws = ws;
      const timer = setTimeout(() => {
        this.pendingInit = undefined;
        ws.close();
        reject(new Error('session init timed out'));
      }, this.opts.requestTimeoutMs);
      this.pendingInit = { resolve, reject, timer, url };

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
    const url = reconnectEndpoint(this.activeEndpoint, this.failoverEndpoints, this.reconnectAttempt);
    let info: SessionInfo;
    try {
      if (this.opts.refreshToken) {
        this.opts.token = await this.opts.refreshToken();
        if (this.closedByUser) return;
      }
      info = await this.openControlChannel(url);
    } catch (e) {
      if (this.closedByUser) return;
      this.scheduleReconnect(e instanceof Error ? e.message : String(e));
      return;
    }
    if (this.closedByUser) return;

    if (url !== this.activeEndpoint) {
      this.activeEndpoint = url;
      this.emit('endpointChanged', url);
    }
    this.session = info;
    this.reconnectAttempt = 0;
    this.startPing();
    if (!info.resumed) {
      // Fresh session: the server forgot our channels, so the roster below is stale.
      for (const channelId of wanted) {
        if (this.channels.delete(channelId)) this.emit('channelLeft', channelId);
      }
      this.dropAllDucking();
      this.e2eeSessionEnded();
      this.channelPolicies.clear();
      this.channelInfos.clear();
      this.transientBitrateBps = undefined;
      this.replayReceiverPrefs();
    }
    this.setState('connected');
    try {
      // A takeover rebuilt the media session on another node: the old peer connection may
      // still look connected for a while but nobody is listening at the other end.
      await this.restoreMedia(info.resumed && !info.migrated && previous?.ssrc === info.ssrc);
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
    if (sameSession && ((this.pc && this.pc.connectionState === 'connected') || this.wt?.isOpen)) {
      this.setState('media-connected');
      return;
    }
    this.pc?.close();
    this.pc = undefined;
    this.clearParticipantTracks();
    this.stopWebTransportMedia();
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
    const base: MediaTrackConstraints = this.opts.audioConstraints ?? defaultAudioConstraints(this.opts.opus);
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
      if (this.localStream !== stream || (!this.pc && !this.wt) || stream === this.opts.localStream) return;
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
      this.wtCapture?.setStream(stream);
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
    this.syncMicrophoneWorklets();
    const processed = pipeline.stream;
    const track = processed?.getAudioTracks()[0];
    if (track) track.enabled = !this.muted;
    const sender = this.pc?.getSenders().find((s) => s.track?.kind === 'audio' || s.track === null);
    if (track && sender) {
      void sender.replaceTrack(track).catch((e: unknown) => {
        this.emit('error', e instanceof Error ? e : new Error(String(e)));
      });
    }
    if (processed) {
      this.wtCapture?.setStream(processed);
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
        const endpoint = this.pendingInit?.url ?? this.activeEndpoint;
        this.failoverEndpoints = (d.failover ?? []).filter((u) => u !== endpoint);
        this.mediaKey = d.media_key ? base64ToBytes(d.media_key) : undefined;
        this.webTransportInfo = webTransportAdvertised(d.webtransport) ? d.webtransport : undefined;
        const info: SessionInfo = {
          sessionId: d.session_id,
          userId: this.userId ?? '',
          ssrc: d.ssrc,
          resumed: d.resumed === true,
          migrated: d.migrated === true,
          endpoint,
          failover: [...this.failoverEndpoints],
          participantStreamCap: d.webrtc_participant_streams ?? 0,
          ...(this.webTransportInfo
            ? {
                webTransport: {
                  urls: [...this.webTransportInfo.urls],
                  certSha256: [...(this.webTransportInfo.cert_sha256 ?? [])],
                },
              }
            : {}),
          ...(d.translation
            ? {
                translation: {
                  speech: d.translation.speech === true,
                  languages: [...(d.translation.languages ?? [])],
                },
              }
            : {}),
        };
        this.resumeToken = d.resume_token || undefined;
        this.resumeGraceMs = d.resume_grace_ms ?? 0;
        this.participantStreamCapValue = d.webrtc_participant_streams ?? 0;
        this.unfocusedGain = d.unfocused_channel_gain ?? DEFAULT_UNFOCUSED_GAIN;
        if (this.pinnedParticipants.length > this.participantStreamCapValue) {
          this.pinnedParticipants = this.pinnedParticipants.slice(0, this.participantStreamCapValue);
        }
        // E2EE that only WebTransport can carry (no encoded-frame API) is announced only when
        // that is the path we are going to take.
        if (this.e2eeGroup && this.e2eeApi === undefined && !this.webTransportPlanned()) this.dropE2ee('WebTransport is not available here');
        const pending = this.pendingInit;
        this.pendingInit = undefined;
        if (pending) {
          clearTimeout(pending.timer);
          pending.resolve(info);
        }
        this.sendE2eeHello(undefined);
        return;
      }
      case 'ChannelJoinAck': {
        const d = (msg as Extract<ServerMessage, { type: 'ChannelJoinAck' }>).data;
        const encrypted = d.audio?.e2ee === true;
        if (encrypted && !this.e2eeGroup) {
          // The server should have refused us (`E2EE_REQUIRED`); never sit in an encrypted
          // channel we cannot take part in.
          this.trySend({ type: 'ChannelLeave', data: { channel_id: d.channel_id } });
          const pending = this.pendingJoins.get(d.channel_id);
          const err = new Error(`E2EE_REQUIRED: channel ${d.channel_id} is end-to-end encrypted and this client has no E2EE`);
          if (pending) {
            clearTimeout(pending.timer);
            this.pendingJoins.delete(d.channel_id);
            pending.reject(err);
          }
          this.refreshE2eeMode();
          this.emit('error', err);
          return;
        }
        const replayed = this.channels.has(d.channel_id) && !this.pendingJoins.has(d.channel_id);
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
            priority: p.is_priority === true,
          });
        }
        if (!this.userId && this.session) {
          const me = d.participants.find((p) => p.ssrc === this.session?.ssrc);
          if (me) this.userId = me.user_id;
        }
        this.channels.set(d.channel_id, roster);
        this.webTransportRosterChanged();
        if (d.transcription) this.transcribedChannels.add(d.channel_id);
        else this.transcribedChannels.delete(d.channel_id);
        if (d.safety_voice) this.monitoredChannels.add(d.channel_id);
        else this.monitoredChannels.delete(d.channel_id);
        const scope: ChannelScope = {};
        if (typeof d.roster_radius === 'number') scope.rosterRadius = d.roster_radius;
        if (typeof d.text_radius === 'number') scope.textRadius = d.text_radius;
        this.channelScopes.set(d.channel_id, scope);
        this.channelInfos.set(d.channel_id, channelInfoFromJoinAck(d));
        this.channelPolicies.set(d.channel_id, parseAudioPolicy(d.audio));
        this.refreshAudioPolicy();
        if (d.positional) this.channelPositional.set(d.channel_id, d.positional);
        else this.forgetChannelSpatial(d.channel_id);
        this.renderParticipants();
        this.refreshDucking(d.channel_id);
        if (encrypted) this.e2eeChannelJoined(d.channel_id, replayed, new Set(roster.keys()));
        else this.e2eeChannelLeft(d.channel_id);
        const list = Array.from(roster.values());
        const pending = this.pendingJoins.get(d.channel_id);
        if (pending) {
          clearTimeout(pending.timer);
          this.pendingJoins.delete(d.channel_id);
          pending.resolve(list);
        }
        this.refreshE2eeMode();
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
          role: d.role ?? 'unknown',
          muted: d.is_muted === true,
          serverMuted: false,
          speaking: false,
          energy: 0,
          priority: d.is_priority === true,
        };
        roster.set(d.user_id, p);
        this.webTransportRosterChanged();
        this.renderParticipant(d.user_id);
        this.emit('participantJoined', d.channel_id, p);
        return;
      }
      case 'ParticipantLeft': {
        const d = (msg as Extract<ServerMessage, { type: 'ParticipantLeft' }>).data;
        this.channels.get(d.channel_id)?.delete(d.user_id);
        this.positions.get(d.channel_id)?.delete(d.user_id);
        this.webTransportRosterChanged();
        this.renderParticipant(d.user_id);
        this.e2eePeerLeft(d.channel_id, d.user_id);
        this.emit('participantLeft', d.channel_id, d.user_id);
        this.refreshDucking(d.channel_id);
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
        if (p && this.isPriorityMember(d.channel_id, p)) {
          this.refreshDucking(d.channel_id);
          if (d.user_id === this.userId) this.renderParticipants();
        }
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
        this.rememberPositions(d.channel_id, d.positions);
        for (const p of d.positions) this.renderParticipant(p.user_id);
        this.emit('positions', d.channel_id, d.positions);
        return;
      }
      case 'RecordingNotification': {
        const d = (msg as Extract<ServerMessage, { type: 'RecordingNotification' }>).data;
        this.emit('recording', d.channel_id, d.recording_id, d.active, d.initiated_by, d.live === true);
        return;
      }
      case 'BitrateCommand': {
        const d = (msg as Extract<ServerMessage, { type: 'BitrateCommand' }>).data;
        this.transientBitrateBps = d.target_bitrate_kbps * 1000;
        void this.applySenderPreferences();
        this.emit('bitrate', d.target_bitrate_kbps, d.reason, d.expected_loss_percent ?? 0);
        return;
      }
      case 'ChannelAudioPolicy': {
        const d = (msg as Extract<ServerMessage, { type: 'ChannelAudioPolicy' }>).data;
        if (!this.channels.has(d.channel_id)) return;
        this.channelPolicies.set(d.channel_id, parseAudioPolicy(d.audio));
        this.refreshAudioPolicy();
        if ('ducking' in d) {
          const info = this.channelInfos.get(d.channel_id);
          if (info) {
            const ducking = parseDucking(d.ducking);
            if (ducking) info.ducking = ducking;
            else delete info.ducking;
            this.renderParticipants();
            this.refreshDucking(d.channel_id);
          }
        }
        return;
      }
      case 'RoleChanged': {
        const d = (msg as Extract<ServerMessage, { type: 'RoleChanged' }>).data;
        if (!this.channels.has(d.channel_id)) return;
        const p = this.channels.get(d.channel_id)?.get(d.user_id);
        if (p) p.role = d.role;
        if (d.user_id === this.userId) {
          const info = this.channelInfos.get(d.channel_id);
          if (info) {
            info.role = d.role;
            info.waitingToSpeak = d.role === 'listener';
          }
        }
        this.renderParticipants();
        this.emit(
          'participantRoleChanged',
          d.channel_id,
          d.user_id,
          d.role,
          d.reason === 'speaker_admitted',
        );
        return;
      }
      case 'PriorityChanged': {
        const d = (msg as Extract<ServerMessage, { type: 'PriorityChanged' }>).data;
        if (!this.channels.has(d.channel_id)) return;
        const p = this.channels.get(d.channel_id)?.get(d.user_id);
        if (p) p.priority = d.priority;
        if (d.user_id === this.userId) {
          const info = this.channelInfos.get(d.channel_id);
          if (info) info.priority = d.priority;
        }
        this.renderParticipants();
        this.emit('participantPriorityChanged', d.channel_id, d.user_id, d.priority);
        this.refreshDucking(d.channel_id);
        return;
      }
      case 'Kick': {
        const d = (msg as Extract<ServerMessage, { type: 'Kick' }>).data;
        this.dropDucking(d.channel_id);
        if (this.channels.delete(d.channel_id)) this.emit('channelLeft', d.channel_id);
        this.channelInfos.delete(d.channel_id);
        this.channelScopes.delete(d.channel_id);
        this.forgetChannelSpatial(d.channel_id);
        if (this.channelPolicies.delete(d.channel_id)) this.refreshAudioPolicy();
        this.e2eeChannelLeft(d.channel_id);
        this.renderParticipants();
        this.emit('kicked', d.channel_id, d.reason);
        return;
      }
      case 'UserBlockChanged': {
        const d = (msg as Extract<ServerMessage, { type: 'UserBlockChanged' }>).data;
        if (d.blocked) this.blockedUsers.add(d.user_id);
        else this.blockedUsers.delete(d.user_id);
        this.renderParticipant(d.user_id);
        this.emit('userBlockChanged', d.user_id, d.blocked);
        return;
      }
      case 'TransmissionChanged': {
        const d = (msg as Extract<ServerMessage, { type: 'TransmissionChanged' }>).data;
        this.transmission = transmissionFromWire(d.mode);
        this.emit('transmissionChanged', this.transmission);
        return;
      }
      case 'TranslationChanged': {
        const d = (msg as Extract<ServerMessage, { type: 'TranslationChanged' }>).data;
        this.translation = {
          ...(d.language ? { language: d.language } : {}),
          ...(d.spoken_language ? { spokenLanguage: d.spoken_language } : {}),
          speech: d.speech === true,
        };
        this.emit('translationChanged', { ...this.translation });
        return;
      }
      case 'ChannelFocusChanged': {
        const d = (msg as Extract<ServerMessage, { type: 'ChannelFocusChanged' }>).data;
        this.focusChannel = d.channel_id ?? undefined;
        this.renderParticipants();
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
        this.renderParticipants();
        this.emit('receiverPreferences', {
          blockedUsers: d.blocked_users,
          localMutes: d.local_mutes,
          volumes: d.volumes,
          transmission,
          focusChannel,
        });
        return;
      }
      case 'ParticipantStreams': {
        const d = (msg as Extract<ServerMessage, { type: 'ParticipantStreams' }>).data;
        this.applyParticipantLayout(d.streams);
        return;
      }
      case 'E2eeHello': {
        this.onE2eeHello((msg as Extract<ServerMessage, { type: 'E2eeHello' }>).data);
        return;
      }
      case 'E2eeSenderKey': {
        this.onE2eeSenderKey((msg as Extract<ServerMessage, { type: 'E2eeSenderKey' }>).data);
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
          this.rtt.record(this.lastPongAt - this.lastPingSentAt);
        }
        return;
      }
      case 'NetworkQuality': {
        const d = (msg as Extract<ServerMessage, { type: 'NetworkQuality' }>).data;
        const quality = networkQualityFromWire(d.quality);
        this.serverQuality = quality;
        this.emit('networkQuality', quality);
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
          const history = this.pendingHistory.get(d.client_ref);
          if (history) {
            clearTimeout(history.timer);
            this.pendingHistory.delete(d.client_ref);
            history.reject(new Error(`${d.code}: ${d.message}`));
          }
          const search = this.pendingSearch.get(d.client_ref);
          if (search) {
            clearTimeout(search.timer);
            this.pendingSearch.delete(d.client_ref);
            search.reject(new Error(`${d.code}: ${d.message}`));
          }
          const speak = this.pendingSpeak.get(d.client_ref);
          if (speak) {
            clearTimeout(speak.timer);
            this.pendingSpeak.delete(d.client_ref);
            speak.reject(new Error(`${d.code}: ${d.message}`));
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
          this.refreshE2eeMode();
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
      case 'ChatMessageUpdated': {
        const d = (msg as Extract<ServerMessage, { type: 'ChatMessageUpdated' }>).data;
        const message = this.toChatMessage(d.message);
        if (message.clientRef !== undefined) {
          const pending = this.pendingChat.get(message.clientRef);
          if (pending) {
            clearTimeout(pending.timer);
            this.pendingChat.delete(message.clientRef);
            pending.resolve(message);
          }
        }
        this.emit('chatMessageUpdated', message);
        return;
      }
      case 'ChatReactionChanged': {
        const d = (msg as Extract<ServerMessage, { type: 'ChatReactionChanged' }>).data;
        const change: ChatReactionChange = {
          messageId: d.message_id,
          messageFromUserId: d.message_from_user_id,
          userId: d.user_id,
          reaction: d.reaction,
          added: d.added,
          count: d.count,
          timestamp: new Date(d.timestamp),
        };
        if (d.channel_id != null) change.channelId = d.channel_id;
        if (d.message_to_user_id != null) change.messageToUserId = d.message_to_user_id;
        this.emit('chatReactionChanged', change);
        return;
      }
      case 'ChatSearchResult': {
        const d = (msg as Extract<ServerMessage, { type: 'ChatSearchResult' }>).data;
        const pending = d.client_ref != null ? this.pendingSearch.get(d.client_ref) : undefined;
        if (!pending || d.client_ref == null) return;
        clearTimeout(pending.timer);
        this.pendingSearch.delete(d.client_ref);
        const page: SearchPage = { query: d.query, messages: d.messages.map((m) => this.toChatMessage(m)) };
        if (d.next_before != null) page.nextBefore = d.next_before;
        pending.resolve(page);
        return;
      }
      case 'ChatHistoryResult': {
        const d = (msg as Extract<ServerMessage, { type: 'ChatHistoryResult' }>).data;
        const pending = d.client_ref != null ? this.pendingHistory.get(d.client_ref) : undefined;
        if (!pending || d.client_ref == null) return;
        clearTimeout(pending.timer);
        this.pendingHistory.delete(d.client_ref);
        const page: HistoryPage = { messages: d.messages.map((m) => this.toChatMessage(m)) };
        if (d.next_before != null) page.nextBefore = d.next_before;
        if (d.next_after != null) page.nextAfter = d.next_after;
        pending.resolve(page);
        return;
      }
      case 'ChatReadMarker': {
        const d = (msg as Extract<ServerMessage, { type: 'ChatReadMarker' }>).data;
        this.emit('chatReadMarker', readMarkerFromWire(d.marker));
        return;
      }
      case 'ChatReadMarkersResult': {
        const d = (msg as Extract<ServerMessage, { type: 'ChatReadMarkersResult' }>).data;
        const key = d.channel_id != null ? `c:${d.channel_id}` : `u:${d.user_id ?? ''}`;
        const list = this.pendingReadMarkers.get(key);
        if (!list || list.length === 0) return;
        const pending = list.shift()!;
        if (list.length === 0) this.pendingReadMarkers.delete(key);
        clearTimeout(pending.timer);
        pending.resolve({ markers: d.markers.map(readMarkerFromWire), unreadCount: d.unread_count });
        return;
      }
      case 'ChatInboxSynced': {
        const d = (msg as Extract<ServerMessage, { type: 'ChatInboxSynced' }>).data;
        this.emit('chatInboxSynced', d.delivered, d.truncated);
        return;
      }
      case 'ParticipantTyping': {
        const d = (msg as Extract<ServerMessage, { type: 'ParticipantTyping' }>).data;
        this.emit('participantTyping', d.channel_id, d.user_id, d.typing);
        return;
      }
      case 'Transcript': {
        const d = (msg as Extract<ServerMessage, { type: 'Transcript' }>).data;
        this.emit('transcript', transcriptFromWire(d.transcript));
        return;
      }
      case 'TtsStatus': {
        const d = (msg as Extract<ServerMessage, { type: 'TtsStatus' }>).data;
        const status: TtsStatus = {
          requestId: d.request_id,
          ...(d.client_ref !== undefined ? { clientRef: d.client_ref } : {}),
          state: d.state,
          ...(d.duration_ms !== undefined ? { durationMs: d.duration_ms } : {}),
          ...(d.message !== undefined ? { message: d.message } : {}),
        };
        if (d.client_ref !== undefined) {
          const pending = this.pendingSpeak.get(d.client_ref);
          if (pending) {
            clearTimeout(pending.timer);
            this.pendingSpeak.delete(d.client_ref);
            const clientRef = d.client_ref;
            const done = new Promise<TtsStatus>((resolve, reject) => {
              this.speechDone.set(clientRef, { resolve, reject, timer: undefined });
            });
            // Nobody is obliged to await `done`.
            done.catch(() => undefined);
            pending.resolve({ requestId: d.request_id, clientRef, done });
          }
          if (d.state === 'finished' || d.state === 'cancelled' || d.state === 'failed') {
            const done = this.speechDone.get(d.client_ref);
            if (done) {
              this.speechDone.delete(d.client_ref);
              done.resolve(status);
            }
          }
        }
        this.emit('ttsStatus', status);
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
    const sent = await this.openLocalMedia();
    const preference = this.opts.transport ?? 'auto';
    if (preference !== 'webrtc') {
      const blocker = this.webTransportBlocker();
      if (blocker === undefined) {
        try {
          await this.startWebTransportMedia(sent);
          return;
        } catch (e) {
          this.stopWebTransportMedia();
          if (this.closedByUser || !this.ws) throw e;
          const err = e instanceof Error ? e : new Error(String(e));
          if (preference === 'webtransport') throw err;
          this.emit('error', new Error(`WebTransport media failed, falling back to WebRTC: ${err.message}`));
        }
      } else if (preference === 'webtransport') {
        throw new Error(`WebTransport unavailable: ${blocker}`);
      }
    }
    if (this.e2eeGroup && this.e2eeApi === undefined) this.dropE2ee('no encoded-frame API for WebRTC');
    await this.startWebRtcMedia(sent);
  }

  /** Microphone, gain pipeline, meter and device watching — shared by both media paths. */
  private async openLocalMedia(): Promise<MediaStream> {
    if (!this.localStream) {
      this.localStream = this.opts.localStream ?? (await this.openMicrophone());
      this.watchInputTrack(this.localStream);
    }
    this.localStream.getAudioTracks().forEach((t) => {
      t.enabled = !this.muted;
    });
    if (this.needsInputPipeline() && !this.inputPipeline) {
      const pipeline = new InputPipeline();
      if (pipeline.open(this.localStream, this.inputGainValue)) this.inputPipeline = pipeline;
      else this.emit('error', new Error('Web Audio is unavailable: input gain / voice effects / visemes ignored'));
    }
    this.syncMicrophoneWorklets();
    const sent = this.sentStream() ?? this.localStream;
    this.startLocalMeter(sent);
    if (typeof navigator !== 'undefined') {
      navigator.mediaDevices?.addEventListener?.('devicechange', this.onDeviceChange);
    }
    return sent;
  }

  private async startWebRtcMedia(sent: MediaStream): Promise<void> {
    const iceServers: RTCIceServer[] = [...(this.opts.iceServers ?? [])];
    if (this.opts.useTurn) {
      const turn = await this.fetchTurnCredentials();
      if (turn) {
        iceServers.push({ urls: turn.uris, username: turn.username, credential: turn.password });
      }
    }

    const pcConfig: RTCConfiguration & { encodedInsertableStreams?: boolean } = {
      iceServers,
      bundlePolicy: 'max-bundle',
      rtcpMuxPolicy: 'require',
    };
    if (this.e2eeGroup && this.e2eeApi === 'streams') pcConfig.encodedInsertableStreams = true;
    const pc = new RTCPeerConnection(pcConfig);
    this.pc = pc;
    this.mediaTransportValue = 'webrtc';
    this.lossWindow.reset();
    this.startQualityTimer();
    // One sendrecv audio transceiver: uplink microphone, downlink server-side mix. Then up to
    // `participantStreams` recvonly ones, each carrying one participant of the server's choice
    // (`ParticipantStreams` says who); the server answers them all, uses at most its cap.
    const track = sent.getAudioTracks()[0];
    if (!track) throw new Error('no audio track');
    const mixed = pc.addTransceiver(track, { direction: 'sendrecv', streams: [sent] });
    this.mixedTransceiver = mixed;
    const extra = this.participantStreamsToOffer();
    for (let i = 0; i < extra; i++) pc.addTransceiver('audio', { direction: 'recvonly' });

    pc.ontrack = (ev) => {
      if (this.pc !== pc) return;
      const stream = ev.streams[0] ?? new MediaStream([ev.track]);
      const mid = ev.transceiver?.mid ?? null;
      if (ev.transceiver !== mixed && mid !== null && mid !== mixed.mid) {
        this.onParticipantTrack(mid, stream);
        return;
      }
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
            this.emit('mediaTransport', 'webrtc');
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
    // Mids exist now and no media flows before the answer: hook the encrypted-frame path.
    if (this.e2eeGroup) this.attachE2eeTransforms(pc, mixed);
    // The server is ICE-lite and answers with its own host candidate, so no trickle needed:
    // wait for local gathering to finish so the offer carries our candidates.
    await this.waitForIceGathering(pc);
    const localSdp = pc.localDescription?.sdp;
    if (!localSdp) throw new Error('missing local description');

    const answerSdp = await this.requestAnswer(localSdp);
    // RFC 7587: the fmtp of the description we *receive* states what the browser should send.
    await pc.setRemoteDescription({
      type: 'answer',
      sdp: applyOpusSenderPreferences(answerSdp, this.opusPreferences),
    });
    this.appliedSenderPrefs = undefined;
    await this.applySenderPreferences(pc);
    if (this.pc === pc && extra > 0 && this.pinnedParticipants.length > 0) {
      this.trySend({ type: 'SetParticipantStreams', data: { pinned: [...this.pinnedParticipants] } });
    }
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

  /** Recompute the merged policy after a join/leave/policy update; apply and announce on change. */
  private refreshAudioPolicy(): void {
    if (this.channelPolicies.size === 0) return;
    const merged = mergeAllAudioPolicies(this.channelPolicies.values());
    if (audioPoliciesEqual(this.audioPolicyValue, merged)) return;
    this.audioPolicyValue = merged;
    this.transientBitrateBps = undefined;
    void this.applySenderPreferences();
    this.reconfigureWebTransportCapture();
    this.emit('audioPolicy', merged);
  }

  // ── Internals: AURX over WebTransport ──

  /** Why AURX over WebTransport cannot be used in this browser at all (`undefined` = it can). */
  private webTransportPossible(): boolean {
    return this.opts.transport !== 'webrtc' && webTransportCapabilityBlocker() === undefined;
  }

  /** Why this session cannot use WebTransport right now (`undefined` = it can). */
  private webTransportBlocker(): string | undefined {
    if (!this.webTransportInfo) return 'the node does not offer WebTransport for this session';
    if (!this.mediaKey || this.mediaKey.length !== AURX_MEDIA_KEY_BYTES) return 'no session media key';
    return webTransportCapabilityBlocker();
  }

  private webTransportPlanned(): boolean {
    return this.opts.transport !== 'webrtc' && this.webTransportBlocker() === undefined;
  }

  private async startWebTransportMedia(sent: MediaStream): Promise<void> {
    const info = this.webTransportInfo;
    const session = this.session;
    const key = this.mediaKey;
    if (!info || !session || !key) throw new Error('WebTransport is not offered for this session');
    const renderer = this.ensureRenderer();
    const ctx = renderer ? workletContext(renderer.context) : undefined;
    if (!renderer || !ctx) throw new Error('Web Audio is unavailable');

    const playback = new AurxPlayback(ctx, (e) => this.emit('error', e));
    this.wtPlayback = playback;
    await playback.init();
    if (this.wtPlayback !== playback) throw new Error('media torn down');

    const o = this.opts.webTransport ?? {};
    const options: AurxWebTransportOptions = {};
    if (o.connectTimeoutMs !== undefined) options.connectTimeoutMs = o.connectTimeoutMs;
    if (o.heartbeatIntervalMs !== undefined) options.heartbeatIntervalMs = o.heartbeatIntervalMs;
    if (o.heartbeatLossLimit !== undefined) options.heartbeatLossLimit = o.heartbeatLossLimit;
    if (this.wtNextSequence !== undefined) options.startSequence = this.wtNextSequence;
    const transport = new AurxWebTransport(
      { sessionId: session.sessionId, ssrc: session.ssrc, masterKey: key },
      {
        onAudio: (audio) => {
          if (this.wt === transport) this.onWebTransportAudio(audio);
        },
        onBitrate: (bps) => {
          if (this.wt === transport) this.onWebTransportBitrate(bps);
        },
        onClosed: (reason) => {
          if (this.wt === transport) this.onWebTransportClosed(reason);
        },
        onError: (e) => {
          if (this.wt === transport) this.emit('error', e);
        },
      },
      options,
    );
    this.wt = transport;
    await transport.connect(info);
    if (this.wt !== transport) throw new Error('media torn down');

    const capture = new AurxCapture(
      (frame) => {
        if (this.wtCapture === capture) this.onCaptureFrame(frame);
      },
      (e) => {
        if (this.wtCapture === capture) this.emit('error', e);
      },
    );
    this.wtCapture = capture;
    capture.setPaused(this.muted);
    await capture.start(sent, this.webTransportOpusConfig());
    if (this.wt !== transport || this.wtCapture !== capture) throw new Error('media torn down');

    this.mediaTransportValue = 'webtransport';
    this.lossWindow.reset();
    this.startQualityTimer();
    this.wtIdleTimer = setInterval(() => this.sweepWebTransportSlots(), 1_000);
    if (this.state === 'connected' || this.state === 'media-connecting') this.setState('media-connected');
    this.emit('mediaTransport', 'webtransport');
  }

  private stopWebTransportMedia(): void {
    const transport = this.wt;
    this.wt = undefined;
    if (transport) {
      this.wtNextSequence = transport.nextSequence;
      transport.close();
    }
    const capture = this.wtCapture;
    this.wtCapture = undefined;
    capture?.stop();
    if (this.wtIdleTimer !== undefined) clearInterval(this.wtIdleTimer);
    this.wtIdleTimer = undefined;
    const hadSlots = this.wtSlots.size > 0;
    for (const ssrc of Array.from(this.wtSlots.keys())) this.removeWebTransportSlot(ssrc, false);
    if (hadSlots) this.emitParticipantStreams();
    const playback = this.wtPlayback;
    this.wtPlayback = undefined;
    playback?.clear();
    this.wtUserBySsrc.clear();
    this.wtDecryptQueue = Promise.resolve();
    if (this.mediaTransportValue === 'webtransport') this.mediaTransportValue = undefined;
  }

  /** Encoder settings: channel policy → `opus` browser options → `webTransport.opus` overrides. */
  private webTransportOpusConfig(): AurxOpusConfig {
    const prefs = this.opusPreferences;
    const overrides: Partial<AurxOpusConfig> = {};
    if (prefs.maxBitrateBps !== undefined) overrides.bitrateBps = prefs.maxBitrateBps;
    if (prefs.fec !== undefined) overrides.fec = prefs.fec;
    if (prefs.dtx !== undefined) overrides.dtx = prefs.dtx;
    if (prefs.cbr !== undefined) overrides.cbr = prefs.cbr;
    overrides.channels = prefs.stereo === true ? 2 : 1;
    return opusConfigFor(this.audioPolicyValue ?? DEFAULT_AUDIO_POLICY, {
      ...overrides,
      ...(this.opts.webTransport?.opus ?? {}),
    });
  }

  private reconfigureWebTransportCapture(): void {
    this.wtCapture?.reconfigure(this.webTransportOpusConfig());
  }

  private onWebTransportBitrate(bps: number): void {
    this.transientBitrateBps = bps;
    this.reconfigureWebTransportCapture();
    this.emit('bitrate', Math.round(bps / 1000), 'server', 0);
  }

  private onWebTransportClosed(reason: string): void {
    if (this.closedByUser || !this.ws) return;
    this.emit('error', new Error(`WebTransport media closed: ${reason}`));
    this.pc?.close();
    this.pc = undefined;
    this.stopWebTransportMedia();
    if (this.state === 'media-connected' || this.state === 'media-connecting') this.setState('connected');
    void this.startMedia().catch((e: unknown) => {
      this.emit('error', e instanceof Error ? e : new Error(String(e)));
    });
  }

  /** The channels a captured frame goes to, split by whether they carry E2EE frames. */
  private captureTargets(): { plain: number[]; encrypted: number[] } {
    const plain: number[] = [];
    const encrypted: number[] = [];
    const mode = this.transmission;
    if (mode.type === 'none') return { plain, encrypted };
    for (const channelId of this.channels.keys()) {
      if (mode.type === 'single' && mode.channelId !== channelId) continue;
      if (this.channelInfos.get(channelId)?.role === 'listener') continue;
      const hash = this.channelHash(channelId);
      if (hash === undefined) continue;
      if (this.e2eeChannels.has(channelId)) encrypted.push(hash);
      else plain.push(hash);
    }
    return { plain, encrypted };
  }

  private channelHash(channelId: string): number | undefined {
    for (const [hash, id] of this.wtChannelByHash) if (id === channelId) return hash;
    let hash: number;
    try {
      hash = channelIdHash(channelId);
    } catch {
      return undefined;
    }
    this.wtChannelByHash.set(hash, channelId);
    return hash;
  }

  private onCaptureFrame(frame: AurxCaptureFrame): void {
    const wt = this.wt;
    if (!wt?.isOpen || frame.opus.length === 0) return;
    const { plain, encrypted } = this.captureTargets();
    for (const hash of plain) wt.sendAudio(frame.opus, hash, frame.timestamp, { energy: frame.energy });
    if (encrypted.length === 0) return;
    const group = this.e2eeGroup;
    if (!group) return;
    void group
      .encrypt(frame.opus)
      .then((sealed) => {
        if (this.wt !== wt || !wt.isOpen) return;
        for (const hash of encrypted) wt.sendAudio(sealed, hash, frame.timestamp, { energy: frame.energy, e2ee: true });
      })
      .catch((e: unknown) => this.emit('error', e instanceof Error ? e : new Error(String(e))));
  }

  /** Who is behind a downlink SSRC, from the rosters of the channels we are in. */
  private userForSsrc(ssrc: number): string | undefined {
    const cached = this.wtUserBySsrc.get(ssrc);
    if (cached !== undefined) return cached;
    for (const roster of this.channels.values()) {
      for (const p of roster.values()) {
        if (p.ssrc === ssrc) {
          this.wtUserBySsrc.set(ssrc, p.userId);
          return p.userId;
        }
      }
    }
    return undefined;
  }

  private onWebTransportAudio(audio: DownlinkAudio): void {
    const playback = this.wtPlayback;
    const renderer = this.renderer;
    if (!playback || !renderer) return;
    if (audio.pcmu) return;
    const now = Date.now();
    let slot = this.wtSlots.get(audio.ssrc);
    if (!slot) {
      const key = wtSlotKey(audio.ssrc);
      const node = playback.node(audio.ssrc, audio.mixed);
      renderer.addSource(key, node);
      slot = { userId: this.userForSsrc(audio.ssrc), e2ee: false, stereo: audio.mixed, lastPacketAt: now, gain: 0 };
      this.wtSlots.set(audio.ssrc, slot);
      if (this.visemesEnabledValue) void this.attachWebTransportVisemeTap(key);
      this.emitParticipantStreams();
    }
    slot.lastPacketAt = now;
    if (slot.userId === undefined) {
      slot.userId = this.userForSsrc(audio.ssrc);
      if (slot.userId !== undefined) this.emitParticipantStreams();
    }
    if (audio.e2ee) {
      const group = this.e2eeGroup;
      const userId = slot.userId;
      if (!group || userId === undefined) return;
      slot.e2ee = true;
      const current = slot;
      this.wtDecryptQueue = this.wtDecryptQueue
        .then(() => group.decrypt(userId, audio.frame))
        .then((plain) => {
          if (this.wtPlayback !== playback || this.wtSlots.get(audio.ssrc) !== current) return;
          current.stereo ||= opusPacketIsStereo(plain);
          this.renderWebTransportSlot(audio.ssrc, current, undefined);
          playback.push(audio.ssrc, plain, audio.sequence, audio.timestamp, current.stereo);
        })
        .catch(() => undefined);
      return;
    }
    slot.stereo ||= audio.mixed || opusPacketIsStereo(audio.frame);
    this.renderWebTransportSlot(audio.ssrc, slot, audio);
    playback.push(audio.ssrc, audio.frame, audio.sequence, audio.timestamp, slot.stereo);
  }

  /**
   * Server-processed frames carry the receiver's gain/direction (mutes, volumes, focus, ducking,
   * positional audio were applied on the node); E2EE frames arrive raw and are rendered here.
   */
  private renderWebTransportSlot(ssrc: number, slot: WebTransportSlot, audio: DownlinkAudio | undefined): void {
    const renderer = this.renderer;
    if (!renderer) return;
    const key = wtSlotKey(ssrc);
    if (audio) {
      if (slot.gain === audio.gain && directionsEqual(slot.direction, audio.direction)) return;
      slot.gain = audio.gain;
      slot.direction = audio.direction;
      renderer.render(key, audio.direction ? { gain: audio.gain, direction: audio.direction } : { gain: audio.gain });
      return;
    }
    renderer.render(key, slot.userId ? renderParams(this.renderInputsFor(slot.userId)) : { gain: 0 });
  }

  private async attachWebTransportVisemeTap(key: string): Promise<void> {
    const renderer = this.renderer;
    const ctx = renderer ? workletContext(renderer.context) : undefined;
    if (!renderer || !ctx) return;
    await loadVisemeWorklet(ctx);
    if (this.visemesEnabledValue && this.renderer === renderer) this.attachVisemeTap(key);
  }

  private removeWebTransportSlot(ssrc: number, notify = true): void {
    if (!this.wtSlots.delete(ssrc)) return;
    const key = wtSlotKey(ssrc);
    this.detachVisemeTap(key);
    this.renderer?.removeTrack(key);
    this.wtPlayback?.remove(ssrc);
    if (notify) this.emitParticipantStreams();
  }

  private sweepWebTransportSlots(): void {
    const idle = this.opts.webTransport?.idleTimeoutMs ?? 5_000;
    const cutoff = Date.now() - idle;
    for (const [ssrc, slot] of this.wtSlots) if (slot.lastPacketAt < cutoff) this.removeWebTransportSlot(ssrc);
  }

  /** Roster changed: SSRC → user lookups start over, departed speakers stop rendering. */
  private webTransportRosterChanged(): void {
    if (!this.wt) return;
    this.wtUserBySsrc.clear();
    for (const [ssrc, slot] of this.wtSlots) {
      if (slot.userId === undefined) continue;
      if (this.userForSsrc(ssrc) !== slot.userId) this.removeWebTransportSlot(ssrc);
    }
  }

  private webTransportStats(transport: AurxWebTransport): ClientStats {
    const s = transport.stats();
    const playback = this.wtPlayback?.stats;
    const input: RtcStatsInput = {
      inbound: {
        packetsReceived: s.audioPacketsReceived,
        packetsLost: s.audioPacketsLost,
        bytesReceived: s.bytesReceived,
        packetsDiscarded: s.packetsRejected + (playback?.framesDropped ?? 0),
        concealedSamples: (playback?.underruns ?? 0) * AURX_FRAME_SAMPLES,
      },
      outbound: { packetsSent: s.packetsSent, bytesSent: s.bytesSent },
    };
    if (s.rttMs !== undefined) input.iceRttSeconds = s.rttMs / 1000;
    const out = assembleClientStats(this.rtt, input, this.lossWindow, this.serverQuality);
    out.transport = 'webtransport';
    return out;
  }

  /**
   * Push the live part of the preferences (the bitrate ceiling) through `setParameters`. The
   * browser's congestion control still adapts underneath it.
   */
  private async applySenderPreferences(pc: RTCPeerConnection | undefined = this.pc): Promise<void> {
    const prefs = this.opusPreferences;
    if (this.appliedSenderPrefs && senderPreferencesEqual(this.appliedSenderPrefs, prefs)) return;
    const sender = pc?.getSenders().find((s) => s.track?.kind === 'audio');
    if (!sender) return;
    const params = sender.getParameters();
    if (!params.encodings || params.encodings.length === 0) params.encodings = [{}];
    const first = params.encodings[0];
    if (first) {
      if (prefs.maxBitrateBps !== undefined) first.maxBitrate = prefs.maxBitrateBps;
      else delete first.maxBitrate;
    }
    try {
      await sender.setParameters(params);
      this.appliedSenderPrefs = prefs;
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

  private startQualityTimer(): void {
    this.stopQualityTimer();
    if (this.opts.qualityReportIntervalMs <= 0) return;
    this.qualityTimer = setInterval(() => {
      this.reportQuality().catch((e: unknown) => {
        this.emit('error', e instanceof Error ? e : new Error(String(e)));
      });
    }, this.opts.qualityReportIntervalMs);
  }

  private stopQualityTimer(): void {
    if (this.qualityTimer !== undefined) clearInterval(this.qualityTimer);
    this.qualityTimer = undefined;
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
      offline: w.offline === true,
      cursor: encodeChatCursor(w.sent_at, w.id),
      reactions: (w.reactions ?? []).map((r) => ({ reaction: r.reaction, count: r.count, userIds: r.user_ids ?? [] })),
    };
    if (w.channel_id != null) m.channelId = w.channel_id;
    if (w.to_user_id != null) m.toUserId = w.to_user_id;
    if (w.metadata != null) m.metadata = w.metadata;
    if (clientRef !== undefined) m.clientRef = clientRef;
    if (w.edited_at != null) m.editedAt = new Date(w.edited_at);
    if (w.deleted_at != null) m.deletedAt = new Date(w.deleted_at);
    if (w.deleted_by != null) m.deletedBy = w.deleted_by;
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

function scopeWire(scope: ChatScope): { channel_id?: string; user_id?: string } {
  if (scope.channelId !== undefined) return { channel_id: scope.channelId };
  if (scope.userId !== undefined) return { user_id: scope.userId };
  throw new Error('ChatScope needs channelId or userId');
}

function scopeKey(scope: ChatScope): string {
  return scope.channelId !== undefined ? `c:${scope.channelId}` : `u:${scope.userId}`;
}

function readMarkerFromWire(w: ChatReadMarkerWire): ReadMarker {
  const m: ReadMarker = {
    userId: w.user_id,
    messageId: w.message_id,
    messageSentAt: new Date(w.message_sent_at),
    readAt: new Date(w.read_at),
  };
  if (w.channel_id != null) m.channelId = w.channel_id;
  if (w.peer_user_id != null) m.peerUserId = w.peer_user_id;
  return m;
}

/**
 * The server's opaque history cursor of a message: URL-safe base64 (no padding) of the
 * big-endian microsecond Unix timestamp followed by the 16 UUID bytes. Computed locally so
 * any message — live or stored — can anchor a `history()` call.
 */
export function encodeChatCursor(sentAt: string | Date, id: string): string {
  const micros = rfc3339Micros(sentAt);
  const bytes = new Uint8Array(24);
  const view = new DataView(bytes.buffer);
  view.setBigInt64(0, micros);
  const hex = id.replace(/-/g, '');
  if (hex.length !== 32) throw new Error(`not a UUID: ${id}`);
  for (let i = 0; i < 16; i++) bytes[8 + i] = parseInt(hex.slice(i * 2, i * 2 + 2), 16);
  let bin = '';
  for (const b of bytes) bin += String.fromCharCode(b);
  return btoa(bin).replace(/\+/g, '-').replace(/\//g, '_').replace(/=+$/, '');
}

/** Microseconds since the Unix epoch of an RFC 3339 string (sub-millisecond digits kept) or a Date. */
function rfc3339Micros(t: string | Date): bigint {
  if (t instanceof Date) return BigInt(t.getTime()) * 1000n;
  const m = /^(.*?)(?:\.(\d+))?(Z|[+-]\d\d:?\d\d)$/.exec(t);
  if (!m) return BigInt(new Date(t).getTime()) * 1000n;
  const whole = Date.parse(`${m[1]}${m[3]}`);
  if (Number.isNaN(whole)) throw new Error(`not an RFC 3339 timestamp: ${t}`);
  const frac = (m[2] ?? '').padEnd(6, '0').slice(0, 6);
  return BigInt(whole) * 1000n + BigInt(frac);
}
