/**
 * The native AURX media packet in the browser (`crates/aurix-common/src/protocol.rs`,
 * protocol v2), for the WebTransport media path. Pure functions over `Uint8Array` plus
 * WebCrypto for the keys; no DOM, so it runs (and is tested) in Node too.
 *
 * ```text
 * header(30) | AES-256-CTR(payload) | HMAC-SHA256(auth_key, header|ciphertext)[..16]
 * header: "AURX" | ver(1) | type(1) | flags(2) | seq(4) | ts(4) | ssrc(4) | ch_hash(4) | len(2) | crc32(4)
 * ```
 */

export const AURX_HEADER_SIZE = 30;
export const AURX_AUTH_TAG_SIZE = 16;
export const AURX_MAX_PACKET_SIZE = 1400;
export const AURX_PROTOCOL_VERSION = 2;
const MAGIC = [0x41, 0x55, 0x52, 0x58];

export const AurxPacketType = {
  Audio: 0x01,
  AudioFec: 0x02,
  Control: 0x10,
  Heartbeat: 0x20,
  HeartbeatAck: 0x21,
  SessionClose: 0x32,
  SessionBind: 0x33,
  SessionBindAck: 0x34,
  BitrateCommand: 0x71,
  Error: 0xff,
} as const;
export type AurxPacketType = (typeof AurxPacketType)[keyof typeof AurxPacketType];

export const AurxFlags = {
  Encrypted: 0x0001,
  Dtx: 0x0004,
  Fec: 0x0008,
  /** The frame is G.711 A-law (PCMA) instead of Opus. */
  Pcma: 0x0010,
  Priority: 0x0020,
  VolumeAttenuated: 0x0080,
  E2ee: 0x0100,
  Authenticated: 0x0400,
  Energy: 0x0800,
  Directional: 0x1000,
  /** The frame is G.711 μ-law (PCMU) instead of Opus. */
  Pcmu: 0x2000,
  Mixed: 0x4000,
} as const;

export interface AurxHeader {
  packetType: number;
  flags: number;
  sequence: number;
  timestamp: number;
  ssrc: number;
  channelIdHash: number;
}

export interface AurxPacket {
  header: AurxHeader;
  payload: Uint8Array;
  /** Present when the `Authenticated` flag is set (tag is verified by {@link AurxKeys.open}). */
  authTag: Uint8Array | undefined;
}

// ── CRC-32 (IEEE, as `crc32fast`) ──

const CRC_TABLE = (() => {
  const t = new Uint32Array(256);
  for (let n = 0; n < 256; n++) {
    let c = n;
    for (let k = 0; k < 8; k++) c = c & 1 ? 0xedb88320 ^ (c >>> 1) : c >>> 1;
    t[n] = c >>> 0;
  }
  return t;
})();

export function crc32(data: Uint8Array): number {
  let c = 0xffffffff;
  for (let i = 0; i < data.length; i++) c = CRC_TABLE[(c ^ data[i]!) & 0xff]! ^ (c >>> 8);
  return (c ^ 0xffffffff) >>> 0;
}

const UUID_RE = /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/i;

/** The 16 raw bytes of a UUID string. */
export function uuidBytes(id: string): Uint8Array {
  if (!UUID_RE.test(id)) throw new Error(`not a UUID: ${id}`);
  const hex = id.replace(/-/g, '');
  const out = new Uint8Array(16);
  for (let i = 0; i < 16; i++) out[i] = parseInt(hex.slice(i * 2, i * 2 + 2), 16);
  return out;
}

/** `channel_id_hash`: CRC-32 of the channel UUID's raw bytes. */
export function channelIdHash(channelId: string): number {
  return crc32(uuidBytes(channelId));
}

// ── Header ──

export function encodeHeader(h: AurxHeader, payloadLength: number, checksum: number, out: Uint8Array): void {
  const v = new DataView(out.buffer, out.byteOffset, out.byteLength);
  out.set(MAGIC, 0);
  out[4] = AURX_PROTOCOL_VERSION;
  out[5] = h.packetType & 0xff;
  v.setUint16(6, h.flags & 0xffff);
  v.setUint32(8, h.sequence >>> 0);
  v.setUint32(12, h.timestamp >>> 0);
  v.setUint32(16, h.ssrc >>> 0);
  v.setUint32(20, h.channelIdHash >>> 0);
  v.setUint16(24, payloadLength);
  v.setUint32(26, checksum >>> 0);
}

/**
 * Strict decoder (exact length, bounded size, CRC over the payload as received). Returns
 * `undefined` for anything malformed; the tag is extracted, not verified.
 */
export function decodePacket(data: Uint8Array): AurxPacket | undefined {
  if (data.length < AURX_HEADER_SIZE || data.length > AURX_MAX_PACKET_SIZE) return undefined;
  for (let i = 0; i < 4; i++) if (data[i] !== MAGIC[i]) return undefined;
  if (data[4] !== AURX_PROTOCOL_VERSION) return undefined;
  const v = new DataView(data.buffer, data.byteOffset, data.byteLength);
  const header: AurxHeader = {
    packetType: data[5]!,
    flags: v.getUint16(6),
    sequence: v.getUint32(8),
    timestamp: v.getUint32(12),
    ssrc: v.getUint32(16),
    channelIdHash: v.getUint32(20),
  };
  const payloadLength = v.getUint16(24);
  const checksum = v.getUint32(26);
  const authenticated = (header.flags & AurxFlags.Authenticated) !== 0;
  const expected = AURX_HEADER_SIZE + payloadLength + (authenticated ? AURX_AUTH_TAG_SIZE : 0);
  if (data.length !== expected) return undefined;
  const payload = data.subarray(AURX_HEADER_SIZE, AURX_HEADER_SIZE + payloadLength);
  if (crc32(payload) !== checksum) return undefined;
  const authTag = authenticated ? data.subarray(AURX_HEADER_SIZE + payloadLength) : undefined;
  return { header, payload, authTag };
}

// ── Keys ──

function subtle(): SubtleCrypto {
  const s = globalThis.crypto?.subtle;
  if (!s) throw new Error('WebCrypto (crypto.subtle) is unavailable; AURX needs a secure context');
  return s;
}

function ascii(label: string): Uint8Array {
  const out = new Uint8Array(label.length);
  for (let i = 0; i < label.length; i++) out[i] = label.charCodeAt(i) & 0x7f;
  return out;
}

async function hmacSha256(key: Uint8Array, data: Uint8Array): Promise<Uint8Array> {
  const k = await subtle().importKey('raw', key as Uint8Array<ArrayBuffer>, { name: 'HMAC', hash: 'SHA-256' }, false, ['sign']);
  return new Uint8Array(await subtle().sign('HMAC', k, data as Uint8Array<ArrayBuffer>));
}

/**
 * Per-session AURX keys (`MediaKeys` in `crypto.rs`):
 *
 * ```text
 * auth = HMAC-SHA256(master, "AURXv2 auth")
 * enc  = HMAC-SHA256(master, "AURXv2 enc")
 * salt = HMAC-SHA256(master, "AURXv2 salt")[0..16]
 * iv   = salt XOR (type(1) | 0(1) | ssrc(4) | seq(4) | ts(4) | 0(2))
 * ```
 */
export class AurxKeys {
  private constructor(
    private readonly auth: CryptoKey,
    private readonly enc: CryptoKey,
    private readonly salt: Uint8Array,
  ) {}

  static async derive(master: Uint8Array): Promise<AurxKeys> {
    if (master.length !== 32) throw new Error('AURX master key must be 32 bytes');
    const [auth, enc, saltFull] = await Promise.all([
      hmacSha256(master, ascii('AURXv2 auth')),
      hmacSha256(master, ascii('AURXv2 enc')),
      hmacSha256(master, ascii('AURXv2 salt')),
    ]);
    const s = subtle();
    const [authKey, encKey] = await Promise.all([
      s.importKey('raw', auth as Uint8Array<ArrayBuffer>, { name: 'HMAC', hash: 'SHA-256' }, false, ['sign']),
      s.importKey('raw', enc as Uint8Array<ArrayBuffer>, { name: 'AES-CTR' }, false, ['encrypt']),
    ]);
    return new AurxKeys(authKey, encKey, saltFull.slice(0, 16));
  }

  iv(h: AurxHeader): Uint8Array {
    const iv = new Uint8Array(16);
    const v = new DataView(iv.buffer);
    iv[0] = h.packetType & 0xff;
    v.setUint32(2, h.ssrc >>> 0);
    v.setUint32(6, h.sequence >>> 0);
    v.setUint32(10, h.timestamp >>> 0);
    for (let i = 0; i < 16; i++) iv[i] = iv[i]! ^ this.salt[i]!;
    return iv;
  }

  private async ctr(h: AurxHeader, data: Uint8Array): Promise<Uint8Array> {
    if (data.length === 0) return new Uint8Array(0);
    return new Uint8Array(
      await subtle().encrypt(
        { name: 'AES-CTR', counter: this.iv(h) as Uint8Array<ArrayBuffer>, length: 128 },
        this.enc,
        data as Uint8Array<ArrayBuffer>,
      ),
    );
  }

  private async tag(headerAndPayload: Uint8Array): Promise<Uint8Array> {
    const mac = new Uint8Array(await subtle().sign('HMAC', this.auth, headerAndPayload as Uint8Array<ArrayBuffer>));
    return mac.subarray(0, AURX_AUTH_TAG_SIZE);
  }

  /** Encrypt `payload` and append the tag (`Encrypted | Authenticated`). */
  async seal(header: AurxHeader, payload: Uint8Array): Promise<Uint8Array> {
    if (AURX_HEADER_SIZE + payload.length + AURX_AUTH_TAG_SIZE > AURX_MAX_PACKET_SIZE) throw new Error('AURX payload too large');
    const h: AurxHeader = { ...header, flags: header.flags | AurxFlags.Encrypted | AurxFlags.Authenticated };
    const out = new Uint8Array(AURX_HEADER_SIZE + payload.length + AURX_AUTH_TAG_SIZE);
    const cipher = await this.ctr(h, payload);
    out.set(cipher, AURX_HEADER_SIZE);
    encodeHeader(h, payload.length, crc32(cipher), out);
    out.set(await this.tag(out.subarray(0, AURX_HEADER_SIZE + payload.length)), AURX_HEADER_SIZE + payload.length);
    return out;
  }

  /** Sign without encrypting (`SessionBind`: the server reads the session id to find the key). */
  async signPlain(header: AurxHeader, payload: Uint8Array): Promise<Uint8Array> {
    const h: AurxHeader = { ...header, flags: (header.flags | AurxFlags.Authenticated) & ~AurxFlags.Encrypted };
    const out = new Uint8Array(AURX_HEADER_SIZE + payload.length + AURX_AUTH_TAG_SIZE);
    out.set(payload, AURX_HEADER_SIZE);
    encodeHeader(h, payload.length, crc32(payload), out);
    out.set(await this.tag(out.subarray(0, AURX_HEADER_SIZE + payload.length)), AURX_HEADER_SIZE + payload.length);
    return out;
  }

  /**
   * Verify the tag and decrypt a decoded packet. Returns the plaintext packet (flags without
   * `Encrypted`), or `undefined` when unauthenticated or the tag does not match.
   */
  async open(data: Uint8Array, packet: AurxPacket): Promise<AurxPacket | undefined> {
    if (!packet.authTag) return undefined;
    const signed = data.subarray(0, AURX_HEADER_SIZE + packet.payload.length);
    const expected = await this.tag(signed);
    let diff = 0;
    for (let i = 0; i < AURX_AUTH_TAG_SIZE; i++) diff |= expected[i]! ^ packet.authTag[i]!;
    if (diff !== 0) return undefined;
    if ((packet.header.flags & AurxFlags.Encrypted) === 0) return packet;
    const plain = await this.ctr(packet.header, packet.payload);
    return {
      header: { ...packet.header, flags: packet.header.flags & ~AurxFlags.Encrypted },
      payload: plain,
      authTag: packet.authTag,
    };
  }
}

// ── Packet builders ──

export function sessionBindHeader(ssrc: number, unixMs: number, nonce: number): AurxHeader {
  return {
    packetType: AurxPacketType.SessionBind,
    flags: 0,
    sequence: nonce >>> 0,
    timestamp: unixMs >>> 0,
    ssrc,
    channelIdHash: 0,
  };
}

/**
 * `SessionBind` payload: session_id (16) | unix_ms (8, BE) | nonce (8, BE). `nonce` is any
 * non-negative safe integer (the header carries its low 32 bits).
 */
export function sessionBindPayload(sessionId: string, unixMs: number, nonce: number): Uint8Array {
  if (!Number.isSafeInteger(nonce) || nonce < 0) throw new RangeError('SessionBind nonce must be a non-negative safe integer');
  if (!Number.isSafeInteger(unixMs)) throw new RangeError('SessionBind timestamp must be a safe integer');
  const out = new Uint8Array(32);
  out.set(uuidBytes(sessionId), 0);
  const v = new DataView(out.buffer);
  v.setBigInt64(16, BigInt(unixMs));
  v.setBigUint64(24, BigInt(nonce));
  return out;
}

export function heartbeatHeader(ssrc: number, sequence: number, timestamp: number): AurxHeader {
  return { packetType: AurxPacketType.Heartbeat, flags: 0, sequence, timestamp, ssrc, channelIdHash: 0 };
}

export function audioHeader(ssrc: number, sequence: number, timestamp: number, channelIdHash: number, flags: number): AurxHeader {
  return { packetType: AurxPacketType.Audio, flags, sequence, timestamp, ssrc, channelIdHash };
}

/** Wire `-dBov` level byte (RFC 6464) of a linear RMS energy; 127 = silence. */
export function audioLevelByte(energy: number): number {
  if (!Number.isFinite(energy) || energy <= 0) return 127;
  return Math.min(127, Math.max(0, Math.round(-20 * Math.log10(energy))));
}

/** Gain factor of a `VolumeAttenuated` byte (128 = unity). */
export function decodeVolumeByte(byte: number): number {
  return byte / 128;
}

export interface AurxDirection {
  /** Radians, positive = listener's right. */
  azimuth: number;
  /** Radians, positive = above. */
  elevation: number;
}

/** Two signed bytes of a `Directional` payload: azimuth in `π/127`, elevation in `π/254`. */
export function decodeDirection(az: number, el: number): AurxDirection {
  const sa = az > 127 ? az - 256 : az;
  const se = el > 127 ? el - 256 : el;
  return { azimuth: (sa * Math.PI) / 127, elevation: (se * (Math.PI / 2)) / 127 };
}

/** A downlink audio payload taken apart: gain byte, direction bytes, then the frame. */
export interface DownlinkAudio {
  ssrc: number;
  sequence: number;
  timestamp: number;
  channelIdHash: number;
  gain: number;
  direction: AurxDirection | undefined;
  e2ee: boolean;
  mixed: boolean;
  /**
   * Codec of `frame` (of the plaintext inside it when `e2ee`). A browser only ever sees G.711
   * on sealed frames of native participants on the PCMU / PCMA fallback; the node re-encodes
   * plaintext G.711 to Opus before it reaches a browser.
   */
  codec: DownlinkCodec;
  frame: Uint8Array;
}

export type DownlinkCodec = 'opus' | 'pcmu' | 'pcma';

/** The codec named by the `Pcmu` / `Pcma` flags of an audio header (Opus when neither). */
export function downlinkCodec(flags: number): DownlinkCodec {
  if (flags & AurxFlags.Pcmu) return 'pcmu';
  if (flags & AurxFlags.Pcma) return 'pcma';
  return 'opus';
}

export function parseDownlinkAudio(p: AurxPacket): DownlinkAudio | undefined {
  if (p.header.packetType !== AurxPacketType.Audio) return undefined;
  const f = p.header.flags;
  let off = 0;
  let gain = 1;
  let direction: AurxDirection | undefined;
  if (f & AurxFlags.VolumeAttenuated) {
    if (p.payload.length < 1) return undefined;
    gain = decodeVolumeByte(p.payload[0]!);
    off = 1;
  }
  if (f & AurxFlags.Directional) {
    if (p.payload.length < off + 2) return undefined;
    direction = decodeDirection(p.payload[off]!, p.payload[off + 1]!);
    off += 2;
  }
  return {
    ssrc: p.header.ssrc,
    sequence: p.header.sequence,
    timestamp: p.header.timestamp,
    channelIdHash: p.header.channelIdHash,
    gain,
    direction,
    e2ee: (f & AurxFlags.E2ee) !== 0,
    mixed: (f & AurxFlags.Mixed) !== 0,
    codec: downlinkCodec(f),
    frame: p.payload.subarray(off),
  };
}

/** Whether an Opus packet is stereo: bit 2 of the TOC byte (RFC 6716 §3.1). */
export function opusPacketIsStereo(frame: Uint8Array): boolean {
  return frame.length > 0 && (frame[0]! & 0x04) !== 0;
}

/**
 * Per-sender anti-replay window over 32-bit sequence numbers (the server keeps one per
 * session; the browser keeps one per downlink SSRC so a replayed or duplicated datagram is
 * not played twice).
 */
export class ReplayWindow {
  private highest: number | undefined;
  private bits = 0n;
  constructor(private readonly size = 128) {}

  /** `true` when `seq` is new (and now remembered). */
  accept(seq: number): boolean {
    seq >>>= 0;
    if (this.highest === undefined) {
      this.highest = seq;
      this.bits = 1n;
      return true;
    }
    const ahead = (seq - this.highest) | 0;
    if (ahead > 0) {
      this.bits = ahead >= this.size ? 1n : ((this.bits << BigInt(ahead)) | 1n) & ((1n << BigInt(this.size)) - 1n);
      this.highest = seq;
      return true;
    }
    const behind = -ahead;
    if (behind >= this.size) return false;
    const mask = 1n << BigInt(behind);
    if (this.bits & mask) return false;
    this.bits |= mask;
    return true;
  }

  reset(): void {
    this.highest = undefined;
    this.bits = 0n;
  }
}

const RESYNC_GAP = 1000;

/**
 * Loss estimate from sequence gaps of one downlink SSRC (for `QualityReport`): a gap counts
 * as lost when first seen and is credited back when the late packet arrives.
 */
export class SequenceLoss {
  /** Highest sequence seen and the count of distinct sequences the current run spans. */
  private highest: number | undefined;
  private extent = 0;
  received = 0;
  /** Sequences expected but never seen (`extent - received`, late arrivals fill the gap). */
  lost = 0;

  observe(seq: number): void {
    seq >>>= 0;
    this.received += 1;
    if (this.highest === undefined) {
      this.highest = seq;
      this.extent = 1;
    } else {
      const ahead = (seq - this.highest) | 0;
      if (ahead >= RESYNC_GAP || ahead <= -RESYNC_GAP) {
        // A jump too large for loss: the sender restarted its sequence.
        this.highest = seq;
        this.extent = this.received;
      } else if (ahead > 0) {
        this.highest = seq;
        this.extent += ahead;
      }
    }
    this.lost = Math.max(0, this.extent - this.received);
  }

  reset(): void {
    this.highest = undefined;
    this.extent = 0;
    this.received = 0;
    this.lost = 0;
  }
}
