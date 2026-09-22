/**
 * AURX over WebTransport: the browser's strict-firewall media path. One sealed AURX packet
 * per QUIC datagram to the node's HTTP/3 endpoint (`SessionInitAck.webtransport`, normally
 * UDP/443, path `/aurix`). The node authenticates the session with the same signed
 * `SessionBind` the native SDKs use; replay protection, E2EE and packet limits are AURX's.
 *
 * Certificates: the node advertises SHA-256 pins of its generated short-lived certificate
 * (`cert_sha256`, passed as `serverCertificateHashes`); an operator certificate from a public
 * CA carries no pins and is verified by the browser like any HTTPS server.
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
  type DownlinkAudio,
} from './aurx.js';
import type { WebTransportInfoWire } from './protocol.js';

export interface WebTransportSessionKeys {
  sessionId: string;
  ssrc: number;
  /** 32-byte AURX master key (`media_key`). */
  masterKey: Uint8Array;
}

export interface AurxWebTransportOptions {
  /** Handshake + bind budget per URL (ms). Default 6000. */
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
  /** Test seam: the `WebTransport` constructor to use. */
  webTransport?: WebTransportConstructor;
}

export type WebTransportConstructor = new (url: string, options?: WebTransportOptions) => WebTransport;

export interface AurxWebTransportStats {
  url: string | undefined;
  packetsSent: number;
  packetsReceived: number;
  bytesSent: number;
  bytesReceived: number;
  /** Datagrams that failed to decode, authenticate or were replays. */
  packetsRejected: number;
  /** Datagrams dropped by the browser's outgoing queue (`writable` backpressure). */
  packetsDroppedLocally: number;
  /** Smoothed heartbeat RTT in ms (`undefined` before the first ack). */
  rttMs: number | undefined;
  /** Audio packets received / missing from the senders' sequences, cumulative over the path. */
  audioPacketsReceived: number;
  audioPacketsLost: number;
  heartbeatsMissed: number;
  maxDatagramSize: number | undefined;
}

export interface AurxWebTransportEvents {
  onAudio: (audio: DownlinkAudio) => void;
  /** A `BitrateCommand` from the server (target bitrate in bps). */
  onBitrate: (targetBps: number) => void;
  /** The path is gone (server close, network, heartbeat loss); `reason` is diagnostic. */
  onClosed: (reason: string) => void;
  onError: (error: Error) => void;
}

export interface WebTransportSupport {
  ok: boolean;
  webTransport: boolean;
  datagrams: boolean;
  crypto: boolean;
}

/** Whether this browser can carry AURX over WebTransport (datagrams + WebCrypto). */
export function detectWebTransportSupport(ctor?: WebTransportConstructor): WebTransportSupport {
  const wt = ctor ?? (globalThis as { WebTransport?: WebTransportConstructor }).WebTransport;
  const webTransport = typeof wt === 'function';
  const datagrams = webTransport && (ctor !== undefined || 'datagrams' in (wt.prototype as object));
  const crypto = typeof globalThis.crypto?.subtle?.sign === 'function';
  return { ok: webTransport && datagrams && crypto, webTransport, datagrams, crypto };
}

function hexBytes(hex: string): Uint8Array {
  const clean = hex.replace(/[^0-9a-f]/gi, '');
  const out = new Uint8Array(clean.length / 2);
  for (let i = 0; i < out.length; i++) out[i] = parseInt(clean.slice(i * 2, i * 2 + 2), 16);
  return out;
}

/** `serverCertificateHashes` for the node's advertised pins (none for CA certificates). */
export function certificateHashes(info: WebTransportInfoWire): WebTransportHash[] | undefined {
  const pins = (info.cert_sha256 ?? []).filter((h) => /^[0-9a-f:]{64,95}$/i.test(h));
  if (pins.length === 0) return undefined;
  return pins.map((h) => ({ algorithm: 'sha-256', value: hexBytes(h) as Uint8Array<ArrayBuffer> }));
}

function timeout<T>(p: Promise<T>, ms: number, what: string): Promise<T> {
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

type State = 'idle' | 'connecting' | 'open' | 'closed';

/** One WebTransport session carrying one AURX media session. */
export class AurxWebTransport {
  private state: State = 'idle';
  private transport: WebTransport | undefined;
  private writer: WritableStreamDefaultWriter<Uint8Array> | undefined;
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
  private readonly counters = { sent: 0, received: 0, bytesSent: 0, bytesReceived: 0, rejected: 0, droppedLocally: 0 };
  private url: string | undefined;
  private closeReason: string | undefined;
  /** Sealed audio payloads waiting for the writer (bounded; oldest dropped). */
  private writing = false;
  private sealChain: Promise<void> = Promise.resolve();
  private readonly outQueue: Uint8Array[] = [];

  constructor(
    private readonly session: WebTransportSessionKeys,
    private readonly events: AurxWebTransportEvents,
    private readonly options: AurxWebTransportOptions = {},
  ) {
    this.sequence = (options.startSequence ?? Math.floor(Math.random() * 0x7fff_ffff)) >>> 0;
  }

  get isOpen(): boolean {
    return this.state === 'open';
  }

  get connectedUrl(): string | undefined {
    return this.url;
  }

  /** Sequence the next uplink packet would use (carry it over to a reconnecting transport). */
  get nextSequence(): number {
    return (this.sequence + 1) >>> 0;
  }

  /**
   * Try the advertised URLs in order; resolves once the node acknowledged our `SessionBind`
   * over the new path (the session now receives its downlink here).
   */
  async connect(info: WebTransportInfoWire): Promise<void> {
    if (this.state !== 'idle') throw new Error(`WebTransport connect in state ${this.state}`);
    this.state = 'connecting';
    const ctor = this.options.webTransport ?? (globalThis as { WebTransport?: WebTransportConstructor }).WebTransport;
    if (typeof ctor !== 'function') {
      this.state = 'closed';
      throw new Error('WebTransport is not available in this browser');
    }
    if (info.urls.length === 0) {
      this.state = 'closed';
      throw new Error('the node advertises no WebTransport URL');
    }
    this.keys = await AurxKeys.derive(this.session.masterKey);
    const hashes = certificateHashes(info);
    const budget = this.options.connectTimeoutMs ?? 6000;
    const errors: string[] = [];
    for (const url of info.urls) {
      if (this.state !== 'connecting') break;
      let transport: WebTransport;
      try {
        transport = hashes ? new ctor(url, { serverCertificateHashes: hashes }) : new ctor(url);
      } catch (e) {
        errors.push(`${url}: ${e instanceof Error ? e.message : String(e)}`);
        continue;
      }
      try {
        await timeout(transport.ready, budget, `WebTransport handshake to ${url}`);
        this.transport = transport;
        this.url = url;
        this.writer = transport.datagrams.writable.getWriter();
        void this.readLoop(transport);
        void transport.closed.then(
          (info) => this.onTransportClosed(transport, `closed by peer (${info.closeCode ?? 0}${info.reason ? `: ${info.reason}` : ''})`),
          (e: unknown) => this.onTransportClosed(transport, `connection lost (${e instanceof Error ? e.message : String(e)})`),
        );
        await timeout(this.bind(), budget, `SessionBind over ${url}`);
        if (this.state !== 'connecting') throw new Error(this.closeReason ?? 'closed while binding');
        this.state = 'open';
        this.startHeartbeats();
        return;
      } catch (e) {
        errors.push(`${url}: ${e instanceof Error ? e.message : String(e)}`);
        this.bindAck?.(false);
        this.transport = undefined;
        this.url = undefined;
        this.dropTransport(transport);
        if (this.state !== 'connecting') break;
      }
    }
    this.state = 'closed';
    throw new Error(`WebTransport media path unavailable: ${errors.join('; ')}`);
  }

  /** Send one Opus (or E2EE-sealed) frame for the channel with `channelIdHash`. */
  sendAudio(frame: Uint8Array, channelIdHash: number, timestamp: number, opts: { energy?: number; e2ee?: boolean } = {}): void {
    if (this.state !== 'open' || !this.keys) return;
    const limit = Math.min(AURX_MAX_PACKET_SIZE, this.transport?.datagrams.maxDatagramSize ?? AURX_MAX_PACKET_SIZE) - (AURX_HEADER_SIZE + AURX_AUTH_TAG_SIZE + 1);
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
    void this.sealAndSend(header, payload);
  }

  stats(): AurxWebTransportStats {
    let received = this.forgottenLoss.received;
    let lost = this.forgottenLoss.lost;
    for (const l of this.loss.values()) {
      received += l.received;
      lost += l.lost;
    }
    return {
      url: this.url,
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
      maxDatagramSize: this.transport?.datagrams.maxDatagramSize,
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
    this.state = 'closed';
    this.closeReason = reason;
    this.stopHeartbeats();
    this.bindAck?.(false);
    this.bindAck = undefined;
    const t = this.transport;
    this.transport = undefined;
    if (t) this.dropTransport(t);
  }

  private takeSequence(): number {
    this.sequence = (this.sequence + 1) >>> 0;
    return this.sequence;
  }

  private async bind(): Promise<void> {
    const keys = this.keys;
    if (!keys) throw new Error('no keys');
    const acked = new Promise<boolean>((resolve) => {
      this.bindAck = resolve;
    });
    const send = async (): Promise<void> => {
      const unixMs = Date.now();
      const nonce = Math.floor(Math.random() * 0xffff_ffff);
      const packet = await keys.signPlain(sessionBindHeader(this.session.ssrc, unixMs, nonce), sessionBindPayload(this.session.sessionId, unixMs, nonce));
      await this.write(packet);
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

  private startHeartbeats(): void {
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

  /** Sealing is asynchronous (WebCrypto); chain it so packets leave in sequence order. */
  private sealAndSend(header: AurxHeader, payload: Uint8Array): Promise<void> {
    this.sealChain = this.sealChain.then(() => this.sealAndSendNow(header, payload));
    return this.sealChain;
  }

  private async sealAndSendNow(header: AurxHeader, payload: Uint8Array): Promise<void> {
    const keys = this.keys;
    if (!keys) return;
    try {
      const packet = await keys.seal(header, payload);
      await this.write(packet);
    } catch (e) {
      if (this.state === 'open') this.events.onError(e instanceof Error ? e : new Error(String(e)));
    }
  }

  private async write(packet: Uint8Array): Promise<void> {
    const writer = this.writer;
    if (!writer) return;
    if (this.writing) {
      // The browser's outgoing datagram queue is full; keep the newest few, drop the rest.
      this.outQueue.push(packet);
      if (this.outQueue.length > 8) {
        this.outQueue.shift();
        this.counters.droppedLocally += 1;
      }
      return;
    }
    this.writing = true;
    try {
      let next: Uint8Array | undefined = packet;
      while (next) {
        await writer.write(next as Uint8Array<ArrayBuffer>);
        this.counters.sent += 1;
        this.counters.bytesSent += next.length;
        next = this.outQueue.shift();
      }
    } finally {
      this.writing = false;
    }
  }

  private async readLoop(transport: WebTransport): Promise<void> {
    const reader = transport.datagrams.readable.getReader();
    try {
      for (;;) {
        const { value, done } = await reader.read();
        if (done) break;
        if (this.transport !== transport) break;
        if (value instanceof Uint8Array) await this.onDatagram(value);
      }
    } catch (e) {
      if (this.transport === transport) this.onTransportClosed(transport, `read failed (${e instanceof Error ? e.message : String(e)})`);
    } finally {
      try {
        reader.releaseLock();
      } catch {
        // stream already gone
      }
    }
  }

  private async onDatagram(data: Uint8Array): Promise<void> {
    const keys = this.keys;
    if (!keys) return;
    this.counters.bytesReceived += data.length;
    const decoded = decodePacket(data);
    if (!decoded) {
      this.counters.rejected += 1;
      return;
    }
    const packet = await keys.open(data, decoded);
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

  private onTransportClosed(transport: WebTransport, reason: string): void {
    if (this.transport !== transport) return;
    this.fail(reason);
  }

  private fail(reason: string): void {
    if (this.state === 'closed') return;
    const wasOpen = this.state === 'open';
    this.close(reason);
    if (wasOpen) this.events.onClosed(reason);
  }

  private dropTransport(transport: WebTransport): void {
    const w = this.writer;
    this.writer = undefined;
    if (w) {
      try {
        w.releaseLock();
      } catch {
        // a pending write keeps the lock; the transport close below ends it
      }
    }
    this.outQueue.length = 0;
    this.writing = false;
    try {
      transport.close({ closeCode: 0, reason: this.closeReason ?? 'done' });
    } catch {
      // already closed
    }
  }
}
