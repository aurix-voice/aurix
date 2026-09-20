import { test } from 'node:test';
import assert from 'node:assert/strict';
import vm from 'node:vm';
import {
  E2EE_CONSTANTS,
  E2eeGroup,
  E2eeIdentity,
  E2eePeerKeys,
  E2eeReplayState,
  E2eeSenderKey,
  FrameCrypto,
  bytesToHex,
  detectE2eeSupport,
  e2eeWorkerSource,
  hexToBytes,
  hkdfSha256,
  x25519,
} from '../dist/e2ee.js';

const hex = bytesToHex;
const filled = (n, v) => new Uint8Array(n).fill(v);
const range = (n) => Uint8Array.from({ length: n }, (_, i) => i);

// ── Primitives against RFC vectors and the Rust suite (`aurix-common/src/e2ee.rs`) ──

test('HKDF-SHA256 matches RFC 5869 case 1', async () => {
  const okm = await hkdfSha256(range(13), filled(22, 0x0b), Uint8Array.from({ length: 10 }, (_, i) => 0xf0 + i), 42);
  assert.equal(hex(okm), '3cb25f25faacd57a90434f64d0362f2a2d2d0a90cf1a5a4c5db02d56ecc4c5bf34007208d5b887185865');
});

test('X25519 matches RFC 7748 §5.2 and rejects low-order points', () => {
  const k = hexToBytes('a546e36bf0527c9d3b16154b82465edd62144c0ac1fc5a18506a2244ba449ac4');
  const u = hexToBytes('e6db6867583030db3594c1a424b15f7c726624ec26b3353b10a903a6d0ab1c4c');
  assert.equal(hex(x25519(k, u)), 'c3da55379de9c6908e94ea4df28d084f32eccf03491c71f754b4075577a28552');
  assert.throws(() => x25519(k, new Uint8Array(32)), /low-order/);
});

test('identity keys, fingerprint and key wrapping match the Rust vectors', async () => {
  const alice = await E2eeIdentity.create(filled(32, 1));
  const bob = await E2eeIdentity.create(filled(32, 2));
  assert.equal(hex(alice.publicKey), 'a4e09292b651c278b9772c569f5fa9bb13d906b46ab68c9df9dc2b4409f8a209');
  assert.equal(hex(bob.publicKey), 'ce8d3ad1ccb633ec7b70c17814a5c76ecd029685050d344745ba05870e587d59');
  assert.equal(alice.fingerprint, '1a92f23852dc908d97316a3b13578281196c1dd73d9ae5e313f3fb6b8954bf55');
  assert.deepEqual(alice.exportSecret(), filled(32, 1));

  const fixed =
    '42424242424242424242424249d1a4e521c229f2380da9fe507d631f090452c1bbc07032dcda4bba6be02de573dc00012e371ae34d5fbd125518632b';
  const wrapped = await alice.wrap(bob.publicKey, 5, filled(32, 9), filled(12, 0x42));
  assert.equal(wrapped.length, E2EE_CONSTANTS.WRAPPED_KEY_LEN);
  assert.equal(hex(wrapped), fixed);
  assert.deepEqual(await bob.unwrap(alice.publicKey, 5, hexToBytes(fixed)), filled(32, 9));
  // Direction, generation and content are all authenticated.
  await assert.rejects(alice.unwrap(bob.publicKey, 5, hexToBytes(fixed)), /authentication/);
  await assert.rejects(bob.unwrap(alice.publicKey, 6, hexToBytes(fixed)), /authentication/);
  const bad = hexToBytes(fixed);
  bad[20] ^= 1;
  await assert.rejects(bob.unwrap(alice.publicKey, 5, bad), /authentication/);
  await assert.rejects(bob.unwrap(alice.publicKey, 5, bad.subarray(1)), /length/);
  // Random nonces: two wraps differ, both open.
  const w1 = await alice.wrap(bob.publicKey, 1, filled(32, 3));
  const w2 = await alice.wrap(bob.publicKey, 1, filled(32, 3));
  assert.notEqual(hex(w1), hex(w2));
  assert.deepEqual(await bob.unwrap(alice.publicKey, 1, w1), filled(32, 3));
  assert.deepEqual(await bob.unwrap(alice.publicKey, 1, w2), filled(32, 3));
});

test('frame seal/open matches the Rust vector and rejects tampering', async () => {
  const key = await E2eeSenderKey.derive(1, range(32));
  const plain = Uint8Array.from([0xf8, 0xff, 0xfe, 0x00, 0x01]);
  const frame = await key.seal(0x01020304, plain);
  assert.equal(hex(frame), '0101020304eb1575b9e6b52709cbdab928367122');
  assert.deepEqual(E2eeSenderKey.peek(frame), { generation: 1, counter: 0x01020304 });
  assert.deepEqual(await key.open(frame), plain);

  const other = await E2eeSenderKey.derive(3, filled(32, 7));
  const f = await other.seal(42, new TextEncoder().encode('opus frame'));
  assert.equal(f.length, 10 + E2EE_CONSTANTS.FRAME_OVERHEAD);
  const bad = new Uint8Array(f);
  bad[7] ^= 1;
  await assert.rejects(other.open(bad), /authentication/);
  const wrongGen = new Uint8Array(f);
  wrongGen[0] = 4;
  await assert.rejects(other.open(wrongGen), /generation/);
  await assert.rejects(other.open(f.subarray(0, E2EE_CONSTANTS.FRAME_OVERHEAD - 1)), /short|length/);
  assert.deepEqual(await other.open(await other.seal(0, new Uint8Array(0))), new Uint8Array(0));
});

test('replay window accepts each counter once, tolerates reordering, expires old ones', () => {
  const r = new E2eeReplayState();
  assert.equal(r.accept(10), true);
  assert.equal(r.accept(10), false);
  assert.equal(r.accept(8), true);
  assert.equal(r.accept(8), false);
  assert.equal(r.accept(300), true);
  assert.equal(r.accept(200), true);
  assert.equal(r.accept(172), false); // 128 behind
  assert.equal(r.accept(173), true);
  assert.equal(r.accept(2 ** 32 - 1), true);
  assert.equal(r.accept(2 ** 32 - 1), false);
});

test('peer keys keep the last four generations and reject replays', async () => {
  const peer = new E2eePeerKeys();
  assert.equal(peer.isEmpty, true);
  const keys = [];
  for (let g = 1; g <= 5; g++) {
    const k = await E2eeSenderKey.derive(g, filled(32, g));
    keys.push(k);
    peer.insert(k);
  }
  assert.equal(peer.hasGeneration(1), false);
  assert.equal(peer.hasGeneration(2), true);
  assert.equal(peer.hasGeneration(5), true);
  const f = await keys[4].seal(7, Uint8Array.from([1, 2, 3]));
  assert.deepEqual(await peer.open(f), Uint8Array.from([1, 2, 3]));
  await assert.rejects(peer.open(f), /replayed/);
  await assert.rejects(peer.open(await keys[0].seal(1, Uint8Array.from([9]))), /no key/);
  // The previous generation is still accepted (frames in flight across a rotation).
  assert.deepEqual(await peer.open(await keys[3].seal(0, Uint8Array.from([4]))), Uint8Array.from([4]));
});

// ── Frame transform: plaintext never leaks in either direction ──

test('FrameCrypto holds/encrypts uplink and only plays decryptable downlink', async () => {
  const fc = new FrameCrypto();
  const plain = Uint8Array.from([0xf8, 1, 2, 3]);
  assert.deepEqual(await fc.transformSend(plain), plain);
  fc.mode = 'hold';
  assert.equal(await fc.transformSend(plain), undefined);
  fc.mode = 'encrypt';
  assert.equal(await fc.transformSend(plain), undefined); // no key yet
  assert.equal(fc.stats.held, 2);
  await fc.setOwnKey(1, filled(32, 1));
  const sent = await fc.transformSend(plain);
  assert.equal(sent.length, plain.length + E2EE_CONSTANTS.FRAME_OVERHEAD);
  assert.deepEqual(E2eeSenderKey.peek(sent), { generation: 1, counter: 0 });
  assert.equal(fc.stats.framesE2ee, 1);

  // Receiving side: unknown mid (the server mix) and unknown senders are silenced.
  const rx = new FrameCrypto();
  rx.mode = 'encrypt';
  assert.equal(await rx.transformReceive('0', sent), undefined);
  rx.setLayout([
    ['1', 'alice'],
    ['2', undefined],
  ]);
  assert.equal(await rx.transformReceive('1', sent), undefined);
  assert.equal(rx.stats.undecryptable, 1);
  await rx.setPeerKey('alice', 1, filled(32, 1));
  assert.deepEqual(await rx.transformReceive('1', sent), plain);
  assert.equal(await rx.transformReceive('1', sent), undefined); // replay
  assert.equal(await rx.transformReceive('1', plain), undefined); // plaintext on an encrypted track
  assert.equal(await rx.transformReceive('2', sent), undefined);
  assert.equal(await rx.transformReceive(null, sent), undefined);
  assert.equal(rx.stats.undecryptable, 3);
  rx.mode = 'plain';
  assert.deepEqual(await rx.transformReceive('1', plain), plain);
  rx.forgetPeer('alice');
  assert.equal(rx.hasKeyFor('alice'), false);
});

test('FrameCrypto signals rotation when the counter reaches 2^31', async () => {
  const fc = new FrameCrypto();
  await fc.setOwnKey(0, filled(32, 5));
  fc.mode = 'encrypt';
  let signalled = 0;
  fc.onRotateNeeded = () => signalled++;
  // Jump the counter close to the threshold through a sealed frame's header.
  const f0 = await fc.encrypt(Uint8Array.from([1]));
  assert.equal(E2eeSenderKey.peek(f0).counter, 0);
  fc.own.counter = 2 ** 31 - 1;
  const f1 = await fc.encrypt(Uint8Array.from([1]));
  assert.equal(E2eeSenderKey.peek(f1).counter, 2 ** 31 - 1);
  assert.equal(signalled, 1);
  await fc.encrypt(Uint8Array.from([1]));
  assert.equal(signalled, 1);
  await fc.setOwnKey(1, filled(32, 6));
  assert.equal(E2eeSenderKey.peek(await fc.encrypt(Uint8Array.from([1]))).counter, 0);
});

// ── Group state machine (mirrors `e2ee::Group`): a relay between in-memory members ──

class Relay {
  constructor() {
    this.members = new Map(); // userId -> { group, sink }
    this.rotations = [];
    this.decryptable = [];
  }
  add(userId, group) {
    this.members.set(userId, group);
  }
  async deliver(from, out) {
    for (const m of out) {
      const sender = this.members.get(from);
      if (m.type === 'hello') {
        for (const [user, g] of this.members) {
          if (user === from) continue;
          const r = await g.onHello(m.channelId, from, sender.identity.publicKey);
          await this.deliver(user, r.out);
        }
      } else {
        const target = this.members.get(m.to);
        if (!target) continue;
        const r = await target.onSenderKey(m.channelId, from, sender.identity.publicKey, m.generation, m.wrapped);
        if (r.decryptable) this.decryptable.push([m.to, from]);
        await this.deliver(m.to, r.out);
      }
    }
  }
  /** Runs pending rotations (what the client's debounce timer does) until nothing is pending. */
  async settle() {
    for (let i = 0; i < 8; i++) {
      let progressed = false;
      for (const [user, g] of this.members) {
        if (!g.isRotationPending) continue;
        const r = await g.rotate();
        this.rotations.push([user, r.generation]);
        await this.deliver(user, r.out);
        progressed = true;
      }
      if (!progressed) return;
    }
    throw new Error('rotation storm');
  }
}

async function member(relay, userId, seed) {
  const g = await E2eeGroup.create(await E2eeIdentity.create(filled(32, seed)));
  relay.add(userId, g);
  return g;
}

async function roundTrip(from, to, fromId) {
  const plain = Uint8Array.from([0xf8, 0x11, 0x22, 0x33]);
  const frame = await from.encrypt(plain);
  assert.deepEqual(await to.decrypt(fromId, frame), plain);
}

test('group: keys flow after hello, rotate on join and leave, and old members lose access', async () => {
  const relay = new Relay();
  const alice = await member(relay, 'alice', 1);
  const bob = await member(relay, 'bob', 2);
  assert.equal(alice.generation, 0);

  await relay.deliver('alice', alice.joined('ch'));
  await relay.settle();
  assert.equal(alice.isEncrypted('ch'), true);
  assert.equal(alice.active, true);

  await relay.deliver('bob', bob.joined('ch'));
  await relay.settle();
  // Alice rotated because a new peer arrived; Bob learnt Alice via her hello reply.
  assert.equal(alice.generation, 1);
  assert.ok(relay.rotations.some(([u]) => u === 'alice'));
  assert.equal(alice.peerFingerprint('bob'), bob.identity.fingerprint);
  assert.equal(bob.peerFingerprint('alice'), alice.identity.fingerprint);
  assert.deepEqual(alice.decryptablePeers(), ['bob']);
  assert.deepEqual(bob.decryptablePeers(), ['alice']);
  await roundTrip(alice, bob, 'alice');
  await roundTrip(bob, alice, 'bob');

  const carol = await member(relay, 'carol', 3);
  await relay.deliver('carol', carol.joined('ch'));
  await relay.settle();
  assert.equal(alice.generation, 2);
  assert.equal(bob.generation, 2);
  await roundTrip(alice, carol, 'alice');
  await roundTrip(carol, bob, 'carol');
  await roundTrip(bob, carol, 'bob');

  // Carol leaves: her keys are dropped and everyone rotates; frames under her key still open
  // for peers that kept the previous generation, but she never receives the new one.
  const genBefore = alice.generation;
  assert.deepEqual(alice.peerLeft('ch', 'carol'), ['carol']);
  assert.deepEqual(bob.peerLeft('ch', 'carol'), ['carol']);
  assert.deepEqual(carol.left('ch'), ['alice', 'bob']);
  relay.members.delete('carol');
  await relay.settle();
  assert.equal(alice.generation, genBefore + 1);
  assert.equal(alice.hasKeyFor('carol'), false);
  assert.equal(carol.active, false);
  await roundTrip(alice, bob, 'alice');
  const late = await alice.encrypt(Uint8Array.from([1]));
  await assert.rejects(carol.decrypt('alice', late), /unknown E2EE sender|no key/);
});

test('group: simultaneous joins converge, and a peer is kept while any shared channel remains', async () => {
  const relay = new Relay();
  const alice = await member(relay, 'alice', 1);
  const bob = await member(relay, 'bob', 2);
  const a = alice.joined('a');
  const b = bob.joined('a');
  await relay.deliver('alice', a);
  await relay.deliver('bob', b);
  await relay.settle();
  await roundTrip(alice, bob, 'alice');
  await roundTrip(bob, alice, 'bob');

  await relay.deliver('alice', alice.joined('b'));
  await relay.deliver('bob', bob.joined('b'));
  await relay.settle();
  assert.deepEqual(alice.peerLeft('a', 'bob'), []); // still shares `b`
  assert.equal(alice.hasKeyFor('bob'), true);
  assert.deepEqual(alice.peerLeft('b', 'bob'), ['bob']);
  assert.equal(alice.hasKeyFor('bob'), false);
  assert.deepEqual(alice.left('a'), []);
  assert.deepEqual([...alice.channels], ['b']);
});

test('group: unauthenticated or stale keys are rejected, foreign channels ignored', async () => {
  const relay = new Relay();
  const alice = await member(relay, 'alice', 1);
  const bob = await member(relay, 'bob', 2);
  const mallory = await E2eeIdentity.create(filled(32, 9));
  await relay.deliver('alice', alice.joined('ch'));
  await relay.deliver('bob', bob.joined('ch'));
  await relay.settle();

  // A key wrapped by someone else claiming Alice's identity fails authentication and is not installed.
  const forged = await mallory.wrap(bob.identity.publicKey, 7, filled(32, 4));
  await assert.rejects(bob.onSenderKey('ch', 'alice', alice.identity.publicKey, 7, forged), /authentication/);
  assert.equal(bob.frames.peerHasGeneration('alice', 7), false);
  // Same sender, generation mismatch → rejected too.
  const real = await alice.identity.wrap(bob.identity.publicKey, 7, filled(32, 4));
  await assert.rejects(bob.onSenderKey('ch', 'alice', alice.identity.publicKey, 8, real), /authentication/);
  // Messages for channels we are not in do nothing.
  const r = await bob.onHello('other', 'carol', mallory.publicKey);
  assert.deepEqual(r, { out: [], change: undefined });
  assert.equal(bob.peerFingerprint('carol'), undefined);
  // A peer that re-announces with a different identity is re-keyed only after rotation.
  const before = bob.generation;
  const changed = await bob.onHello('ch', 'alice', mallory.publicKey);
  assert.equal(changed.change.kind, 'keyChanged');
  assert.equal(changed.change.previousFingerprint, alice.identity.fingerprint);
  assert.deepEqual(changed.out, []);
  assert.equal(bob.hasKeyFor('alice'), false);
  assert.equal(bob.isRotationPending, true);
  const rot = await bob.rotate();
  assert.equal(rot.generation, before + 1);
  assert.equal(rot.out.length, 1);
  assert.equal(rot.out[0].to, 'alice');
  // Wrapped for Mallory's key now: Alice's real identity cannot open it.
  await assert.rejects(alice.identity.unwrap(bob.identity.publicKey, rot.generation, rot.out[0].wrapped), /authentication/);
  assert.deepEqual(await mallory.unwrap(bob.identity.publicKey, rot.generation, rot.out[0].wrapped) instanceof Uint8Array, true);
});

test('group: resume reconciles the roster, reset forgets everyone but keeps the identity', async () => {
  const relay = new Relay();
  const alice = await member(relay, 'alice', 1);
  const bob = await member(relay, 'bob', 2);
  const carol = await member(relay, 'carol', 3);
  for (const [u, g] of relay.members) await relay.deliver(u, g.joined('ch'));
  await relay.settle();
  assert.deepEqual(alice.decryptablePeers().sort(), ['bob', 'carol']);

  // Carol left while Alice was disconnected: the replayed ack lists only Bob.
  const r = alice.rejoined('ch', new Set(['alice', 'bob']));
  assert.deepEqual(r.gone, ['carol']);
  assert.deepEqual(r.out, [{ type: 'hello', channelId: 'ch' }]);
  assert.equal(alice.hasKeyFor('carol'), false);
  assert.equal(alice.hasKeyFor('bob'), true);
  assert.equal(alice.isRotationPending, true);
  // Unknown channel on resume behaves like a first join.
  assert.deepEqual(alice.rejoined('new', new Set()).out, [{ type: 'hello', channelId: 'new' }]);

  const fp = alice.identity.fingerprint;
  assert.deepEqual(alice.reset().sort(), ['bob']);
  assert.equal(alice.active, false);
  assert.equal(alice.channels.size, 0);
  assert.equal(alice.identity.fingerprint, fp);
});

// ── The transform worker is self-contained: run its source in a bare realm ──

test('worker source runs standalone and encrypts/decrypts encoded frames', async () => {
  const posted = [];
  const self = { postMessage: (m) => posted.push(m), onmessage: null, onrtctransform: null };
  const context = vm.createContext({ self, crypto: globalThis.crypto, TransformStream });
  vm.runInContext(e2eeWorkerSource(), context, { filename: 'e2ee-worker.js' });
  assert.equal(typeof self.onmessage, 'function');
  assert.equal(typeof self.onrtctransform, 'function');

  const post = (data) => self.onmessage({ data });
  post({ type: 'mode', mode: 'encrypt' });
  post({ type: 'ownKey', generation: 2, secret: filled(32, 1) });
  post({ type: 'peerKey', userId: 'bob', generation: 4, secret: filled(32, 2) });
  post({ type: 'layout', entries: [['1', 'bob'], ['0', undefined]] });

  const run = async (kind, mid, payloads) => {
    const out = [];
    const readable = new ReadableStream({
      start(c) {
        for (const p of payloads) c.enqueue({ data: p.buffer.slice(p.byteOffset, p.byteOffset + p.byteLength), timestamp: 1 });
        c.close();
      },
    });
    const writable = new WritableStream({ write: (f) => void out.push(new Uint8Array(f.data)) });
    self.onrtctransform({ transformer: { readable, writable, options: { kind, mid } } });
    await new Promise((r) => setTimeout(r, 20));
    return out;
  };

  const plain = Uint8Array.from([0xf8, 9, 8, 7]);
  const [sealed] = await run('sender', null, [plain]);
  assert.deepEqual(E2eeSenderKey.peek(sealed), { generation: 2, counter: 0 });
  const own = await E2eeSenderKey.derive(2, filled(32, 1));
  assert.deepEqual(await own.open(sealed), plain);

  const bobKey = await E2eeSenderKey.derive(4, filled(32, 2));
  const fromBob = await bobKey.seal(11, plain);
  const received = await run('receiver', '1', [fromBob, fromBob, plain, sealed]);
  assert.deepEqual(received, [plain]); // replay, plaintext and a foreign generation are dropped
  assert.deepEqual(await run('receiver', '0', [fromBob]), []); // the mix carries nothing decryptable

  post({ type: 'stats' });
  const stats = posted.find((m) => m.type === 'stats');
  assert.deepEqual({ ...stats.stats }, { framesE2ee: 2, undecryptable: 3, held: 0 }); // 1 sealed + 1 opened; the unmapped mix is not an error
  post({ type: 'mode', mode: 'hold' });
  assert.deepEqual(await run('sender', null, [plain]), []);
  post({ type: 'forgetPeer', userId: 'bob' });
  post({ type: 'mode', mode: 'encrypt' });
  assert.deepEqual(await run('receiver', '1', [await bobKey.seal(12, plain)]), []);
});

test('capability detection needs WebCrypto plus an encoded-frame API', () => {
  const s = detectE2eeSupport();
  assert.equal(s.crypto, true);
  assert.equal(s.transform, undefined);
  assert.equal(s.ok, false);
  globalThis.RTCRtpScriptTransform = class {};
  globalThis.Worker = class {};
  globalThis.Blob = globalThis.Blob ?? class {};
  try {
    assert.deepEqual(detectE2eeSupport(), { crypto: true, transform: 'script', ok: true });
    assert.equal(detectE2eeSupport('streams').ok, false);
    class FakeSender {
      createEncodedStreams() {}
    }
    globalThis.RTCRtpSender = FakeSender;
    globalThis.RTCRtpReceiver = FakeSender;
    assert.equal(detectE2eeSupport('streams').transform, 'streams');
    assert.equal(detectE2eeSupport('auto').transform, 'script');
    delete globalThis.RTCRtpScriptTransform;
    assert.equal(detectE2eeSupport('auto').transform, 'streams');
    assert.equal(detectE2eeSupport('script').ok, false);
  } finally {
    delete globalThis.RTCRtpScriptTransform;
    delete globalThis.Worker;
    delete globalThis.RTCRtpSender;
    delete globalThis.RTCRtpReceiver;
  }
});
