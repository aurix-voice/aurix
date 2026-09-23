/**
 * Group end-to-end encryption of voice frames ("Aurix E2EE v1"), the browser side of
 * `crates/aurix-common/src/e2ee.rs` — same key schedule, wire formats and rotation rules, so
 * browsers and native clients decrypt each other.
 *
 * ```text
 * frame  = generation(1) | counter(4, BE) | AES-256-CTR(enc, IV, opus) | HMAC-SHA256(auth, header|ct)[..10]
 * IV     = (salt(12) XOR (generation | counter | 0*7)) | 0*4
 * enc / auth / salt = HKDF-SHA256(secret, "aurix-e2ee-v1 enc" / "… auth" / "… salt")
 * wrap   = nonce(12) | AES-256-CTR(wk_enc, nonce|0*4, secret) | HMAC-SHA256(wk_auth, generation|nonce|ct)[..16]
 * wk_*   = HKDF-SHA256(X25519(sender_sk, recipient_pk), salt = sender_pk|recipient_pk, "aurix-e2ee-v1 wrap enc" / "… wrap auth")
 * ```
 *
 * Symmetric primitives come from WebCrypto; X25519 is a small RFC 7748 implementation on
 * `BigInt` (WebCrypto X25519 is not available in every engine and raw-key import differs
 * between the ones that have it). It runs only when keys are exchanged, never per frame.
 *
 * The classes between `WORKER_UNITS_BEGIN` and `WORKER_UNITS_END` are also serialised
 * (`Function.prototype.toString`) into the worker that backs `RTCRtpScriptTransform`, so they
 * must be self-contained: no imports, no references to module-level bindings other than
 * `E2EE` (inlined as JSON) and each other.
 */

const E2EE = {
  VERSION: 1,
  PUBLIC_KEY_LEN: 32,
  SECRET_LEN: 32,
  FRAME_HEADER_LEN: 5,
  FRAME_TAG_LEN: 10,
  /** Bytes an encrypted frame is longer than the Opus frame inside it. */
  FRAME_OVERHEAD: 15,
  WRAP_NONCE_LEN: 12,
  WRAP_TAG_LEN: 16,
  WRAPPED_KEY_LEN: 60,
  /** Generations of one peer a receiver keeps decrypting. */
  KEPT_GENERATIONS: 4,
  /** A sender rotates before its frame counter gets anywhere near wrapping. */
  ROTATE_AT_COUNTER: 2 ** 31,
  /** Out-of-order tolerance of the per-generation replay window. */
  REPLAY_WINDOW: 128,
} as const;

/** Protocol constants (see the module docs). */
export const E2EE_CONSTANTS: Readonly<typeof E2EE> = E2EE;

export class E2eeError extends Error {
  constructor(message: string) {
    super(message);
    this.name = 'E2eeError';
  }
}

// ── Encoding helpers ─────────────────────────────────────────────────────────────────────

export function bytesToBase64(bytes: Uint8Array): string {
  let bin = '';
  for (const b of bytes) bin += String.fromCharCode(b);
  return btoa(bin);
}

export function base64ToBytes(b64: string): Uint8Array {
  const bin = atob(b64);
  const out = new Uint8Array(bin.length);
  for (let i = 0; i < bin.length; i++) out[i] = bin.charCodeAt(i);
  return out;
}

export function bytesToHex(bytes: Uint8Array): string {
  let s = '';
  for (const b of bytes) s += b.toString(16).padStart(2, '0');
  return s;
}

export function hexToBytes(hex: string): Uint8Array {
  if (hex.length % 2 !== 0 || /[^0-9a-fA-F]/.test(hex)) throw new E2eeError('invalid hex');
  const out = new Uint8Array(hex.length / 2);
  for (let i = 0; i < out.length; i++) out[i] = parseInt(hex.slice(i * 2, i * 2 + 2), 16);
  return out;
}

function concat(...parts: Uint8Array[]): Uint8Array {
  let n = 0;
  for (const p of parts) n += p.length;
  const out = new Uint8Array(n);
  let o = 0;
  for (const p of parts) {
    out.set(p, o);
    o += p.length;
  }
  return out;
}

function subtleOrThrow(): SubtleCrypto {
  const s = globalThis.crypto?.subtle;
  if (!s) throw new E2eeError('WebCrypto (crypto.subtle) is unavailable; E2EE needs a secure context');
  return s;
}

function randomBytes(n: number): Uint8Array {
  const out = new Uint8Array(n);
  if (!globalThis.crypto?.getRandomValues) throw new E2eeError('crypto.getRandomValues is unavailable');
  globalThis.crypto.getRandomValues(out);
  return out;
}

// ── Primitives ───────────────────────────────────────────────────────────────────────────

/** HKDF-SHA256 (RFC 5869). */
export async function hkdfSha256(salt: Uint8Array, ikm: Uint8Array, info: Uint8Array, len: number): Promise<Uint8Array> {
  const subtle = subtleOrThrow();
  const key = await subtle.importKey('raw', ikm as Uint8Array<ArrayBuffer>, 'HKDF', false, ['deriveBits']);
  const bits = await subtle.deriveBits({ name: 'HKDF', hash: 'SHA-256', salt: salt as Uint8Array<ArrayBuffer>, info: info as Uint8Array<ArrayBuffer> }, key, len * 8);
  return new Uint8Array(bits);
}

export async function sha256(data: Uint8Array): Promise<Uint8Array> {
  return new Uint8Array(await subtleOrThrow().digest('SHA-256', data as Uint8Array<ArrayBuffer>));
}

async function hmacSha256(key: Uint8Array, data: Uint8Array): Promise<Uint8Array> {
  const subtle = subtleOrThrow();
  const k = await subtle.importKey('raw', key as Uint8Array<ArrayBuffer>, { name: 'HMAC', hash: 'SHA-256' }, false, ['sign']);
  return new Uint8Array(await subtle.sign('HMAC', k, data as Uint8Array<ArrayBuffer>));
}

async function aes256Ctr(key: Uint8Array, iv: Uint8Array, data: Uint8Array): Promise<Uint8Array> {
  if (data.length === 0) return new Uint8Array(0);
  const subtle = subtleOrThrow();
  const k = await subtle.importKey('raw', key as Uint8Array<ArrayBuffer>, { name: 'AES-CTR' }, false, ['encrypt']);
  return new Uint8Array(await subtle.encrypt({ name: 'AES-CTR', counter: iv as Uint8Array<ArrayBuffer>, length: 128 }, k, data as Uint8Array<ArrayBuffer>));
}

/** Hex SHA-256 of an identity public key, the value users compare out of band. */
export async function e2eeFingerprint(publicKey: Uint8Array): Promise<string> {
  return bytesToHex(await sha256(publicKey));
}

// ── X25519 (RFC 7748) ────────────────────────────────────────────────────────────────────

const P25519 = (1n << 255n) - 19n;
const A24 = 121665n;

function decodeLittleEndian(b: Uint8Array): bigint {
  let x = 0n;
  for (let i = b.length - 1; i >= 0; i--) x = (x << 8n) | BigInt(b[i] ?? 0);
  return x;
}

function encodeLittleEndian(x: bigint): Uint8Array {
  const out = new Uint8Array(32);
  for (let i = 0; i < 32; i++) {
    out[i] = Number(x & 0xffn);
    x >>= 8n;
  }
  return out;
}

function mod(a: bigint): bigint {
  const r = a % P25519;
  return r < 0n ? r + P25519 : r;
}

function powMod(base: bigint, exp: bigint): bigint {
  let result = 1n;
  let b = mod(base);
  let e = exp;
  while (e > 0n) {
    if (e & 1n) result = (result * b) % P25519;
    b = (b * b) % P25519;
    e >>= 1n;
  }
  return result;
}

/** `scalar * u` on Curve25519; throws on a low-order `u` (all-zero shared secret). */
export function x25519(scalar: Uint8Array, u: Uint8Array): Uint8Array {
  if (scalar.length !== 32 || u.length !== 32) throw new E2eeError('X25519 inputs must be 32 bytes');
  const k = new Uint8Array(scalar);
  k[0] = (k[0] ?? 0) & 248;
  k[31] = ((k[31] ?? 0) & 127) | 64;
  const uc = new Uint8Array(u);
  uc[31] = (uc[31] ?? 0) & 127;
  const kn = decodeLittleEndian(k);
  const x1 = mod(decodeLittleEndian(uc));
  let x2 = 1n;
  let z2 = 0n;
  let x3 = x1;
  let z3 = 1n;
  let swap = 0n;
  for (let t = 254; t >= 0; t--) {
    const kt = (kn >> BigInt(t)) & 1n;
    swap ^= kt;
    if (swap === 1n) {
      [x2, x3] = [x3, x2];
      [z2, z3] = [z3, z2];
    }
    swap = kt;
    const a = mod(x2 + z2);
    const aa = (a * a) % P25519;
    const b = mod(x2 - z2);
    const bb = (b * b) % P25519;
    const e = mod(aa - bb);
    const c = mod(x3 + z3);
    const d = mod(x3 - z3);
    const da = (d * a) % P25519;
    const cb = (c * b) % P25519;
    const s = mod(da + cb);
    x3 = (s * s) % P25519;
    const df = mod(da - cb);
    z3 = (x1 * ((df * df) % P25519)) % P25519;
    x2 = (aa * bb) % P25519;
    z2 = (e * mod(aa + A24 * e)) % P25519;
  }
  if (swap === 1n) {
    [x2, x3] = [x3, x2];
    [z2, z3] = [z3, z2];
  }
  const out = encodeLittleEndian((x2 * powMod(z2, P25519 - 2n)) % P25519);
  if (out.every((b) => b === 0)) throw new E2eeError('low-order X25519 public key');
  return out;
}

const X25519_BASE = (() => {
  const b = new Uint8Array(32);
  b[0] = 9;
  return b;
})();

// ── Identity key ─────────────────────────────────────────────────────────────────────────

const ascii = (s: string): Uint8Array => Uint8Array.from(s, (c) => c.charCodeAt(0));
const INFO_WRAP_ENC = ascii('aurix-e2ee-v1 wrap enc');
const INFO_WRAP_AUTH = ascii('aurix-e2ee-v1 wrap auth');

/** Long-lived X25519 key of one client. */
export class E2eeIdentity {
  private constructor(
    private readonly secret: Uint8Array,
    readonly publicKey: Uint8Array,
    readonly fingerprint: string,
  ) {}

  /** A fresh random identity, or the one for a stored 32-byte `secret` (stable fingerprint). */
  static async create(secret?: Uint8Array): Promise<E2eeIdentity> {
    if (secret !== undefined && secret.length !== E2EE.SECRET_LEN) {
      throw new E2eeError('E2EE identity secret must be 32 bytes');
    }
    const sk = new Uint8Array(secret ?? randomBytes(E2EE.SECRET_LEN));
    const pk = x25519(sk, X25519_BASE);
    return new E2eeIdentity(sk, pk, await e2eeFingerprint(pk));
  }

  /** The secret to persist so the next session keeps this fingerprint. */
  exportSecret(): Uint8Array {
    return new Uint8Array(this.secret);
  }

  private async wrapKeys(peer: Uint8Array, senderPk: Uint8Array, recipientPk: Uint8Array): Promise<[Uint8Array, Uint8Array]> {
    const shared = x25519(this.secret, peer);
    const salt = concat(senderPk, recipientPk);
    return [await hkdfSha256(salt, shared, INFO_WRAP_ENC, 32), await hkdfSha256(salt, shared, INFO_WRAP_AUTH, 32)];
  }

  /** Seals `secret` (generation `generation`) for the peer holding `recipient`. */
  async wrap(recipient: Uint8Array, generation: number, secret: Uint8Array, nonce?: Uint8Array): Promise<Uint8Array> {
    if (recipient.length !== E2EE.PUBLIC_KEY_LEN) throw new E2eeError('recipient key must be 32 bytes');
    const [enc, auth] = await this.wrapKeys(recipient, this.publicKey, recipient);
    const n = nonce ?? randomBytes(E2EE.WRAP_NONCE_LEN);
    const iv = new Uint8Array(16);
    iv.set(n);
    const ct = await aes256Ctr(enc, iv, secret);
    const tag = await hmacSha256(auth, concat(new Uint8Array([generation & 0xff]), n, ct));
    return concat(n, ct, tag.subarray(0, E2EE.WRAP_TAG_LEN));
  }

  /** Opens a key wrapped by the peer holding `sender` for us. */
  async unwrap(sender: Uint8Array, generation: number, wrapped: Uint8Array): Promise<Uint8Array> {
    if (wrapped.length !== E2EE.WRAPPED_KEY_LEN) throw new E2eeError('wrapped key has a wrong length');
    if (sender.length !== E2EE.PUBLIC_KEY_LEN) throw new E2eeError('sender key must be 32 bytes');
    const [enc, auth] = await this.wrapKeys(sender, sender, this.publicKey);
    const nonce = wrapped.subarray(0, E2EE.WRAP_NONCE_LEN);
    const ct = wrapped.subarray(E2EE.WRAP_NONCE_LEN, E2EE.WRAP_NONCE_LEN + E2EE.SECRET_LEN);
    const tag = wrapped.subarray(E2EE.WRAP_NONCE_LEN + E2EE.SECRET_LEN);
    const expected = await hmacSha256(auth, concat(new Uint8Array([generation & 0xff]), nonce, ct));
    if (!FrameCrypto.tagsEqual(expected.subarray(0, E2EE.WRAP_TAG_LEN), tag)) {
      throw new E2eeError('wrapped key failed authentication');
    }
    const iv = new Uint8Array(16);
    iv.set(nonce);
    return aes256Ctr(enc, iv, ct);
  }
}

// ── WORKER_UNITS_BEGIN ───────────────────────────────────────────────────────────────────

/** Frame keys of one sender generation. */
export class E2eeSenderKey {
  private constructor(
    readonly generation: number,
    private readonly enc: CryptoKey,
    private readonly auth: CryptoKey,
    private readonly salt: Uint8Array,
  ) {}

  static async derive(generation: number, secret: Uint8Array): Promise<E2eeSenderKey> {
    const subtle = globalThis.crypto.subtle;
    const ikm = await subtle.importKey('raw', secret as Uint8Array<ArrayBuffer>, 'HKDF', false, ['deriveBits']);
    const expand = async (info: string, len: number) =>
      new Uint8Array(
        await subtle.deriveBits({ name: 'HKDF', hash: 'SHA-256', salt: new Uint8Array(0), info: Uint8Array.from(info, (c) => c.charCodeAt(0)) }, ikm, len * 8),
      );
    const [enc, auth, salt] = await Promise.all([
      expand('aurix-e2ee-v1 enc', 32),
      expand('aurix-e2ee-v1 auth', 32),
      expand('aurix-e2ee-v1 salt', 12),
    ]);
    const [encKey, authKey] = await Promise.all([
      subtle.importKey('raw', enc, { name: 'AES-CTR' }, false, ['encrypt']),
      subtle.importKey('raw', auth, { name: 'HMAC', hash: 'SHA-256' }, false, ['sign']),
    ]);
    return new E2eeSenderKey(generation & 0xff, encKey, authKey, salt);
  }

  private iv(counter: number): Uint8Array<ArrayBuffer> {
    const iv = new Uint8Array(16);
    iv[0] = this.generation;
    iv[1] = (counter >>> 24) & 0xff;
    iv[2] = (counter >>> 16) & 0xff;
    iv[3] = (counter >>> 8) & 0xff;
    iv[4] = counter & 0xff;
    for (let i = 0; i < 12; i++) iv[i] = (iv[i] ?? 0) ^ (this.salt[i] ?? 0);
    return iv;
  }

  /** Encrypts `plain` as frame number `counter` of this generation. */
  async seal(counter: number, plain: Uint8Array): Promise<Uint8Array> {
    const subtle = globalThis.crypto.subtle;
    const out = new Uint8Array(plain.length + 15);
    out[0] = this.generation;
    out[1] = (counter >>> 24) & 0xff;
    out[2] = (counter >>> 16) & 0xff;
    out[3] = (counter >>> 8) & 0xff;
    out[4] = counter & 0xff;
    if (plain.length > 0) {
      const ct = await subtle.encrypt({ name: 'AES-CTR', counter: this.iv(counter), length: 128 }, this.enc, plain as Uint8Array<ArrayBuffer>);
      out.set(new Uint8Array(ct), 5);
    }
    const tag = new Uint8Array(await subtle.sign('HMAC', this.auth, out.subarray(0, 5 + plain.length) as Uint8Array<ArrayBuffer>));
    out.set(tag.subarray(0, 10), 5 + plain.length);
    return out;
  }

  /** Generation byte and counter of an encrypted frame (no authentication). */
  static peek(frame: Uint8Array): { generation: number; counter: number } | undefined {
    if (frame.length < 15) return undefined;
    return {
      generation: frame[0] ?? 0,
      counter: ((frame[1] ?? 0) * 0x1000000 + ((frame[2] ?? 0) << 16) + ((frame[3] ?? 0) << 8) + (frame[4] ?? 0)) >>> 0,
    };
  }

  /** Authenticates and decrypts a frame of this generation. */
  async open(frame: Uint8Array): Promise<Uint8Array> {
    const head = E2eeSenderKey.peek(frame);
    if (!head) throw new Error('E2EE frame too short');
    if (head.generation !== this.generation) throw new Error('E2EE frame generation mismatch');
    const subtle = globalThis.crypto.subtle;
    const body = frame.subarray(0, frame.length - 10);
    const tag = frame.subarray(frame.length - 10);
    const expected = new Uint8Array(await subtle.sign('HMAC', this.auth, body as Uint8Array<ArrayBuffer>));
    if (!FrameCrypto.tagsEqual(expected.subarray(0, 10), tag)) throw new Error('E2EE frame failed authentication');
    const ct = body.subarray(5);
    if (ct.length === 0) return new Uint8Array(0);
    return new Uint8Array(await subtle.encrypt({ name: 'AES-CTR', counter: this.iv(head.counter), length: 128 }, this.enc, ct as Uint8Array<ArrayBuffer>));
  }
}

/** Anti-replay window of one received generation (bit `i` of `window`: `highest - i` seen). */
export class E2eeReplayState {
  private highest: number | undefined = undefined;
  private window = 0n;

  /** `true` (and records it) when `counter` was not seen before and is not too old. */
  accept(counter: number): boolean {
    const h = this.highest;
    if (h === undefined) {
      this.highest = counter;
      this.window = 1n;
      return true;
    }
    if (counter > h) {
      const shift = counter - h;
      this.window = shift >= 128 ? 0n : (this.window << BigInt(shift)) & ((1n << 128n) - 1n);
      this.window |= 1n;
      this.highest = counter;
      return true;
    }
    const age = h - counter;
    if (age >= 128) return false;
    const bit = 1n << BigInt(age);
    if ((this.window & bit) !== 0n) return false;
    this.window |= bit;
    return true;
  }
}

/** Receiving side of one peer: its last few sender keys. */
export class E2eePeerKeys {
  private generations: Array<{ key: E2eeSenderKey; replay: E2eeReplayState }> = [];

  insert(key: E2eeSenderKey): void {
    this.generations = this.generations.filter((g) => g.key.generation !== key.generation);
    this.generations.push({ key, replay: new E2eeReplayState() });
    while (this.generations.length > 4) this.generations.shift();
  }

  get isEmpty(): boolean {
    return this.generations.length === 0;
  }

  hasGeneration(generation: number): boolean {
    return this.generations.some((g) => g.key.generation === generation);
  }

  /** Decrypts `frame`, rejecting unknown generations and replayed counters. */
  async open(frame: Uint8Array): Promise<Uint8Array> {
    const head = E2eeSenderKey.peek(frame);
    if (!head) throw new Error('E2EE frame too short');
    const g = this.generations.find((x) => x.key.generation === head.generation);
    if (!g) throw new Error('no key for this E2EE generation');
    const plain = await g.key.open(frame);
    if (!g.replay.accept(head.counter)) throw new Error('replayed E2EE frame');
    return plain;
  }
}

export type E2eeMode = 'plain' | 'hold' | 'encrypt';

export interface E2eeFrameStats {
  /** Frames sealed on the uplink plus frames opened on the downlink. */
  framesE2ee: number;
  /** Received frames dropped because no key of their sender could open them. */
  undecryptable: number;
  /** Uplink frames dropped while the session's encryption state was still unknown. */
  held: number;
}

/**
 * Per-frame side of the group: the local sender key + counter, every peer's sender keys and
 * the `mid → user` layout of the per-participant downlink tracks. Lives wherever the encoded
 * frames are (main thread or the transform worker) and is fed by [`E2eeGroup`].
 *
 * - `plain`: frames pass untouched (no encrypted channel joined).
 * - `hold`: uplink frames are dropped (a join is in flight and may turn out encrypted).
 * - `encrypt`: uplink frames are sealed; downlink frames are opened with their sender's key or
 *   dropped — never played back as received.
 */
export class FrameCrypto {
  mode: E2eeMode = 'plain';
  private own: { key: E2eeSenderKey; counter: number } | undefined = undefined;
  private peers = new Map<string, E2eePeerKeys>();
  layout = new Map<string, string>();
  readonly stats: E2eeFrameStats = { framesE2ee: 0, undecryptable: 0, held: 0 };
  /** Called once when the frame counter of the current key reaches the rotation threshold. */
  onRotateNeeded: (() => void) | undefined = undefined;
  private rotateSignalled = false;

  static tagsEqual(a: Uint8Array, b: Uint8Array): boolean {
    if (a.length !== b.length) return false;
    let diff = 0;
    for (let i = 0; i < a.length; i++) diff |= (a[i] ?? 0) ^ (b[i] ?? 0);
    return diff === 0;
  }

  get generation(): number | undefined {
    return this.own?.key.generation;
  }

  async setOwnKey(generation: number, secret: Uint8Array): Promise<void> {
    const key = await E2eeSenderKey.derive(generation, secret);
    this.own = { key, counter: 0 };
    this.rotateSignalled = false;
  }

  async setPeerKey(userId: string, generation: number, secret: Uint8Array): Promise<void> {
    const key = await E2eeSenderKey.derive(generation, secret);
    let keys = this.peers.get(userId);
    if (!keys) {
      keys = new E2eePeerKeys();
      this.peers.set(userId, keys);
    }
    keys.insert(key);
  }

  forgetPeer(userId: string): void {
    this.peers.delete(userId);
  }

  hasKeyFor(userId: string): boolean {
    const keys = this.peers.get(userId);
    return keys !== undefined && !keys.isEmpty;
  }

  peerHasGeneration(userId: string, generation: number): boolean {
    return this.peers.get(userId)?.hasGeneration(generation) ?? false;
  }

  setLayout(entries: Array<[string, string | undefined]>): void {
    this.layout = new Map();
    for (const [mid, user] of entries) if (user !== undefined) this.layout.set(mid, user);
  }

  /** Encrypts one of our frames. */
  async encrypt(plain: Uint8Array): Promise<Uint8Array> {
    const own = this.own;
    if (!own) throw new Error('no E2EE sender key');
    const counter = own.counter;
    own.counter = (own.counter + 1) >>> 0;
    if (own.counter >= 2 ** 31 && !this.rotateSignalled) {
      this.rotateSignalled = true;
      this.onRotateNeeded?.();
    }
    this.stats.framesE2ee += 1;
    return own.key.seal(counter, plain);
  }

  /** Decrypts a frame sent by `userId`. */
  async decrypt(userId: string, frame: Uint8Array): Promise<Uint8Array> {
    const keys = this.peers.get(userId);
    if (!keys) throw new Error('unknown E2EE sender');
    return keys.open(frame);
  }

  /** Uplink: the bytes to send in place of `data`, or `undefined` to drop the frame. */
  async transformSend(data: Uint8Array): Promise<Uint8Array | undefined> {
    switch (this.mode) {
      case 'plain':
        return data;
      case 'hold':
        this.stats.held += 1;
        return undefined;
      case 'encrypt':
        if (!this.own) {
          this.stats.held += 1;
          return undefined;
        }
        return this.encrypt(data);
      default:
        return undefined;
    }
  }

  /** Downlink track `mid`: the bytes to play in place of `data`, or `undefined` to drop. */
  async transformReceive(mid: string | null, data: Uint8Array): Promise<Uint8Array | undefined> {
    if (this.mode !== 'encrypt') return data;
    const user = mid === null ? undefined : this.layout.get(mid);
    if (user === undefined) return undefined;
    try {
      const plain = await this.decrypt(user, data);
      this.stats.framesE2ee += 1;
      return plain;
    } catch {
      this.stats.undecryptable += 1;
      return undefined;
    }
  }

  /** A `TransformStream` transformer for one sender (`kind: 'sender'`) or receiver pipeline. */
  transformer(kind: 'sender' | 'receiver', midOf: () => string | null): Transformer<RTCEncodedAudioFrame, RTCEncodedAudioFrame> {
    return {
      transform: async (frame, controller) => {
        const input = new Uint8Array(frame.data);
        const output = kind === 'sender' ? await this.transformSend(input) : await this.transformReceive(midOf(), input);
        if (output === undefined) return;
        if (output !== input) {
          const buf = new ArrayBuffer(output.byteLength);
          new Uint8Array(buf).set(output);
          frame.data = buf;
        }
        controller.enqueue(frame);
      },
    };
  }
}

/** Messages `E2eeGroup` posts to the transform worker (and it answers). */
export type E2eeWorkerMessage =
  | { type: 'mode'; mode: E2eeMode }
  | { type: 'ownKey'; generation: number; secret: Uint8Array }
  | { type: 'peerKey'; userId: string; generation: number; secret: Uint8Array }
  | { type: 'forgetPeer'; userId: string }
  | { type: 'layout'; entries: Array<[string, string | undefined]> }
  | { type: 'stats' };

export type E2eeWorkerReply = { type: 'rotate' } | { type: 'stats'; stats: E2eeFrameStats };

interface WorkerScope {
  postMessage(msg: E2eeWorkerReply): void;
  onmessage: ((ev: MessageEvent<E2eeWorkerMessage>) => void) | null;
  onrtctransform?: ((ev: { transformer: RTCTransformerLike }) => void) | null;
}

interface RTCTransformerLike {
  readable: ReadableStream<RTCEncodedAudioFrame>;
  writable: WritableStream<RTCEncodedAudioFrame>;
  options: { kind: 'sender' | 'receiver'; mid: string | null };
}

/** Body of the `RTCRtpScriptTransform` worker: one `FrameCrypto` fed over `postMessage`. */
export function e2eeWorkerMain(scope: WorkerScope): FrameCrypto {
  const frames = new FrameCrypto();
  frames.onRotateNeeded = () => scope.postMessage({ type: 'rotate' });
  let queue: Promise<unknown> = Promise.resolve();
  const serial = (task: () => Promise<void> | void) => {
    queue = queue.then(task, task);
  };
  scope.onmessage = (ev) => {
    const m = ev.data;
    switch (m.type) {
      case 'mode':
        serial(() => {
          frames.mode = m.mode;
        });
        return;
      case 'ownKey':
        serial(() => frames.setOwnKey(m.generation, m.secret));
        return;
      case 'peerKey':
        serial(() => frames.setPeerKey(m.userId, m.generation, m.secret));
        return;
      case 'forgetPeer':
        serial(() => frames.forgetPeer(m.userId));
        return;
      case 'layout':
        serial(() => frames.setLayout(m.entries));
        return;
      case 'stats':
        scope.postMessage({ type: 'stats', stats: { ...frames.stats } });
        return;
      default:
        return;
    }
  };
  scope.onrtctransform = (ev) => {
    const t = ev.transformer;
    const transformer = frames.transformer(t.options.kind, () => t.options.mid);
    // Frames wait for key material posted earlier: keep them behind the same queue.
    void t.readable
      .pipeThrough(
        new TransformStream<RTCEncodedAudioFrame, RTCEncodedAudioFrame>({
          transform: async (frame, controller) => {
            await queue;
            await transformer.transform?.(frame, controller);
          },
        }),
      )
      .pipeTo(t.writable)
      .catch(() => undefined);
  };
  return frames;
}

// ── WORKER_UNITS_END ─────────────────────────────────────────────────────────────────────

/** JavaScript source of the transform worker (self-contained; served from a `blob:` URL). */
export function e2eeWorkerSource(): string {
  const units = [E2eeSenderKey, E2eeReplayState, E2eePeerKeys, FrameCrypto];
  return `'use strict';\n${units.map((u) => u.toString()).join('\n')}\n(${e2eeWorkerMain.toString()})(self);\n`;
}

// ── Group state machine ──────────────────────────────────────────────────────────────────

export type E2eeOutgoing =
  | { type: 'hello'; channelId: string }
  | { type: 'senderKey'; channelId: string; to: string; generation: number; wrapped: Uint8Array };

export type E2eePeerChange = { kind: 'new' } | { kind: 'keyChanged'; previousFingerprint: string };

/** Receives key material as the group learns it (the transform worker's mirror). */
export interface E2eeKeySink {
  post(msg: E2eeWorkerMessage): void;
}

interface Peer {
  publicKey: Uint8Array;
  fingerprint: string;
  channels: Set<string>;
  /** Generation of our key we last wrapped for this peer. */
  sentGeneration: number | undefined;
}

/**
 * One client's view of the encrypted groups it belongs to: identity, own sender key and the
 * peers (across all encrypted channels) it exchanges keys with. Mirrors `e2ee::Group`; the
 * caller serialises calls (they are async because wrapping uses WebCrypto).
 */
export class E2eeGroup {
  readonly frames = new FrameCrypto();
  private secret: Uint8Array;
  private generationValue = 0;
  private channelSet = new Set<string>();
  private peers = new Map<string, Peer>();
  private rotationPending = false;
  /** Fired when the frame counter demands a rotation (the caller schedules `rotate()`). */
  onRotateNeeded: (() => void) | undefined = undefined;

  private constructor(
    readonly identity: E2eeIdentity,
    private readonly sink: E2eeKeySink | undefined,
  ) {
    this.secret = randomBytes(E2EE.SECRET_LEN);
    this.frames.onRotateNeeded = () => {
      this.rotationPending = true;
      this.onRotateNeeded?.();
    };
  }

  static async create(identity: E2eeIdentity, sink?: E2eeKeySink): Promise<E2eeGroup> {
    const g = new E2eeGroup(identity, sink);
    await g.installOwnKey();
    return g;
  }

  private async installOwnKey(): Promise<void> {
    await this.frames.setOwnKey(this.generationValue, this.secret);
    this.sink?.post({ type: 'ownKey', generation: this.generationValue, secret: new Uint8Array(this.secret) });
  }

  get generation(): number {
    return this.generationValue;
  }

  get channels(): ReadonlySet<string> {
    return this.channelSet;
  }

  isEncrypted(channelId: string): boolean {
    return this.channelSet.has(channelId);
  }

  /** Whether any encrypted channel is joined, i.e. the uplink must be encrypted. */
  get active(): boolean {
    return this.channelSet.size > 0;
  }

  get isRotationPending(): boolean {
    return this.rotationPending;
  }

  peerFingerprint(userId: string): string | undefined {
    return this.peers.get(userId)?.fingerprint;
  }

  /** Peers we hold a sender key of (we can decrypt their frames). */
  decryptablePeers(): string[] {
    return [...this.peers.keys()].filter((u) => this.frames.hasKeyFor(u));
  }

  hasKeyFor(userId: string): boolean {
    return this.frames.hasKeyFor(userId);
  }

  /** We joined an encrypted channel: announce ourselves so members key us (and rotate). */
  joined(channelId: string): E2eeOutgoing[] {
    if (this.channelSet.has(channelId)) return [];
    this.channelSet.add(channelId);
    return [{ type: 'hello', channelId }];
  }

  /**
   * Our membership was re-acknowledged (session resume): peers absent from the server's
   * `members` left meanwhile and are forgotten; a fresh hello makes the rest re-send keys we
   * may have missed. Returns the messages to send and the forgotten peers we could decrypt.
   */
  rejoined(channelId: string, members: ReadonlySet<string>): { out: E2eeOutgoing[]; gone: string[] } {
    if (!this.channelSet.has(channelId)) return { out: this.joined(channelId), gone: [] };
    const gone: string[] = [];
    for (const [user, peer] of [...this.peers]) {
      if (peer.channels.has(channelId) && !members.has(user)) gone.push(...this.peerLeft(channelId, user));
    }
    return { out: [{ type: 'hello', channelId }], gone };
  }

  /** We left an encrypted channel; returns the dropped peers we could decrypt. */
  left(channelId: string): string[] {
    if (!this.channelSet.delete(channelId)) return [];
    const gone: string[] = [];
    for (const [user, peer] of [...this.peers]) {
      peer.channels.delete(channelId);
      if (peer.channels.size === 0) gone.push(...this.dropPeer(user));
    }
    return gone;
  }

  /** Drops every channel and peer (session ended); the identity key stays. */
  reset(): string[] {
    const gone: string[] = [];
    for (const user of [...this.peers.keys()]) gone.push(...this.dropPeer(user));
    this.channelSet.clear();
    this.rotationPending = true;
    return gone;
  }

  /** A peer left `channelId`; returns it if it was dropped and we could decrypt it. */
  peerLeft(channelId: string, userId: string): string[] {
    const peer = this.peers.get(userId);
    if (!peer) return [];
    peer.channels.delete(channelId);
    if (peer.channels.size > 0) return [];
    return this.dropPeer(userId);
  }

  private dropPeer(userId: string): string[] {
    const decryptable = this.frames.hasKeyFor(userId);
    this.peers.delete(userId);
    this.frames.forgetPeer(userId);
    this.sink?.post({ type: 'forgetPeer', userId });
    this.rotationPending = true;
    return decryptable ? [userId] : [];
  }

  private async learnPeer(channelId: string, userId: string, publicKey: Uint8Array): Promise<E2eePeerChange | undefined> {
    const peer = this.peers.get(userId);
    if (peer && FrameCrypto.tagsEqual(peer.publicKey, publicKey)) {
      peer.channels.add(channelId);
      return undefined;
    }
    const fingerprint = await e2eeFingerprint(publicKey);
    if (peer) {
      const previousFingerprint = peer.fingerprint;
      peer.publicKey = new Uint8Array(publicKey);
      peer.fingerprint = fingerprint;
      peer.channels.add(channelId);
      peer.sentGeneration = undefined;
      this.frames.forgetPeer(userId);
      this.sink?.post({ type: 'forgetPeer', userId });
      this.rotationPending = true;
      return { kind: 'keyChanged', previousFingerprint };
    }
    this.peers.set(userId, { publicKey: new Uint8Array(publicKey), fingerprint, channels: new Set([channelId]), sentGeneration: undefined });
    this.rotationPending = true;
    return { kind: 'new' };
  }

  /**
   * A peer announced itself in `channelId`. A peer we already trust gets our current key
   * right away; a new or re-keyed peer triggers a rotation instead, so it only ever receives
   * a key that post-dates its arrival.
   */
  async onHello(channelId: string, userId: string, publicKey: Uint8Array): Promise<{ out: E2eeOutgoing[]; change: E2eePeerChange | undefined }> {
    if (!this.channelSet.has(channelId)) return { out: [], change: undefined };
    if (publicKey.length !== E2EE.PUBLIC_KEY_LEN) throw new E2eeError('peer public key must be 32 bytes');
    const change = await this.learnPeer(channelId, userId, publicKey);
    if (change) return { out: [], change };
    const msg = await this.wrapFor(userId);
    return { out: msg ? [msg] : [], change: undefined };
  }

  /** A peer sent us their sender key; `decryptable` is set when we could not open its frames before. */
  async onSenderKey(
    channelId: string,
    userId: string,
    publicKey: Uint8Array,
    generation: number,
    wrapped: Uint8Array,
  ): Promise<{ out: E2eeOutgoing[]; change: E2eePeerChange | undefined; decryptable: boolean }> {
    if (!this.channelSet.has(channelId)) return { out: [], change: undefined, decryptable: false };
    const secret = await this.identity.unwrap(publicKey, generation, wrapped);
    const change = await this.learnPeer(channelId, userId, publicKey);
    const peer = this.peers.get(userId);
    if (!peer) throw new E2eeError('peer vanished');
    const wasDecryptable = this.frames.hasKeyFor(userId);
    await this.frames.setPeerKey(userId, generation, secret);
    this.sink?.post({ type: 'peerKey', userId, generation, secret: new Uint8Array(secret) });
    // A peer that keyed us before we keyed them (both joined at once) still needs ours.
    const out: E2eeOutgoing[] = [];
    if (!change && peer.sentGeneration === undefined) {
      const msg = await this.wrapFor(userId);
      if (msg) out.push(msg);
    }
    return { out, change, decryptable: !wasDecryptable };
  }

  private async wrapFor(userId: string): Promise<E2eeOutgoing | undefined> {
    const peer = this.peers.get(userId);
    if (!peer) return undefined;
    const channelId = peer.channels.values().next().value;
    if (channelId === undefined) return undefined;
    const generation = this.generationValue;
    const wrapped = await this.identity.wrap(peer.publicKey, generation, this.secret);
    peer.sentGeneration = generation;
    return { type: 'senderKey', channelId, to: userId, generation, wrapped };
  }

  /**
   * Picks a fresh sender key and wraps it for every peer; no-op unless a rotation is pending
   * or `force`. Callers debounce this (one rotation per join wave).
   */
  async rotate(force = false): Promise<{ out: E2eeOutgoing[]; generation: number } | undefined> {
    if (!(force || this.rotationPending)) return undefined;
    this.rotationPending = false;
    this.secret = randomBytes(E2EE.SECRET_LEN);
    this.generationValue = (this.generationValue + 1) & 0xff;
    await this.installOwnKey();
    const out: E2eeOutgoing[] = [];
    for (const user of [...this.peers.keys()]) {
      const msg = await this.wrapFor(user);
      if (msg) out.push(msg);
    }
    return { out, generation: this.generationValue };
  }

  /** Encrypts one of our frames (main-thread transform path / tests). */
  encrypt(plain: Uint8Array): Promise<Uint8Array> {
    return this.frames.encrypt(plain);
  }

  /** Decrypts a frame sent by `userId` (main-thread transform path / tests). */
  decrypt(userId: string, frame: Uint8Array): Promise<Uint8Array> {
    return this.frames.decrypt(userId, frame);
  }
}

// ── Capability detection ─────────────────────────────────────────────────────────────────

/** Which encoded-frame API the browser offers for E2EE. */
export type E2eeTransformApi = 'script' | 'streams';

export interface E2eeSupport {
  /** WebCrypto is available (secure context). */
  crypto: boolean;
  /** `RTCRtpScriptTransform` (standard, worker-based) or Chromium's `createEncodedStreams()`; absent = no E2EE. */
  transform: E2eeTransformApi | undefined;
  /** Both of the above: encrypted channels can be joined. */
  ok: boolean;
}

interface EncodedStreamsCapable {
  createEncodedStreams?: () => { readable: ReadableStream<RTCEncodedAudioFrame>; writable: WritableStream<RTCEncodedAudioFrame> };
}

export function detectE2eeSupport(prefer: 'auto' | E2eeTransformApi = 'auto'): E2eeSupport {
  const crypto = typeof globalThis.crypto?.subtle?.deriveBits === 'function' && typeof TransformStream === 'function';
  const script = typeof RTCRtpScriptTransform === 'function' && typeof Worker === 'function' && typeof Blob === 'function';
  const streams =
    typeof RTCRtpSender === 'function' &&
    typeof (RTCRtpSender.prototype as EncodedStreamsCapable).createEncodedStreams === 'function' &&
    typeof RTCRtpReceiver === 'function' &&
    typeof (RTCRtpReceiver.prototype as EncodedStreamsCapable).createEncodedStreams === 'function';
  let transform: E2eeTransformApi | undefined;
  if (prefer === 'script') transform = script ? 'script' : undefined;
  else if (prefer === 'streams') transform = streams ? 'streams' : undefined;
  else transform = script ? 'script' : streams ? 'streams' : undefined;
  return { crypto, transform, ok: crypto && transform !== undefined };
}

/** Installs `FrameCrypto` pipelines on a sender/receiver via Chromium's `createEncodedStreams()`. */
export function attachEncodedStreams(
  target: RTCRtpSender | RTCRtpReceiver,
  frames: FrameCrypto,
  kind: 'sender' | 'receiver',
  midOf: () => string | null,
): void {
  const create = (target as EncodedStreamsCapable).createEncodedStreams;
  if (typeof create !== 'function') throw new E2eeError('createEncodedStreams is unavailable');
  const { readable, writable } = create.call(target);
  void readable
    .pipeThrough(new TransformStream(frames.transformer(kind, midOf)))
    .pipeTo(writable)
    .catch(() => undefined);
}
