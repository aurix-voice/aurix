/**
 * The browser side of one AURX media session, independent of what carries the packets.
 * `AurxWebTransport` (QUIC datagrams) and `AurxWebSocketTunnel` (binary frames on the control
 * WebSocket) are thin carriers over this class, which owns everything AURX: key derivation,
 * the signed `SessionBind`, sealing/opening through WebCrypto, heartbeats with RTT and dead-path
 * detection, per-sender anti-replay and loss accounting, and the counters behind `stats()`.
 */

import {
  AURX_AUTH_TAG_SIZE,
  AURX_HEADER_SIZE,
  AURX_MAX_PACKET_SIZE,
  AurxFlags,
  AurxKeys,
  AurxPacketType,
  ReplayWindow,
  SequenceLoss,
  audioHeader,
  audioLevelByte,
  decodePacket,
  heartbeatHeader,
  parseDownlinkAudio,
  sessionBindHeader,
  sessionBindPayload,
  type AurxHeader,
  type AurxPacket,
  type DownlinkAudio,
} from './aurx.js';

export interface AurxSessionKeys {
  sessionId: string;
  ssrc: number;
  /** 32-byte AURX master key (`media_key`). */
  masterKey: Uint8Array;
}

export interface AurxLinkOptions {
  /** Handshake + bind budget (ms). Default 6000. */
  connectTimeoutMs?: number;
  /** Heartbeat interval (ms), 0 disables heartbeats (no RTT probe, no dead-path detection). Default 2000. */
  heartbeatIntervalMs?: number;
  /** Consecutive unanswered heartbeats before the path counts as dead. Default 5. */
  heartbeatLossLimit?: number;
  /**
   * First uplink sequence to use. A resumed session must continue where its previous path
   * stopped (`nextSequence` of the old transport): the node keeps one anti-replay window per
   * session across paths. Default: random.
   */
  startSequence?: number;
}

export interface AurxLinkStats {
  url: string | undefined;
  packetsSent: number;
  packetsReceived: number;
  bytesSent: number;
  bytesReceived: number;
  /** Packets that failed to decode, authenticate or were replays. */
  packetsRejected: number;
  /** Packets dropped locally: outgoing-queue backpressure or a WebCrypto backlog after a main-thread stall (either direction). */
  packetsDroppedLocally: number;
  /** Smoothed heartbeat RTT in ms (`undefined` before the first ack). */
  rttMs: number | undefined;
  /** Audio packets received / missing from the senders' sequences, cumulative over the path. */
  audioPacketsReceived: number;
  audioPacketsLost: number;
  heartbeatsMissed: number;
  /** Largest packet the carrier takes (`undefined` = the AURX maximum). */
  maxDatagramSize: number | undefined;
}

export interface AurxLinkEvents {
  onAudio: (audio: DownlinkAudio) => void;
  /** A `BitrateCommand` from the server (target bitrate in bps). */
  onBitrate: (targetBps: number) => void;
  /** The path is gone (server close, network, heartbeat loss); `reason` is diagnostic. */
  onClosed: (reason: string) => void;
  onError: (error: Error) => void;
}

export function timeout<T>(p: Promise<T>, ms: number, what: string): Promise<T> {
  let timer: ReturnType<typeof setTimeout> | undefined;
  const t = new Promise<never>((_, reject) => {
    timer = setTimeout(() => reject(new Error(`${what} timed out after ${ms} ms`)), ms);
  });
  return Promise.race([p, t]).finally(() => {
    if (timer !== undefined) clearTimeout(timer);
  });
}

const BIND_RETRY_MS = 500;
const REPLAY_WINDOW = 256;
/** Audio frames allowed in flight through WebCrypto (uplink) before the oldest are dropped. */
const MAX_PENDING_SEALS = 128;
/** Packets allowed in flight through WebCrypto (downlink) before newcomers are dropped. */
const MAX_PENDING_OPENS = 512;

export type AurxLinkState = 'idle' | 'connecting' | 'open' | 'closed';

export abstract class AurxLink {
  protected state: AurxLinkState = 'idle';
  private keys: AurxKeys | undefined;
  private sequence: number;
  private heartbeatTimer: ReturnType<typeof setInterval> | undefined;
  private heartbeatSeq = 0;
  private heartbeatSent = new Map<number, number>();
  private heartbeatsMissed = 0;
  private rtt: number | undefined;
  private bindAck: ((ok: boolean) => void) | undefined;
  private readonly replay = new Map<number, ReplayWindow>();
  private readonly loss = new Map<number, SequenceLoss>();
  private forgottenLoss = { received: 0, lost: 0 };
  protected readonly counters = { sent: 0, received: 0, bytesSent: 0, bytesReceived: 0, rejected: 0, droppedLocally: 0 };
  protected closeReason: string | undefined;
  private sealChain: Promise<void> = Promise.resolve();
  private pendingSeals = 0;
  private openChain: Promise<void> = Promise.resolve();
  private pendingOpens = 0;

  protected constructor(
    protected readonly session: AurxSessionKeys,
    protected readonly events: AurxLinkEvents,
    protected readonly options: AurxLinkOptions = {},
  ) {
    this.sequence = (options.startSequence ?? Math.floor(Math.random() * 0x7fff_ffff)) >>> 0;
  }

  get isOpen(): boolean {
    return this.state === 'open';
  }

  /** Sequence the next uplink packet would use (carry it over to a reconnecting transport). */
  get nextSequence(): number {
    return (this.sequence + 1) >>> 0;
  }

  /** Where the packets go (diagnostic; the WebTransport URL or the control-channel URL). */
  abstract get connectedUrl(): string | undefined;

  /** Largest packet the carrier accepts right now (`undefined` = the AURX maximum). */
  protected abstract carrierMaxPacketSize(): number | undefined;

  /** Hand one sealed packet to the carrier; `false` = dropped for backpressure (counted by the caller). */
  protected abstract carrierWrite(packet: Uint8Array): boolean;

  /** Release the carrier after `close()`; `reason` is what `close()` was given. */
  protected abstract carrierClose(reason: string): void;

  /** Send one Opus (or E2EE-sealed) frame for the channel with `channelIdHash`. */
  sendAudio(frame: Uint8Array, channelIdHash: number, timestamp: number, opts: { energy?: number; e2ee?: boolean } = {}): void {
    if (this.state !== 'open' || !this.keys) return;
    const limit = Math.min(AURX_MAX_PACKET_SIZE, this.carrierMaxPacketSize() ?? AURX_MAX_PACKET_SIZE) - (AURX_HEADER_SIZE + AURX_AUTH_TAG_SIZE + 1);
    if (frame.length === 0) return;
    if (frame.length > limit) {
      this.counters.droppedLocally += 1;
      return;
    }
    let flags = 0;
    let payload: Uint8Array;
    if (opts.energy !== undefined) {
      flags |= AurxFlags.Energy;
      payload = new Uint8Array(1 + frame.length);
      payload[0] = audioLevelByte(opts.energy);
      payload.set(frame, 1);
    } else {
      payload = frame;
    }
    if (opts.e2ee) flags |= AurxFlags.E2ee;
    const header = audioHeader(this.session.ssrc, this.takeSequence(), timestamp >>> 0, channelIdHash, flags);
    if (this.pendingSeals >= MAX_PENDING_SEALS) {
      this.counters.droppedLocally += 1;
      return;
    }
    void this.sealAndSend(header, payload);
  }

  stats(): AurxLinkStats {
    let received = this.forgottenLoss.received;
    let lost = this.forgottenLoss.lost;
    for (const l of this.loss.values()) {
      received += l.received;
      lost += l.lost;
    }
    return {
      url: this.connectedUrl,
      packetsSent: this.counters.sent,
      packetsReceived: this.counters.received,
      bytesSent: this.counters.bytesSent,
      bytesReceived: this.counters.bytesReceived,
      packetsRejected: this.counters.rejected,
      packetsDroppedLocally: this.counters.droppedLocally,
      rttMs: this.rtt,
      audioPacketsReceived: received,
      audioPacketsLost: lost,
      heartbeatsMissed: this.heartbeatsMissed,
      maxDatagramSize: this.carrierMaxPacketSize(),
    };
  }

  /** Forget the per-SSRC state of a participant who left. */
  forgetSsrc(ssrc: number): void {
    this.replay.delete(ssrc);
    const l = this.loss.get(ssrc);
    if (l) {
      this.forgottenLoss.received += l.received;
      this.forgottenLoss.lost += l.lost;
      this.loss.delete(ssrc);
    }
  }

  close(reason = 'closed by client'): void {
    if (this.state === 'closed') return;
    this.closeReason = reason;
    this.state = 'closed';
    this.stopHeartbeats();
    this.bindAck?.(false);
    this.bindAck = undefined;
    this.carrierClose(reason);
  }

  // ── For carriers ──

  protected async deriveKeys(): Promise<void> {
    this.keys = await AurxKeys.derive(this.session.masterKey);
  }

  /**
   * Sends `SessionBind` (retrying every 500 ms) until the node acknowledges it over this
   * carrier; rejects when the link is closed meanwhile.
   */
  protected async bind(): Promise<void> {
    const keys = this.keys;
    if (!keys) throw new Error('no keys');
    const acked = new Promise<boolean>((resolve) => {
      this.bindAck = resolve;
    });
    const send = async (): Promise<void> => {
      const unixMs = Date.now();
      const nonce = Math.floor(Math.random() * 0xffff_ffff);
      const packet = await keys.signPlain(sessionBindHeader(this.session.ssrc, unixMs, nonce), sessionBindPayload(this.session.sessionId, unixMs, nonce));
      this.write(packet);
    };
    await send();
    const retry = setInterval(() => {
      void send().catch(() => undefined);
    }, BIND_RETRY_MS);
    try {
      const ok = await acked;
      if (!ok) throw new Error(this.closeReason ?? 'bind aborted');
    } finally {
      clearInterval(retry);
      this.bindAck = undefined;
    }
  }

  /** A bind attempt over a carrier that went away: unblock `bind()` without closing the link. */
  protected abortBind(): void {
    this.bindAck?.(false);
  }

  protected startHeartbeats(): void {
    this.stopHeartbeats();
    const interval = this.options.heartbeatIntervalMs ?? 2000;
    const limit = this.options.heartbeatLossLimit ?? 5;
    if (interval <= 0) return;
    this.heartbeatTimer = setInterval(() => {
      if (this.state !== 'open') return;
      const outstanding = this.heartbeatSent.size;
      if (outstanding >= limit) {
        this.heartbeatsMissed = outstanding;
        this.fail(`no heartbeat answer for ${outstanding} intervals`);
        return;
      }
      this.heartbeatSeq = (this.heartbeatSeq + 1) >>> 0;
      const ts = this.heartbeatSeq;
      this.heartbeatSent.set(ts, performance.now());
      void this.sealAndSend(heartbeatHeader(this.session.ssrc, this.takeSequence(), ts), new Uint8Array(0));
    }, interval);
  }

  private stopHeartbeats(): void {
    if (this.heartbeatTimer !== undefined) clearInterval(this.heartbeatTimer);
    this.heartbeatTimer = undefined;
    this.heartbeatSent.clear();
  }

  /** The path is gone: close and, if it had been open, tell the owner. */
  protected fail(reason: string): void {
    if (this.state === 'closed') return;
    const wasOpen = this.state === 'open';
    this.close(reason);
    if (wasOpen) this.events.onClosed(reason);
  }

  /**
   * Authentication is asynchronous (WebCrypto): every packet is opened as soon as it arrives and
   * the results are handled in arrival order, so a backlog after a main-thread stall drains in one
   * round trip instead of one per packet.
   */
  protected onCarrierPacket(data: Uint8Array): void {
    const keys = this.keys;
    if (!keys) return;
    this.counters.bytesReceived += data.length;
    const decoded = decodePacket(data);
    if (!decoded) {
      this.counters.rejected += 1;
      return;
    }
    if (this.pendingOpens >= MAX_PENDING_OPENS) {
      this.counters.droppedLocally += 1;
      return;
    }
    this.pendingOpens += 1;
    const opened = keys.open(data, decoded).finally(() => {
      this.pendingOpens -= 1;
    });
    this.openChain = this.openChain.then(async () => {
      let packet: AurxPacket | undefined;
      try {
        packet = await opened;
      } catch {
        packet = undefined;
      }
      if (this.keys === keys) this.onPacket(packet);
    });
  }

  // ── Internals ──

  private takeSequence(): number {
    this.sequence = (this.sequence + 1) >>> 0;
    return this.sequence;
  }

  /**
   * Sealing is asynchronous (WebCrypto). Every packet starts sealing at once — so a burst of frames
   * released after a long main-thread stall (game render loop, GC) costs one round trip, not one per
   * frame — and only the writes are chained, so packets still leave in sequence order.
   */
  private sealAndSend(header: AurxHeader, payload: Uint8Array): Promise<void> {
    const keys = this.keys;
    if (!keys) return Promise.resolve();
    this.pendingSeals += 1;
    const sealed = keys.seal(header, payload).finally(() => {
      this.pendingSeals -= 1;
    });
    this.sealChain = this.sealChain.then(() => this.sendSealed(sealed));
    return this.sealChain;
  }

  private async sendSealed(sealed: Promise<Uint8Array>): Promise<void> {
    try {
      this.write(await sealed);
    } catch (e) {
      if (this.state === 'open') this.events.onError(e instanceof Error ? e : new Error(String(e)));
    }
  }

  private write(packet: Uint8Array): void {
    if (this.state === 'closed') return;
    if (!this.carrierWrite(packet)) {
      this.counters.droppedLocally += 1;
      return;
    }
    this.counters.sent += 1;
    this.counters.bytesSent += packet.length;
  }

  private onPacket(packet: AurxPacket | undefined): void {
    if (!packet) {
      this.counters.rejected += 1;
      return;
    }
    const h = packet.header;
    let window = this.replay.get(h.ssrc);
    if (!window) {
      window = new ReplayWindow(REPLAY_WINDOW);
      this.replay.set(h.ssrc, window);
    }
    if (h.packetType === AurxPacketType.Audio || h.packetType === AurxPacketType.AudioFec) {
      if (!window.accept(h.sequence)) {
        this.counters.rejected += 1;
        return;
      }
    }
    this.counters.received += 1;
    switch (h.packetType) {
      case AurxPacketType.SessionBindAck:
        this.bindAck?.(true);
        return;
      case AurxPacketType.HeartbeatAck: {
        const sent = this.heartbeatSent.get(h.timestamp);
        if (sent !== undefined) {
          const sample = performance.now() - sent;
          this.rtt = this.rtt === undefined ? sample : this.rtt * 0.8 + sample * 0.2;
          for (const ts of Array.from(this.heartbeatSent.keys())) if (ts <= h.timestamp) this.heartbeatSent.delete(ts);
        }
        this.heartbeatsMissed = 0;
        return;
      }
      case AurxPacketType.Audio: {
        const audio = parseDownlinkAudio(packet);
        if (!audio) {
          this.counters.rejected += 1;
          return;
        }
        let loss = this.loss.get(h.ssrc);
        if (!loss) {
          loss = new SequenceLoss();
          this.loss.set(h.ssrc, loss);
        }
        loss.observe(h.sequence);
        this.events.onAudio(audio);
        return;
      }
      case AurxPacketType.BitrateCommand: {
        if (packet.payload.length >= 4) {
          const v = new DataView(packet.payload.buffer, packet.payload.byteOffset, packet.payload.byteLength);
          this.events.onBitrate(v.getUint32(0));
        }
        return;
      }
      case AurxPacketType.SessionClose:
        this.fail('session closed by the server');
        return;
      default:
        return;
    }
  }
}
