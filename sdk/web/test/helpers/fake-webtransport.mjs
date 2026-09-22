import { AurxFlags, AurxKeys, AurxPacketType, decodePacket, parseDownlinkAudio } from '../../dist/aurx.js';
import { FakeAudioContext, FakeAudioWorkletNode } from './fake-audio.mjs';

// ── WebCodecs stand-ins ──

/** Opus "encoder": one byte of TOC (stereo bit from the config) followed by the frame's peak level. */
export class FakeAudioEncoder {
  static all = [];
  static supported = true;
  constructor({ output, error }) {
    this.output = output;
    this.error = error;
    this.state = 'unconfigured';
    this.config = undefined;
    this.configs = [];
    this.encodeQueueSize = 0;
    this.encoded = 0;
    FakeAudioEncoder.all.push(this);
  }
  static async isConfigSupported(config) {
    return { supported: FakeAudioEncoder.supported, config };
  }
  configure(config) {
    if (this.state === 'closed') throw new DOMException('closed', 'InvalidStateError');
    this.config = config;
    this.configs.push(config);
    this.state = 'configured';
  }
  encode(audio) {
    if (this.state !== 'configured') throw new DOMException('not configured', 'InvalidStateError');
    const pcm = new Float32Array(audio.numberOfFrames);
    audio.copyTo(pcm, { planeIndex: 0, format: 'f32-planar' });
    let peak = 0;
    for (const s of pcm) peak = Math.max(peak, Math.abs(s));
    const toc = 0x78 | (audio.numberOfChannels === 2 ? 0x04 : 0);
    const data = new Uint8Array([toc, Math.round(peak * 255), this.encoded & 0xff]);
    this.encoded += 1;
    this.output(new FakeEncodedAudioChunk({ type: 'key', timestamp: audio.timestamp, data }));
  }
  async flush() {}
  close() {
    this.state = 'closed';
  }
}

/** Opus "decoder": a 960-sample frame at the packet's peak level (second byte), mono or stereo per TOC. */
export class FakeAudioDecoder {
  static all = [];
  constructor({ output, error }) {
    this.output = output;
    this.error = error;
    this.state = 'unconfigured';
    this.config = undefined;
    this.decodeQueueSize = 0;
    this.decoded = [];
    FakeAudioDecoder.all.push(this);
  }
  static async isConfigSupported(config) {
    return { supported: true, config };
  }
  configure(config) {
    if (this.state === 'closed') throw new DOMException('closed', 'InvalidStateError');
    this.config = config;
    this.state = 'configured';
  }
  decode(chunk) {
    if (this.state !== 'configured') throw new DOMException('not configured', 'InvalidStateError');
    const bytes = new Uint8Array(chunk.byteLength);
    chunk.copyTo(bytes);
    this.decoded.push(bytes);
    const channels = this.config.numberOfChannels;
    const level = (bytes[1] ?? 0) / 255;
    const data = new Float32Array(960 * channels).fill(level);
    this.output(
      new FakeAudioData({ format: 'f32-planar', sampleRate: 48000, numberOfFrames: 960, numberOfChannels: channels, timestamp: chunk.timestamp, data }),
    );
  }
  async flush() {}
  close() {
    this.state = 'closed';
  }
}

export class FakeAudioData {
  constructor({ format, sampleRate, numberOfFrames, numberOfChannels, timestamp, data }) {
    this.format = format;
    this.sampleRate = sampleRate;
    this.numberOfFrames = numberOfFrames;
    this.numberOfChannels = numberOfChannels;
    this.timestamp = timestamp;
    this.data = Float32Array.from(data);
    this.closed = false;
  }
  allocationSize({ planeIndex }) {
    if (planeIndex >= this.numberOfChannels) throw new RangeError('plane');
    return this.numberOfFrames * 4;
  }
  copyTo(dest, { planeIndex = 0 } = {}) {
    if (this.closed) throw new DOMException('closed', 'InvalidStateError');
    const view = dest instanceof Float32Array ? dest : new Float32Array(dest.buffer, dest.byteOffset, dest.byteLength / 4);
    view.set(this.data.subarray(planeIndex * this.numberOfFrames, (planeIndex + 1) * this.numberOfFrames));
  }
  close() {
    this.closed = true;
  }
}

export class FakeEncodedAudioChunk {
  constructor({ type, timestamp, data }) {
    this.type = type;
    this.timestamp = timestamp;
    this.data = Uint8Array.from(data);
    this.byteLength = this.data.length;
  }
  copyTo(dest) {
    new Uint8Array(dest.buffer ?? dest, dest.byteOffset ?? 0).set(this.data);
  }
}

// ── WebTransport stand-in ──

/**
 * `WebTransport` with datagram streams. `FakeWebTransport.behaviour(url)` decides per URL:
 * `'refuse'` rejects `ready`, `'hang'` never settles it, anything else connects and hands the
 * datagrams to `FakeWebTransport.node` (a {@link FakeNode}) when one is set.
 */
export class FakeWebTransport {
  static all = [];
  static behaviour = () => 'accept';
  static node = undefined;
  static maxDatagramSize = 1200;
  constructor(url, options = {}) {
    this.url = url;
    this.options = options;
    this.sent = [];
    this.closedWith = undefined;
    let resolveReady;
    let rejectReady;
    this.ready = new Promise((res, rej) => {
      resolveReady = res;
      rejectReady = rej;
    });
    this.ready.catch(() => undefined);
    let resolveClosed;
    let rejectClosed;
    this.closed = new Promise((res, rej) => {
      resolveClosed = res;
      rejectClosed = rej;
    });
    this.closed.catch(() => undefined);
    this.settleClosed = resolveClosed;
    this.failClosed = rejectClosed;
    const transport = this;
    this.readable = new ReadableStream({
      start(controller) {
        transport.inbound = controller;
      },
    });
    this.datagramsValue = {
      maxDatagramSize: FakeWebTransport.maxDatagramSize,
      readable: this.readable,
      writable: new WritableStream({
        async write(chunk) {
          const copy = Uint8Array.from(chunk);
          transport.sent.push(copy);
          transport.onsent?.(copy);
          await FakeWebTransport.node?.onDatagram(transport, copy);
        },
      }),
    };
    FakeWebTransport.all.push(this);
    const mode = FakeWebTransport.behaviour(url);
    if (mode === 'refuse') queueMicrotask(() => rejectReady(new Error(`refused ${url}`)));
    else if (mode !== 'hang') queueMicrotask(() => resolveReady(undefined));
  }
  /** On the prototype like the real API (`'datagrams' in WebTransport.prototype` is the feature probe). */
  get datagrams() {
    return this.datagramsValue;
  }
  /** A datagram from the "server". */
  deliver(bytes) {
    if (this.closedWith !== undefined) return;
    this.inbound.enqueue(Uint8Array.from(bytes));
  }
  /** The server (or network) ends the session. */
  drop(reason = 'lost') {
    if (this.closedWith !== undefined) return;
    this.closedWith = { closeCode: 0, reason };
    this.failClosed(new Error(reason));
    try {
      this.inbound.error(new Error(reason));
    } catch {
      // already closed
    }
  }
  close(info = {}) {
    if (this.closedWith !== undefined) return;
    this.closedWith = info;
    this.settleClosed(info);
    try {
      this.inbound.close();
    } catch {
      // already closed
    }
  }
}

/**
 * The node side of one AURX session: opens what the browser sends, acknowledges `SessionBind`
 * and heartbeats, keeps the uplink audio, and can seal downlink packets for the browser.
 */
export class FakeNode {
  constructor(masterKey, ssrc, { ackBind = true } = {}) {
    this.masterKey = masterKey;
    this.ssrc = ssrc;
    this.ackBind = ackBind;
    this.keys = undefined;
    this.binds = [];
    this.heartbeats = [];
    this.audio = [];
    this.rejected = 0;
    this.sequence = 0;
    this.bound = undefined;
  }
  async ready() {
    if (!this.keys) this.keys = await AurxKeys.derive(this.masterKey);
    return this.keys;
  }
  async onDatagram(transport, data) {
    const keys = await this.ready();
    const decoded = decodePacket(data);
    const packet = decoded && (await keys.open(data, decoded));
    if (!packet) {
      this.rejected += 1;
      return;
    }
    switch (packet.header.packetType) {
      case AurxPacketType.SessionBind:
        this.binds.push({ transport, nonce: packet.header.sequence, payload: packet.payload });
        if (this.ackBind) {
          this.bound = transport;
          transport.deliver(await this.seal({ packetType: AurxPacketType.SessionBindAck, flags: 0, sequence: this.nextSequence(), timestamp: 0, ssrc: this.ssrc, channelIdHash: 0 }, new Uint8Array(0)));
        }
        return;
      case AurxPacketType.Heartbeat:
        this.heartbeats.push(packet.header.timestamp);
        transport.deliver(
          await this.seal({ packetType: AurxPacketType.HeartbeatAck, flags: 0, sequence: this.nextSequence(), timestamp: packet.header.timestamp, ssrc: this.ssrc, channelIdHash: 0 }, new Uint8Array(0)),
        );
        return;
      case AurxPacketType.Audio: {
        const f = packet.header.flags;
        const energy = (f & AurxFlags.Energy) !== 0;
        this.audio.push({
          sequence: packet.header.sequence,
          timestamp: packet.header.timestamp,
          channelIdHash: packet.header.channelIdHash,
          e2ee: (f & AurxFlags.E2ee) !== 0,
          level: energy ? packet.payload[0] : undefined,
          frame: packet.payload.subarray(energy ? 1 : 0),
        });
        return;
      }
      default:
        return;
    }
  }
  nextSequence() {
    this.sequence = (this.sequence + 1) >>> 0;
    return this.sequence;
  }
  async seal(header, payload) {
    return (await this.ready()).seal(header, payload);
  }
  /** A sealed downlink audio datagram from `ssrc` (server gain/direction bytes optional). */
  async audioPacket(ssrc, sequence, frame, { channelIdHash = 0, timestamp = sequence * 960, gain, direction, e2ee = false, mixed = false, pcmu = false, pcma = false } = {}) {
    let flags = 0;
    const parts = [];
    if (gain !== undefined) {
      flags |= AurxFlags.VolumeAttenuated;
      parts.push(Math.max(0, Math.min(255, Math.round(gain * 128))));
    }
    if (direction) {
      flags |= AurxFlags.Directional;
      parts.push((Math.round((direction.azimuth * 127) / Math.PI) + 256) & 0xff, (Math.round((direction.elevation * 127) / (Math.PI / 2)) + 256) & 0xff);
    }
    if (e2ee) flags |= AurxFlags.E2ee;
    if (mixed) flags |= AurxFlags.Mixed;
    if (pcmu) flags |= AurxFlags.Pcmu;
    if (pcma) flags |= AurxFlags.Pcma;
    const payload = new Uint8Array(parts.length + frame.length);
    payload.set(parts, 0);
    payload.set(frame, parts.length);
    return this.seal({ packetType: AurxPacketType.Audio, flags, sequence, timestamp, ssrc, channelIdHash }, payload);
  }
  async bitrateCommand(bps) {
    const payload = new Uint8Array(4);
    new DataView(payload.buffer).setUint32(0, bps);
    return this.seal({ packetType: AurxPacketType.BitrateCommand, flags: 0, sequence: this.nextSequence(), timestamp: 0, ssrc: this.ssrc, channelIdHash: 0 }, payload);
  }
  async sessionClose() {
    return this.seal({ packetType: AurxPacketType.SessionClose, flags: 0, sequence: this.nextSequence(), timestamp: 0, ssrc: this.ssrc, channelIdHash: 0 }, new Uint8Array(0));
  }
}

/** Decode what a browser sent as downlink-shaped audio (for symmetric assertions). */
export function downlinkOf(packet) {
  return parseDownlinkAudio(packet);
}

/** Install the browser globals AURX over WebTransport needs (WebCodecs, Web Audio, WebTransport). */
export function installWebTransportGlobals() {
  globalThis.WebTransport = FakeWebTransport;
  globalThis.AudioEncoder = FakeAudioEncoder;
  globalThis.AudioDecoder = FakeAudioDecoder;
  globalThis.AudioData = FakeAudioData;
  globalThis.EncodedAudioChunk = FakeEncodedAudioChunk;
  globalThis.AudioContext = FakeAudioContext;
  globalThis.AudioWorkletNode = FakeAudioWorkletNode;
}

export function resetWebTransportFakes() {
  FakeWebTransport.all.length = 0;
  FakeWebTransport.behaviour = () => 'accept';
  FakeWebTransport.node = undefined;
  FakeAudioEncoder.all.length = 0;
  FakeAudioDecoder.all.length = 0;
}

/** Wait for pending microtasks / WebCrypto promises to drain. */
export const settle = async (rounds = 5) => {
  for (let i = 0; i < rounds; i++) await new Promise((r) => setTimeout(r, 0));
};

/** Poll `predicate` until true or `ms` elapsed. */
export async function until(predicate, ms = 2000, what = 'condition') {
  const deadline = Date.now() + ms;
  while (!predicate()) {
    if (Date.now() > deadline) throw new Error(`timed out waiting for ${what}`);
    await new Promise((r) => setTimeout(r, 2));
  }
}
