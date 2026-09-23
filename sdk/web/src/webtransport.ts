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

import { AurxLink, timeout, type AurxLinkEvents, type AurxLinkOptions, type AurxLinkStats, type AurxSessionKeys } from './aurx-link.js';
import type { WebTransportInfoWire } from './protocol.js';

export type WebTransportSessionKeys = AurxSessionKeys;

export interface AurxWebTransportOptions extends AurxLinkOptions {
  /** Test seam: the `WebTransport` constructor to use. */
  webTransport?: WebTransportConstructor;
}

export type WebTransportConstructor = new (url: string, options?: WebTransportOptions) => WebTransport;

export type AurxWebTransportStats = AurxLinkStats;

export type AurxWebTransportEvents = AurxLinkEvents;

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

/** Datagrams the browser may hold in its outgoing queue before the newest are dropped. */
const MAX_WRITE_BACKLOG = 64;
/** `reader.read()` calls kept in flight so a stalled main thread drains a backlog in one task. */
const READS_IN_FLIGHT = 16;
/**
 * Datagrams the browser may hand to its network layer before `write()` waits for one to leave.
 * Chromium's default is 1: every further write then costs a renderer↔network round trip, i.e.
 * one main-thread task per packet, which caps a page with a heavy game loop at a few dozen
 * packets per second and queues heartbeats behind seconds of audio.
 */
const OUTGOING_BUFFERED_DATAGRAMS = 64;
/** Datagrams the browser keeps for us while the main thread is busy (Chromium default: 1). */
const INCOMING_BUFFERED_DATAGRAMS = 256;
/** Milliseconds after which a datagram still waiting in either browser queue is discarded. */
const DATAGRAM_MAX_AGE_MS = 500;

type DatagramQueueTuning = Partial<{
  outgoingMaxBufferedDatagrams: number;
  incomingMaxBufferedDatagrams: number;
  outgoingHighWaterMark: number;
  incomingHighWaterMark: number;
  outgoingMaxAge: number | null;
  incomingMaxAge: number | null;
}>;

/** Best effort: browsers differ in which of the (renamed) attributes they expose and accept. */
function tuneDatagramQueues(transport: WebTransport): void {
  const datagrams = transport.datagrams as WebTransportDatagramDuplexStream & DatagramQueueTuning;
  const set = (apply: () => void): void => {
    try {
      apply();
    } catch {
      // attribute missing or read-only in this browser
    }
  };
  set(() => {
    if ('outgoingMaxBufferedDatagrams' in datagrams) datagrams.outgoingMaxBufferedDatagrams = OUTGOING_BUFFERED_DATAGRAMS;
    else datagrams.outgoingHighWaterMark = OUTGOING_BUFFERED_DATAGRAMS;
  });
  set(() => {
    if ('incomingMaxBufferedDatagrams' in datagrams) datagrams.incomingMaxBufferedDatagrams = INCOMING_BUFFERED_DATAGRAMS;
    else datagrams.incomingHighWaterMark = INCOMING_BUFFERED_DATAGRAMS;
  });
  set(() => {
    datagrams.outgoingMaxAge = DATAGRAM_MAX_AGE_MS;
  });
  set(() => {
    datagrams.incomingMaxAge = DATAGRAM_MAX_AGE_MS;
  });
}

/** One WebTransport session carrying one AURX media session. */
export class AurxWebTransport extends AurxLink {
  private transport: WebTransport | undefined;
  private writer: WritableStreamDefaultWriter<Uint8Array> | undefined;
  private url: string | undefined;

  constructor(session: WebTransportSessionKeys, events: AurxWebTransportEvents, private readonly wtOptions: AurxWebTransportOptions = {}) {
    super(session, events, wtOptions);
  }

  get connectedUrl(): string | undefined {
    return this.url;
  }

  /**
   * Try the advertised URLs in order; resolves once the node acknowledged our `SessionBind`
   * over the new path (the session now receives its downlink here).
   */
  async connect(info: WebTransportInfoWire): Promise<void> {
    if (this.state !== 'idle') throw new Error(`WebTransport connect in state ${this.state}`);
    this.state = 'connecting';
    const ctor = this.wtOptions.webTransport ?? (globalThis as { WebTransport?: WebTransportConstructor }).WebTransport;
    if (typeof ctor !== 'function') {
      this.state = 'closed';
      throw new Error('WebTransport is not available in this browser');
    }
    if (info.urls.length === 0) {
      this.state = 'closed';
      throw new Error('the node advertises no WebTransport URL');
    }
    await this.deriveKeys();
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
        tuneDatagramQueues(transport);
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
        this.abortBind();
        this.transport = undefined;
        this.url = undefined;
        this.dropTransport(transport);
        if (this.state !== 'connecting') break;
      }
    }
    this.state = 'closed';
    throw new Error(`WebTransport media path unavailable: ${errors.join('; ')}`);
  }

  protected carrierMaxPacketSize(): number | undefined {
    return this.transport?.datagrams.maxDatagramSize;
  }

  /**
   * Hands one datagram to the browser without waiting for it to leave: awaiting each `write()` costs
   * a main-thread task per packet, which caps the uplink at one packet per render frame when the
   * page runs a heavy game loop. `desiredSize` bounds the browser-side backlog instead.
   */
  protected carrierWrite(packet: Uint8Array): boolean {
    const writer = this.writer;
    if (!writer) return false;
    const desired = writer.desiredSize;
    if (desired !== null && desired <= -MAX_WRITE_BACKLOG) return false;
    writer.write(packet as Uint8Array<ArrayBuffer>).catch((e: unknown) => {
      if (this.state === 'open' && this.writer === writer) this.events.onError(e instanceof Error ? e : new Error(String(e)));
    });
    return true;
  }

  protected carrierClose(): void {
    const t = this.transport;
    this.transport = undefined;
    if (t) this.dropTransport(t);
  }

  /**
   * Keeps several `read()` calls pending at once: each resolution is a main-thread task, so a
   * single outstanding read would cap the downlink at one datagram per render frame.
   */
  private async readLoop(transport: WebTransport): Promise<void> {
    const reader = transport.datagrams.readable.getReader();
    let failed = false;
    const pump = async (): Promise<void> => {
      for (;;) {
        const { value, done } = await reader.read();
        if (done || this.transport !== transport) return;
        if (value instanceof Uint8Array) this.onCarrierPacket(value);
      }
    };
    try {
      await Promise.all(
        Array.from({ length: READS_IN_FLIGHT }, () =>
          pump().catch((e: unknown) => {
            if (failed) return;
            failed = true;
            if (this.transport === transport) this.onTransportClosed(transport, `read failed (${e instanceof Error ? e.message : String(e)})`);
          }),
        ),
      );
    } finally {
      try {
        reader.releaseLock();
      } catch {
        // stream already gone
      }
    }
  }

  private onTransportClosed(transport: WebTransport, reason: string): void {
    if (this.transport !== transport) return;
    this.fail(reason);
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
    try {
      transport.close({ closeCode: 0, reason: this.closeReason ?? 'done' });
    } catch {
      // already closed
    }
  }
}
