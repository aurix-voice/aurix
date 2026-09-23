/**
 * One room session on the Aurix Web SDK, exposed to React as an external store. The controller
 * owns the `AurixClient`, mirrors what the UI needs into an immutable snapshot and re-issues
 * the player token through the backend when the SDK reconnects.
 */

import {
  AurixClient,
  type ChatMessage,
  type ConnectionState,
  type MediaTransport,
  type NetworkQuality,
  type Participant,
} from "@aurix/web-sdk";

import { api, type JoinGrant } from "./api";

export type Phase = "connecting" | "connected" | "reconnecting" | "failed" | "left" | "kicked";

export interface Person {
  userId: string;
  name: string;
  speaking: boolean;
  energy: number;
  muted: boolean;
  self: boolean;
  bot: boolean;
  priority: boolean;
}

export interface Snapshot {
  phase: Phase;
  transport?: MediaTransport;
  people: Person[];
  muted: boolean;
  outputMuted: boolean;
  micAvailable: boolean;
  quality?: NetworkQuality;
  messages: ChatMessage[];
  typing: string[];
  needsAudioTap: boolean;
  error?: string;
}

const MAX_MESSAGES = 200;
const BOT_NAME_RE = /\bbot\b/i;

type Listener = () => void;

export class RoomController {
  private client: AurixClient | undefined;
  private snapshot: Snapshot;
  private readonly listeners = new Set<Listener>();
  private readonly output: HTMLAudioElement;
  private mic: MediaStream | undefined;
  private silentCtx: AudioContext | undefined;
  private selfId: string | undefined;
  private localSpeaking = false;
  private localEnergy = 0;
  private readonly typingTimers = new Map<string, ReturnType<typeof setTimeout>>();
  private stopped = false;

  constructor(
    private readonly slug: string,
    private readonly name: string,
    private readonly deviceId: string,
    private grant: JoinGrant,
  ) {
    this.output = document.createElement("audio");
    this.output.autoplay = true;
    this.output.setAttribute("playsinline", "");
    this.snapshot = {
      phase: "connecting",
      people: [],
      muted: false,
      outputMuted: false,
      micAvailable: true,
      messages: [],
      typing: [],
      needsAudioTap: false,
    };
  }

  subscribe = (listener: Listener): (() => void) => {
    this.listeners.add(listener);
    return () => this.listeners.delete(listener);
  };

  getSnapshot = (): Snapshot => this.snapshot;

  private patch(partial: Partial<Snapshot>): void {
    this.snapshot = { ...this.snapshot, ...partial };
    for (const l of this.listeners) l();
  }

  async start(): Promise<void> {
    let localStream: MediaStream | undefined;
    let micAvailable = true;
    try {
      localStream = await navigator.mediaDevices.getUserMedia({
        audio: { echoCancellation: true, noiseSuppression: true, autoGainControl: true, channelCount: 1 },
        video: false,
      });
    } catch {
      micAvailable = false;
      localStream = this.silentStream();
    }
    if (this.stopped) {
      localStream.getTracks().forEach((t) => t.stop());
      return;
    }
    this.mic = localStream;
    this.patch({ micAvailable });

    const client = new AurixClient({
      apiUrl: this.grant.apiUrl,
      wsUrl: this.grant.wsUrl,
      token: this.grant.token,
      refreshToken: async () => {
        this.grant = await api.join(this.slug, this.name, this.deviceId);
        return this.grant.token;
      },
      deviceId: this.deviceId,
      localStream,
      localVoiceActivity: true,
      autoReconnect: true,
      transport: this.grant.transport,
      webRtcConnectTimeoutMs: 6000,
    });
    this.client = client;
    this.wire(client);
    client.attachAudioOutput(this.output);

    try {
      await client.connect();
      await client.joinChannel(this.grant.channelId);
      this.refreshPeople();
      if (!(await client.resumeAudio())) this.patch({ needsAudioTap: true });
    } catch (e) {
      if (this.stopped) return;
      this.patch({ phase: "failed", error: e instanceof Error ? e.message : String(e) });
    }
  }

  private silentStream(): MediaStream {
    this.silentCtx = new AudioContext();
    return this.silentCtx.createMediaStreamDestination().stream;
  }

  private wire(client: AurixClient): void {
    client.on("connectionState", (state: ConnectionState) => {
      if (state === "reconnecting") this.patch({ phase: "reconnecting" });
      else if (state === "failed" && this.snapshot.phase !== "left") this.patch({ phase: "failed" });
    });
    client.on("sessionReady", (info) => {
      this.selfId = info.userId;
    });
    client.on("mediaTransport", (transport) => this.patch({ transport, phase: "connected", error: undefined }));
    client.on("recovered", () => {
      this.patch({ phase: "connected", error: undefined });
      this.refreshPeople();
    });
    client.on("failedToRecover", (err) => this.patch({ phase: "failed", error: err.message }));
    client.on("kicked", () => {
      this.patch({ phase: "kicked" });
      this.stop();
    });
    client.on("sessionClosed", (reason) => {
      if (this.snapshot.phase === "connected" || this.snapshot.phase === "reconnecting") {
        this.patch({ phase: "failed", error: reason });
      }
    });
    const refresh = () => this.refreshPeople();
    client.on("channelJoined", refresh);
    client.on("participantJoined", refresh);
    client.on("participantLeft", (_c, userId) => {
      this.clearTyping(userId);
      refresh();
    });
    client.on("participantUpdated", refresh);
    client.on("speaking", refresh);
    client.on("energy", refresh);
    client.on("participantPriorityChanged", refresh);
    client.on("localSpeaking", (speaking) => {
      this.localSpeaking = speaking;
      this.refreshPeople();
    });
    client.on("localEnergy", (sample) => {
      this.localEnergy = sample.energy;
      if (this.localSpeaking) this.refreshPeople();
    });
    client.on("networkQuality", (quality) => this.patch({ quality }));
    client.on("chatMessage", (message) => {
      if (message.channelId !== this.grant.channelId) return;
      this.clearTyping(message.fromUserId);
      const messages = [...this.snapshot.messages.filter((m) => m.id !== message.id), message].slice(-MAX_MESSAGES);
      this.patch({ messages });
    });
    client.on("chatMessageUpdated", (message) => {
      const messages = this.snapshot.messages.map((m) => (m.id === message.id ? message : m));
      this.patch({ messages });
    });
    client.on("participantTyping", (_c, userId, typing) => {
      if (userId === this.selfId) return;
      this.clearTyping(userId);
      if (!typing) return;
      this.typingTimers.set(
        userId,
        setTimeout(() => this.clearTyping(userId), 4000),
      );
      this.patch({ typing: [...this.snapshot.typing.filter((u) => u !== userId), userId] });
    });
    client.on("error", (err) => console.warn("[aurix]", err.message));
    client.on("serverError", (code, message) => console.warn("[aurix]", code, message));
  }

  private clearTyping(userId: string): void {
    const timer = this.typingTimers.get(userId);
    if (timer) clearTimeout(timer);
    this.typingTimers.delete(userId);
    if (this.snapshot.typing.includes(userId)) this.patch({ typing: this.snapshot.typing.filter((u) => u !== userId) });
  }

  private refreshPeople(): void {
    const client = this.client;
    if (!client) return;
    const remote: Participant[] = client.participants(this.grant.channelId);
    const others: Person[] = remote
      .filter((p) => p.userId !== this.selfId)
      .map((p) => ({
        userId: p.userId,
        name: p.displayName,
        speaking: p.speaking,
        energy: p.energy,
        muted: p.muted || p.serverMuted,
        self: false,
        bot: p.priority || BOT_NAME_RE.test(p.displayName),
        priority: p.priority,
      }))
      .sort((a, b) => Number(b.bot) - Number(a.bot) || a.name.localeCompare(b.name));
    const self: Person = {
      userId: this.selfId ?? "self",
      name: this.name,
      speaking: this.localSpeaking && !this.snapshot.muted,
      energy: this.localEnergy,
      muted: this.snapshot.muted || !this.snapshot.micAvailable,
      self: true,
      bot: false,
      priority: false,
    };
    this.patch({ people: [self, ...others] });
  }

  setMuted(muted: boolean): void {
    this.client?.setMuted(muted);
    this.snapshot = { ...this.snapshot, muted };
    this.refreshPeople();
  }

  setOutputMuted(muted: boolean): void {
    this.client?.setOutputMuted(muted);
    this.patch({ outputMuted: muted });
  }

  async tapAudio(): Promise<void> {
    const ok = (await this.client?.resumeAudio()) ?? true;
    this.patch({ needsAudioTap: !ok });
  }

  async send(text: string): Promise<void> {
    const trimmed = text.trim();
    if (!trimmed || !this.client) return;
    await this.client.sendMessage(this.grant.channelId, trimmed.slice(0, 1000));
  }

  typing(): void {
    this.client?.setTyping(this.grant.channelId, true);
  }

  leave(): void {
    if (this.snapshot.phase !== "kicked") this.patch({ phase: "left" });
    this.stop();
  }

  private stop(): void {
    this.stopped = true;
    this.client?.disconnect();
    this.client = undefined;
    this.output.srcObject = null;
    this.mic?.getTracks().forEach((t) => t.stop());
    void this.silentCtx?.close().catch(() => undefined);
    for (const timer of this.typingTimers.values()) clearTimeout(timer);
    this.typingTimers.clear();
  }
}
