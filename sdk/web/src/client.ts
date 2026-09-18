import {
  AURIX_SUBPROTOCOL,
  BEARER_SUBPROTOCOL_PREFIX,
  MAX_PARTICIPANT_VOLUME,
  RESUME_SUBPROTOCOL_PREFIX,
  parseServerMessage,
  type ClientMessage,
  type LocalMute,
  type ParticipantBrief,
  type ParticipantVolume,
  type RecordingConsent,
  type ServerMessage,
  type TurnCredentials,
  type UnknownMessage,
  type UserPosition,
} from './protocol.js';

export interface AurixClientOptions {
  /** REST base URL, e.g. `https://voice.example.com` (used for TURN credentials). */
  apiUrl: string;
  /** WebSocket URL of the control channel, e.g. `wss://voice.example.com/ws`. */
  wsUrl: string;
  /** Player JWT issued by your backend via `POST /v1/tokens`. */
  token: string;
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
}

/** Marker for "in every channel" in the local-mute table. */
const ALL_CHANNELS = '*';

interface Pending<T> {
  resolve: (value: T) => void;
  reject: (error: Error) => void;
  timer: ReturnType<typeof setTimeout>;
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
  private listeners = new Map<keyof AurixEvents, Set<AnyListener>>();
  private pendingJoins = new Map<string, Pending<Participant[]>>();
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
  }

  /** Channel-scoped mutes need membership, so they are re-sent per `ChannelJoinAck`. */
  private replayChannelMutes(channelId: string): void {
    for (const [userId, scopes] of this.localMutes) {
      if (scopes.has(channelId) && !scopes.has(ALL_CHANNELS)) {
        this.trySend({
          type: 'SetParticipantMute',
          data: { user_id: userId, channel_id: channelId, muted: true },
        });
      }
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
    this.pc?.close();
    this.pc = undefined;
    if (this.localStream && this.localStream !== this.opts.localStream) {
      this.localStream.getTracks().forEach((t) => t.stop());
    }
    this.localStream = undefined;
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
    this.rejectPending(this.pendingAnswer, reason);
    this.pendingAnswer = undefined;
    this.rejectPending(this.pendingInit, reason);
    this.pendingInit = undefined;
  }

  // ── Channels ──

  /** Join a channel the token authorises. Resolves with the current participant list. */
  joinChannel(channelId: string): Promise<Participant[]> {
    this.requireOpen();
    if (this.pendingJoins.has(channelId)) return Promise.reject(new Error('join already pending'));
    return new Promise<Participant[]>((resolve, reject) => {
      const timer = setTimeout(() => {
        this.pendingJoins.delete(channelId);
        reject(new Error(`join ${channelId} timed out`));
      }, this.opts.requestTimeoutMs);
      this.pendingJoins.set(channelId, { resolve, reject, timer });
      this.send({ type: 'ChannelJoin', data: { channel_id: channelId, token: this.opts.token } });
    });
  }

  leaveChannel(channelId: string): void {
    this.requireOpen();
    this.send({ type: 'ChannelLeave', data: { channel_id: channelId } });
    if (this.channels.delete(channelId)) this.emit('channelLeft', channelId);
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
          this.emit('participantUpdated', d.channel_id, p);
        }
        this.emit('speaking', d.channel_id, d.user_id, d.speaking);
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
        this.emit('receiverPreferences', {
          blockedUsers: d.blocked_users,
          localMutes: d.local_mutes,
          volumes: d.volumes,
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
        // A failed join/offer is reported as a generic Error; fail the oldest pending request.
        const join = this.pendingJoins.entries().next();
        if (!join.done) {
          const [channelId, pending] = join.value;
          clearTimeout(pending.timer);
          this.pendingJoins.delete(channelId);
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

    this.localStream ??=
      this.opts.localStream ??
      (await navigator.mediaDevices.getUserMedia({
        audio: this.opts.audioConstraints ?? {
          echoCancellation: true,
          noiseSuppression: true,
          autoGainControl: true,
        },
        video: false,
      }));
    this.localStream.getAudioTracks().forEach((t) => {
      t.enabled = !this.muted;
    });

    const pc = new RTCPeerConnection({ iceServers, bundlePolicy: 'max-bundle', rtcpMuxPolicy: 'require' });
    this.pc = pc;
    // One sendrecv audio transceiver: uplink microphone, downlink server-side mix.
    const track = this.localStream.getAudioTracks()[0];
    if (!track) throw new Error('no audio track');
    pc.addTransceiver(track, { direction: 'sendrecv', streams: [this.localStream] });

    pc.ontrack = (ev) => {
      const stream = ev.streams[0] ?? new MediaStream([ev.track]);
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
    await pc.setLocalDescription(offer);
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
