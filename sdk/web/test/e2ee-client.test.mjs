import { test } from 'node:test';
import assert from 'node:assert/strict';
import vm from 'node:vm';
import { resolveObjectURL } from 'node:buffer';
import { AurixClient } from '../dist/client.js';
import { E2eeIdentity, E2eeSenderKey, base64ToBytes, bytesToBase64 } from '../dist/e2ee.js';
import { FakeAudioContext } from './helpers/fake-audio.mjs';

// ── Browser stand-ins ──

class FakeSocket {
  static OPEN = 1;
  static CLOSED = 3;
  static last;
  constructor(url) {
    this.url = url;
    this.readyState = FakeSocket.OPEN;
    this.sent = [];
    FakeSocket.last = this;
    queueMicrotask(() => this.onopen?.({}));
  }
  send(data) {
    const msg = JSON.parse(data);
    this.sent.push(msg);
    this.onsent?.(msg);
  }
  close(code = 1000, reason = '') {
    if (this.readyState === FakeSocket.CLOSED) return;
    this.readyState = FakeSocket.CLOSED;
    this.onclose?.({ code, reason });
  }
  receive(msg) {
    this.onmessage?.({ data: JSON.stringify(msg) });
  }
}

class FakeTrack extends EventTarget {
  constructor(kind = 'audio') {
    super();
    this.kind = kind;
    this.enabled = true;
    this.readyState = 'live';
  }
  stop() {
    this.readyState = 'ended';
  }
}

class FakeMediaStream {
  constructor(tracks = []) {
    this.tracks = tracks;
  }
  getAudioTracks() {
    return this.tracks.filter((t) => t.kind === 'audio');
  }
  getTracks() {
    return [...this.tracks];
  }
}

/** One encoded-frame pipeline end (`createEncodedStreams()`): frames go in, transformed frames come out. */
class EncodedPipe {
  constructor() {
    this.out = [];
    this.readable = new ReadableStream({ start: (c) => (this.controller = c) });
    this.writable = new WritableStream({ write: (f) => void this.out.push(new Uint8Array(f.data)) });
  }
  push(bytes) {
    const data = bytes.buffer.slice(bytes.byteOffset, bytes.byteOffset + bytes.byteLength);
    this.controller.enqueue({ data, timestamp: 0 });
    return new Promise((r) => setTimeout(r, 15));
  }
}

class FakeRtpEnd {
  constructor(track) {
    this.track = track;
    this.transform = undefined;
    this.pipe = undefined;
  }
  getParameters() {
    return { encodings: [{}] };
  }
  async setParameters() {}
  async replaceTrack() {}
}
// Chromium's legacy API lives on the prototypes; the standard one is a `transform` property.
FakeRtpEnd.prototype.createEncodedStreams = function createEncodedStreams() {
  if (this.pipe) throw new Error('createEncodedStreams called twice');
  this.pipe = new EncodedPipe();
  return { readable: this.pipe.readable, writable: this.pipe.writable };
};

class FakePeerConnection extends EventTarget {
  static all = [];
  constructor(config) {
    super();
    this.config = config;
    this.transceivers = [];
    this.connectionState = 'new';
    this.iceGatheringState = 'complete';
    this.localDescription = null;
    FakePeerConnection.all.push(this);
  }
  addTransceiver(trackOrKind, init = {}) {
    const t = {
      mid: null,
      direction: init.direction ?? 'sendrecv',
      sender: new FakeRtpEnd(typeof trackOrKind === 'string' ? null : trackOrKind),
      receiver: new FakeRtpEnd(new FakeTrack()),
    };
    this.transceivers.push(t);
    return t;
  }
  getTransceivers() {
    return [...this.transceivers];
  }
  getSenders() {
    return this.transceivers.map((t) => t.sender);
  }
  async createOffer() {
    this.transceivers.forEach((t, i) => {
      t.mid = String(i);
    });
    return { type: 'offer', sdp: `v=0\r\n${this.transceivers.map((t) => `m=audio 9 UDP/TLS/RTP/SAVPF 111\r\na=mid:${t.mid}\r\n`).join('')}` };
  }
  async setLocalDescription(desc) {
    this.localDescription = desc;
  }
  async setRemoteDescription() {}
  connect() {
    this.connectionState = 'connected';
    this.onconnectionstatechange?.();
  }
  close() {
    this.connectionState = 'closed';
  }
}

/** `Worker` that really runs the SDK's transform worker source (from its `blob:` URL) in a bare realm. */
class FakeWorker extends EventTarget {
  static last;
  constructor(url) {
    super();
    FakeWorker.last = this;
    this.scope = { postMessage: (m) => this.dispatchEvent(Object.assign(new Event('message'), { data: m })), onmessage: null, onrtctransform: null };
    this.pending = [];
    this.ready = resolveObjectURL(url)
      .text()
      .then((src) => {
        const context = vm.createContext({ self: this.scope, crypto: globalThis.crypto, TransformStream });
        vm.runInContext(src, context, { filename: 'e2ee-worker.js' });
        for (const m of this.pending) this.scope.onmessage({ data: m });
        this.pending = undefined;
      });
  }
  postMessage(m) {
    if (this.pending) this.pending.push(m);
    else this.scope.onmessage({ data: m });
  }
  terminate() {}
}

/** `RTCRtpScriptTransform`: hands the worker a transformer for the sender/receiver it is set on. */
class FakeScriptTransform {
  constructor(worker, options) {
    this.worker = worker;
    this.options = options;
    this.pipe = new EncodedPipe();
    void worker.ready.then(() => worker.scope.onrtctransform({ transformer: { readable: this.pipe.readable, writable: this.pipe.writable, options } }));
  }
}

globalThis.WebSocket = FakeSocket;
globalThis.RTCPeerConnection = FakePeerConnection;
globalThis.MediaStream = FakeMediaStream;

function withStreamsApi(on) {
  if (on) {
    globalThis.RTCRtpSender = FakeRtpEnd;
    globalThis.RTCRtpReceiver = FakeRtpEnd;
  } else {
    delete globalThis.RTCRtpSender;
    delete globalThis.RTCRtpReceiver;
  }
}
function withScriptApi(on) {
  if (on) {
    globalThis.RTCRtpScriptTransform = FakeScriptTransform;
    globalThis.Worker = FakeWorker;
  } else {
    delete globalThis.RTCRtpScriptTransform;
    delete globalThis.Worker;
  }
}

const tick = (ms = 0) => new Promise((r) => setTimeout(r, ms));
const ack = (extra = {}) => ({
  type: 'SessionInitAck',
  data: { session_id: 's1', ssrc: 7, media_addr: '10.0.0.2:40000', media_key: 'AAAA', webrtc_participant_streams: 2, ...extra },
});
const brief = (user_id, ssrc) => ({ user_id, display_name: user_id, ssrc, role: 'speaker', is_muted: false, is_speaking: false });
const filled = (n, v) => new Uint8Array(n).fill(v);

async function connected(opts = {}) {
  const client = new AurixClient({
    apiUrl: 'http://api',
    wsUrl: 'wss://a/ws',
    token: 't',
    autoReconnect: false,
    pingIntervalMs: 0,
    qualityReportIntervalMs: 0,
    useTurn: false,
    localVoiceActivity: false,
    localStream: new FakeMediaStream([new FakeTrack()]),
    audioContext: new FakeAudioContext(),
    ...opts,
  });
  const errors = [];
  client.on('error', (e) => errors.push(e.message));
  const before = FakeSocket.last;
  const connecting = client.connect();
  while (FakeSocket.last === before) await tick(1);
  const sock = FakeSocket.last;
  sock.onsent = (msg) => {
    if (msg.type === 'WebRtcOffer') queueMicrotask(() => sock.receive({ type: 'WebRtcAnswer', data: { sdp: 'v=0\r\nanswer' } }));
  };
  sock.receive(ack());
  await connecting;
  const pc = FakePeerConnection.all.at(-1);
  pc.connect();
  return { client, sock, pc, errors };
}

async function joinEncrypted(client, sock, channelId, participants) {
  const joining = client.joinChannel(channelId);
  await tick();
  sock.receive({ type: 'ChannelJoinAck', data: { channel_id: channelId, participants, audio: { e2ee: true } } });
  return joining;
}

/** The peer side of the key exchange, driven by hand through the fake socket. */
async function peer(userId, seed) {
  return { userId, identity: await E2eeIdentity.create(filled(32, seed)), secret: filled(32, seed + 100), generation: 3 };
}
function helloFrom(sock, p, channelId) {
  sock.receive({ type: 'E2eeHello', data: { channel_id: channelId, user_id: p.userId, public_key: bytesToBase64(p.identity.publicKey) } });
}
async function keyFrom(sock, p, channelId, clientPublicKey) {
  const wrapped = await p.identity.wrap(clientPublicKey, p.generation, p.secret);
  sock.receive({
    type: 'E2eeSenderKey',
    data: { channel_id: channelId, from: p.userId, to: 'me', public_key: bytesToBase64(p.identity.publicKey), generation: p.generation, key: bytesToBase64(wrapped) },
  });
}
const senderKeys = (sock) => sock.sent.filter((m) => m.type === 'E2eeSenderKey');
const hellos = (sock) => sock.sent.filter((m) => m.type === 'E2eeHello');

// ── No capability: explicit failure, never a plaintext downgrade ──

test('without an encoded-frame API, E2EE is unavailable and encrypted channels are refused', async () => {
  withStreamsApi(false);
  withScriptApi(false);
  const { client, sock, errors } = await connected({ e2ee: true });
  assert.equal(client.e2eeAvailable, false);
  assert.equal(client.e2eeTransformApi, undefined);
  assert.ok(errors.some((m) => /E2EE unavailable/.test(m)), errors.join('; '));
  assert.equal(hellos(sock).length, 0);

  // A misbehaving server acks an encrypted channel anyway: the client leaves it at once.
  await assert.rejects(joinEncrypted(client, sock, 'secret', [brief('me', 7)]), /E2EE_REQUIRED/);
  assert.ok(sock.sent.some((m) => m.type === 'ChannelLeave' && m.data.channel_id === 'secret'));
  assert.equal(client.isChannelEncrypted('secret'), false);

  // Plain channels still work.
  const joining = client.joinChannel('plain');
  await tick();
  sock.receive({ type: 'ChannelJoinAck', data: { channel_id: 'plain', participants: [brief('me', 7)] } });
  await joining;
  client.disconnect();
});

test('e2ee: false disables the feature silently; an explicit identity keeps its fingerprint', async () => {
  withStreamsApi(true);
  const off = await connected({ e2ee: false });
  assert.equal(off.client.e2eeAvailable, false);
  assert.equal(hellos(off.sock).length, 0);
  off.client.disconnect();

  const secret = filled(32, 1);
  const { client, sock } = await connected({ e2ee: { identity: secret } });
  assert.equal(client.e2eeAvailable, true);
  assert.equal(client.e2eeTransformApi, 'streams');
  assert.equal(client.e2eeFingerprint, '1a92f23852dc908d97316a3b13578281196c1dd73d9ae5e313f3fb6b8954bf55');
  assert.deepEqual(client.e2eeIdentitySecret, secret);
  assert.equal(client.e2eeGeneration, 0);
  // Capability announcement right after the session ack: no channel, our public key.
  const [hello] = hellos(sock);
  assert.equal(hello.data.channel_id, undefined);
  assert.equal(base64ToBytes(hello.data.public_key).length, 32);
  client.disconnect();
  withStreamsApi(false);
});

// ── Full flow over both encoded-frame APIs ──

for (const api of ['streams', 'script']) {
  test(`[${api}] encrypted channel: hold → hello → keys → rotation on join/leave, frames sealed and opened`, async () => {
    withStreamsApi(api === 'streams');
    withScriptApi(api === 'script');
    try {
      const { client, sock, pc, errors } = await connected({ e2ee: true, participantStreams: 2 });
      assert.equal(client.e2eeTransformApi, api);
      const events = [];
      client.on('e2eePeerKey', (u, fp, prev) => events.push(['key', u, fp, prev]));
      client.on('e2eePeerDecryptable', (u, ok) => events.push(['decryptable', u, ok]));
      client.on('e2eeKeyRotated', (g) => events.push(['rotated', g]));
      const myPk = base64ToBytes(hellos(sock)[0].data.public_key);
      if (api === 'streams') assert.equal(pc.config.encodedInsertableStreams, true);
      else assert.equal(pc.config.encodedInsertableStreams, undefined);

      const [mixed, t1] = pc.transceivers;
      const pipeOf = (end) => (api === 'streams' ? end.pipe : end.transform.pipe);
      if (api === 'script') {
        assert.deepEqual(mixed.sender.transform.options, { kind: 'sender', mid: '0' });
        assert.deepEqual(t1.receiver.transform.options, { kind: 'receiver', mid: '1' });
        assert.equal(mixed.sender.transform.worker, FakeWorker.last);
        await FakeWorker.last.ready;
      }
      const up = pipeOf(mixed.sender);
      const down1 = pipeOf(t1.receiver);
      const downMixed = pipeOf(mixed.receiver);
      const opus = Uint8Array.from([0xf8, 1, 2, 3, 4]);

      // Plain mode before any join: frames pass untouched both ways.
      await up.push(opus);
      assert.deepEqual(up.out, [opus]);
      await downMixed.push(opus);
      assert.deepEqual(downMixed.out, [opus]);

      // Join in flight: uplink is held (nothing leaves until the ack says plain or encrypted).
      const joining = client.joinChannel('war-room');
      await tick();
      await up.push(opus);
      assert.equal(up.out.length, 1);
      sock.receive({ type: 'ChannelJoinAck', data: { channel_id: 'war-room', participants: [brief('me', 7), brief('bob', 8)], audio: { e2ee: true } } });
      await joining;
      assert.equal(client.isChannelEncrypted('war-room'), true);
      assert.equal(hellos(sock).length, 2);
      assert.equal(hellos(sock)[1].data.channel_id, 'war-room');

      // Encrypted mode with no peers yet: uplink frames are sealed under our generation-0 key.
      await up.push(opus);
      assert.equal(up.out.length, 2);
      assert.deepEqual(E2eeSenderKey.peek(up.out[1]), { generation: 0, counter: 0 });
      // Downlink on the mix or an unmapped track is silenced.
      await downMixed.push(opus);
      assert.equal(downMixed.out.length, 1);

      // Bob announces himself: we learn his fingerprint, rotate (debounced) and wrap the new key for him.
      const bob = await peer('bob', 2);
      helloFrom(sock, bob, 'war-room');
      await tick(80);
      assert.deepEqual(events[0], ['key', 'bob', bob.identity.fingerprint, undefined]);
      assert.deepEqual(events[1], ['rotated', 1]);
      assert.equal(client.e2eeGeneration, 1);
      assert.equal(client.e2eePeerFingerprint('bob'), bob.identity.fingerprint);
      const [k1] = senderKeys(sock);
      assert.equal(k1.data.to, 'bob');
      assert.equal(k1.data.generation, 1);
      assert.equal(k1.data.channel_id, 'war-room');
      assert.deepEqual(base64ToBytes(k1.data.public_key), myPk);
      const mySecret = await bob.identity.unwrap(myPk, 1, base64ToBytes(k1.data.key));
      const myKey = await E2eeSenderKey.derive(1, mySecret);

      // Our uplink is now readable by Bob (and by nobody without the wrapped key).
      await up.push(opus);
      const sealed = up.out.at(-1);
      assert.deepEqual(E2eeSenderKey.peek(sealed).generation, 1);
      assert.deepEqual(await myKey.open(sealed), opus);

      // Bob keys us: decryptable, and his frames on his track play; replay/plaintext do not.
      assert.equal(client.isE2eePeerDecryptable('bob'), false);
      await keyFrom(sock, bob, 'war-room', myPk);
      await tick(20);
      assert.deepEqual(events.at(-1), ['decryptable', 'bob', true]);
      assert.deepEqual(client.e2eeDecryptablePeers(), ['bob']);
      sock.receive({ type: 'ParticipantStreams', data: { streams: [{ mid: '1', user_id: 'bob' }] } });
      await tick(20);
      const bobKey = await E2eeSenderKey.derive(bob.generation, bob.secret);
      const fromBob = await bobKey.seal(5, opus);
      await down1.push(fromBob);
      assert.deepEqual(down1.out, [opus]);
      await down1.push(fromBob);
      await down1.push(opus);
      assert.equal(down1.out.length, 1);
      const stats = await client.refreshE2eeStats();
      assert.ok(stats.framesE2ee >= 3, JSON.stringify(stats));
      assert.equal(stats.undecryptable, 2);
      assert.equal(stats.held, 1);

      // A forged key (someone else pretending to be Bob) is rejected and reported, not installed.
      const mallory = await peer('bob', 9);
      const forged = await mallory.identity.wrap(myPk, 4, filled(32, 4));
      sock.receive({
        type: 'E2eeSenderKey',
        data: { channel_id: 'war-room', from: 'bob', to: 'me', public_key: bytesToBase64(bob.identity.publicKey), generation: 4, key: bytesToBase64(forged) },
      });
      await tick(20);
      assert.ok(errors.some((m) => /E2EE key from bob rejected/.test(m)), errors.join('; '));
      assert.equal(client.e2eeGeneration, 1);

      // Carol joins: one more rotation, wrapped for both.
      const carol = await peer('carol', 3);
      sock.receive({ type: 'ParticipantJoined', data: { channel_id: 'war-room', ...brief('carol', 9) } });
      helloFrom(sock, carol, 'war-room');
      await tick(80);
      assert.equal(client.e2eeGeneration, 2);
      const gen2 = senderKeys(sock).filter((m) => m.data.generation === 2);
      assert.deepEqual(gen2.map((m) => m.data.to).sort(), ['bob', 'carol']);
      // Bob's old generation is still accepted (kept generations), a fresh one from him too.
      await down1.push(await bobKey.seal(6, opus));
      assert.equal(down1.out.length, 2);

      // Bob leaves: he is no longer decryptable, and the key rotates so he cannot follow along.
      sock.receive({ type: 'ParticipantLeft', data: { channel_id: 'war-room', user_id: 'bob' } });
      await tick(80);
      assert.deepEqual(events.at(-2), ['decryptable', 'bob', false]);
      assert.deepEqual(events.at(-1), ['rotated', 3]);
      assert.deepEqual(client.e2eeDecryptablePeers(), []);
      assert.equal(client.e2eePeerFingerprint('bob'), undefined);
      const gen3 = senderKeys(sock).filter((m) => m.data.generation === 3);
      assert.deepEqual(gen3.map((m) => m.data.to), ['carol']);
      await down1.push(await bobKey.seal(7, opus));
      assert.equal(down1.out.length, 2);

      // Leaving the last encrypted channel returns to plaintext: frames pass again.
      client.leaveChannel('war-room');
      await tick(80);
      assert.equal(client.isChannelEncrypted('war-room'), false);
      await up.push(opus);
      assert.deepEqual(up.out.at(-1), opus);
      await downMixed.push(opus);
      assert.deepEqual(downMixed.out.at(-1), opus);
      client.disconnect();
    } finally {
      withStreamsApi(false);
      withScriptApi(false);
    }
  });
}

test('a session resumed after reconnect re-announces and drops peers that left meanwhile', async () => {
  withStreamsApi(true);
  try {
    const { client, sock } = await connected({ e2ee: true, participantStreams: 2, autoReconnect: true, reconnect: { initialDelayMs: 1, maxDelayMs: 1, maxAttempts: 2 } });
    const myPk = base64ToBytes(hellos(sock)[0].data.public_key);
    const events = [];
    client.on('e2eePeerDecryptable', (u, ok) => events.push([u, ok]));
    await joinEncrypted(client, sock, 'ch', [brief('me', 7), brief('bob', 8), brief('carol', 9)]);
    const bob = await peer('bob', 2);
    const carol = await peer('carol', 3);
    await keyFrom(sock, bob, 'ch', myPk);
    await keyFrom(sock, carol, 'ch', myPk);
    await tick(80);
    assert.deepEqual(client.e2eeDecryptablePeers().sort(), ['bob', 'carol']);
    const fp = client.e2eeFingerprint;

    // Control socket drops; the reconnect resumes the session and replays the join ack without Carol.
    sock.close(1006, 'lost');
    await tick(10);
    const sock2 = FakeSocket.last;
    assert.notEqual(sock2, sock);
    sock2.onsent = (msg) => {
      if (msg.type === 'WebRtcOffer') queueMicrotask(() => sock2.receive({ type: 'WebRtcAnswer', data: { sdp: 'v=0\r\nanswer' } }));
    };
    sock2.receive(ack({ resumed: true }));
    await tick(10);
    sock2.receive({ type: 'ChannelJoinAck', data: { channel_id: 'ch', participants: [brief('me', 7), brief('bob', 8)], audio: { e2ee: true } } });
    await tick(80);
    assert.equal(client.e2eeFingerprint, fp);
    assert.deepEqual(events.at(-1), ['carol', false]);
    assert.deepEqual(client.e2eeDecryptablePeers(), ['bob']);
    assert.ok(hellos(sock2).some((m) => m.data.channel_id === undefined));
    assert.ok(hellos(sock2).some((m) => m.data.channel_id === 'ch'));
    assert.equal(client.isChannelEncrypted('ch'), true);
    client.disconnect();
  } finally {
    withStreamsApi(false);
  }
});
