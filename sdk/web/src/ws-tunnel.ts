/**
 * AURX over the control WebSocket: the media path of last resort. The node accepts sealed AURX
 * packets as binary frames on the signalling connection itself (`SessionInitAck.media_tunnel`),
 * so voice gets through wherever the WebSocket did — HTTP-only reverse proxies, corporate
 * proxies, tunnels that carry no UDP. The price is TCP head-of-line blocking: one lost segment
 * stalls every frame behind it, so `webtransport` and `webrtc` are always preferred when they
 * can connect. Authentication, replay protection, E2EE and packet limits are AURX's, exactly as
 * on the native SDKs' WebSocket link.
 */

import { AurxLink, timeout, type AurxLinkEvents, type AurxLinkOptions, type AurxLinkStats, type AurxSessionKeys } from './aurx-link.js';
import { AURX_MAX_PACKET_SIZE } from './aurx.js';

export type AurxWebSocketTunnelOptions = AurxLinkOptions;
export type AurxWebSocketTunnelStats = AurxLinkStats;
export type AurxWebSocketTunnelEvents = AurxLinkEvents;

/** The socket the tunnel writes to; `WebSocket` satisfies it. */
export interface TunnelSocket {
  readonly url: string;
  readonly bufferedAmount: number;
  readonly readyState: number;
  send(data: ArrayBufferView): void;
}

const WS_OPEN = 1;
/**
 * Bytes the browser may still hold for this socket before new media frames are dropped rather
 * than queued: on a stalled TCP connection a queue only adds delay to every frame behind it.
 * 64 KiB is about a second of a 100 kbit/s stereo uplink plus control traffic.
 */
const MAX_SOCKET_BACKLOG = 64 * 1024;

/** One AURX media session carried as binary frames on an already open WebSocket. */
export class AurxWebSocketTunnel extends AurxLink {
  private socket: TunnelSocket | undefined;

  constructor(
    session: AurxSessionKeys,
    events: AurxWebSocketTunnelEvents,
    private readonly carrier: TunnelSocket,
    options: AurxWebSocketTunnelOptions = {},
  ) {
    super(session, events, options);
  }

  get connectedUrl(): string | undefined {
    return this.socket?.url;
  }

  /** Binds the session over the socket; resolves once the node acknowledged the `SessionBind`. */
  async connect(): Promise<void> {
    if (this.state !== 'idle') throw new Error(`WebSocket tunnel connect in state ${this.state}`);
    this.state = 'connecting';
    if (this.carrier.readyState !== WS_OPEN) {
      this.state = 'closed';
      throw new Error('the control WebSocket is not open');
    }
    await this.deriveKeys();
    this.socket = this.carrier;
    const budget = this.options.connectTimeoutMs ?? 6000;
    try {
      await timeout(this.bind(), budget, 'SessionBind over the WebSocket tunnel');
      if (this.state !== 'connecting') throw new Error(this.closeReason ?? 'closed while binding');
    } catch (e) {
      this.abortBind();
      this.socket = undefined;
      this.state = 'closed';
      throw new Error(`WebSocket media tunnel unavailable: ${e instanceof Error ? e.message : String(e)}`);
    }
    this.state = 'open';
    this.startHeartbeats();
  }

  /** Feed one binary frame received on the socket (the owner demultiplexes text from binary). */
  onFrame(data: ArrayBuffer | Uint8Array): void {
    if (this.state === 'closed') return;
    this.onCarrierPacket(data instanceof Uint8Array ? data : new Uint8Array(data));
  }

  /** The socket went away underneath us: the path is gone (the owner reconnects the session). */
  socketClosed(reason: string): void {
    this.fail(reason);
  }

  protected carrierMaxPacketSize(): number | undefined {
    return AURX_MAX_PACKET_SIZE;
  }

  protected carrierWrite(packet: Uint8Array): boolean {
    const socket = this.socket;
    if (!socket || socket.readyState !== WS_OPEN) return false;
    if (socket.bufferedAmount > MAX_SOCKET_BACKLOG) return false;
    try {
      socket.send(packet);
    } catch (e) {
      if (this.state === 'open') this.events.onError(e instanceof Error ? e : new Error(String(e)));
      return false;
    }
    return true;
  }

  protected carrierClose(): void {
    // The socket belongs to the control channel and stays open; only stop using it.
    this.socket = undefined;
  }
}
