import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import vm from 'node:vm';
import { AurixBridge } from '../dist/bridge.js';

/** Minimal stand-in for AurixClient: records calls, lets tests fire events. */
class FakeClient {
  constructor(options) {
    this.options = options;
    this.calls = [];
    this.listeners = new Map();
    this.connectionState = 'disconnected';
    this.sessionInfo = undefined;
    this.isMuted = false;
    this.transcriptsEnabled = false;
    this.inputGain = 1;
    this.attached = [];
  }
  on(event, listener) {
    let set = this.listeners.get(event);
    if (!set) this.listeners.set(event, (set = new Set()));
    set.add(listener);
    return () => set.delete(listener);
  }
  emit(event, ...args) {
    for (const l of this.listeners.get(event) ?? []) l(...args);
  }
  record(name, ...args) {
    this.calls.push([name, ...args]);
  }
  connect() {
    this.record('connect');
    this.connectionState = 'connecting';
    return new Promise((resolve, reject) => {
      this.resolveConnect = resolve;
      this.rejectConnect = reject;
    });
  }
  disconnect(reason) {
    this.record('disconnect', reason);
    this.connectionState = 'disconnected';
  }
  resumeAudio() {
    this.record('resumeAudio');
    return Promise.resolve(this.audioRunning ?? true);
  }
  joinChannel(channelId, joinToken) {
    this.record('joinChannel', channelId, joinToken);
    if (joinToken === undefined && this.options.joinToken) {
      return this.options.joinToken(channelId).then((t) => [{ userId: 'u2', displayName: 'Bob', ssrc: 2, role: 'speaker', muted: false, serverMuted: false, speaking: false, energy: 0, token: t }]);
    }
    if (channelId === 'full') return Promise.reject(Object.assign(new Error('channel is full'), { code: 'CHANNEL_FULL' }));
    return Promise.resolve([]);
  }
  leaveChannel(channelId) {
    this.record('leaveChannel', channelId);
  }
  joinedChannels() {
    return ['c1'];
  }
  setMuted(m) {
    this.record('setMuted', m);
    this.isMuted = m;
  }
  setTransmission(mode) {
    this.record('setTransmission', mode);
  }
  setParticipantMuted(userId, muted, channelId) {
    this.record('setParticipantMuted', userId, muted, channelId);
  }
  sendMessage(channelId, text, options) {
    this.record('sendMessage', channelId, text, options);
    return Promise.resolve({ id: 'm1', channelId, fromUserId: 'me', displayName: 'Me', text, sentAt: new Date(0), own: true, system: false });
  }
  history(scope, options) {
    this.record('history', scope, options);
    return Promise.resolve({ messages: [] });
  }
  speak(text, options) {
    this.record('speak', text, options);
    return Promise.resolve({ requestId: 'r1', clientRef: options.clientRef ?? 'auto', done: new Promise(() => {}) });
  }
  updatePosition(channelId, position, orientation) {
    this.record('updatePosition', channelId, position, orientation);
  }
  attachAudioOutput(el) {
    this.attached.push(el);
  }
  detachAudioOutput() {}
  get e2eeAvailable() {
    return this.e2ee !== undefined;
  }
  get e2eeTransformApi() {
    return this.e2ee?.api;
  }
  get e2eeFingerprint() {
    return this.e2ee?.fingerprint;
  }
  get e2eeIdentitySecret() {
    return this.e2ee?.secret;
  }
  get e2eeGeneration() {
    return this.e2ee?.generation;
  }
  get e2eeStats() {
    return this.e2ee?.stats;
  }
  e2eePeerFingerprint(userId) {
    this.record('e2eePeerFingerprint', userId);
    return this.e2ee?.peers.get(userId);
  }
  isE2eePeerDecryptable(userId) {
    this.record('isE2eePeerDecryptable', userId);
    return this.e2ee?.peers.has(userId) ?? false;
  }
  e2eeDecryptablePeers() {
    return [...(this.e2ee?.peers.keys() ?? [])];
  }
  isChannelEncrypted(channelId) {
    this.record('isChannelEncrypted', channelId);
    return channelId === 'secret';
  }
  refreshE2eeStats() {
    this.record('refreshE2eeStats');
    return Promise.resolve(this.e2ee?.stats);
  }
  rotateE2eeKey() {
    this.record('rotateE2eeKey');
    if (!this.e2ee) return Promise.resolve(undefined);
    this.e2ee.generation += 1;
    return Promise.resolve(this.e2ee.generation);
  }
  setPriority(channelId, priority, userId) {
    this.record('setPriority', channelId, priority, userId);
  }
  isPriority(channelId) {
    return channelId === 'raid';
  }
  getChannelDucking(channelId) {
    return channelId === 'raid' ? { gain: 0.3, attackMs: 40, releaseMs: 300, holdMs: 200, moderators: true } : undefined;
  }
  isDuckingActive(channelId) {
    return channelId === 'raid';
  }
  setVoiceEffects(effects) {
    this.record('setVoiceEffects', effects);
    this.effects = effects;
    return Promise.resolve();
  }
  get voiceEffects() {
    return this.effects ?? { ringModHz: 0 };
  }
  setVisemes(enabled) {
    this.record('setVisemes', enabled);
    this.visemes = enabled;
    return Promise.resolve();
  }
  get visemesEnabled() {
    return this.visemes === true;
  }
  getParticipantVisemes(userId) {
    return userId === 'alice' ? { dominant: 'AA', mouthOpen: 0.7, sequence: 3 } : undefined;
  }
  getLocalVisemes() {
    return this.visemes ? { dominant: 'silence', mouthOpen: 0, sequence: 0 } : undefined;
  }
}

function make(extraOptions = {}, bridgeOptions = {}) {
  let client;
  const bridge = new AurixBridge({
    document: null,
    createClient: (o) => (client = new FakeClient(o)),
    ...bridgeOptions,
  });
  const handle = bridge.create(JSON.stringify({ apiUrl: 'http://api', wsUrl: 'ws://ws', token: 't', ...extraOptions }));
  return { bridge, handle, client };
}

const ok = (json) => {
  const r = JSON.parse(json);
  assert.equal(r.ok, true, json);
  return r;
};
const drain = (bridge, handle) => JSON.parse(bridge.drain(handle));
const tick = () => new Promise((r) => setTimeout(r, 0));

test('create validates options and forwards only the ones given', () => {
  const bridge = new AurixBridge({ document: null, createClient: (o) => new FakeClient(o) });
  assert.throws(() => bridge.create(JSON.stringify({ apiUrl: 'x' })), /required/);
  assert.throws(() => bridge.create('[1]'), /object/);
  const { client, handle } = make({ useTurn: false, pingIntervalMs: 5000, opus: { stereo: true } });
  assert.equal(handle, 1);
  assert.deepEqual(client.options, { apiUrl: 'http://api', wsUrl: 'ws://ws', token: 't', useTurn: false, pingIntervalMs: 5000, opus: { stereo: true } });
  assert.equal(client.options.refreshToken, undefined);
});

test('e2ee options: booleans pass through, the base64 identity becomes the 32-byte secret', () => {
  assert.equal(make({ e2ee: false }).client.options.e2ee, false);
  assert.equal(make({ e2ee: true }).client.options.e2ee, true);
  const secret = Uint8Array.from({ length: 32 }, (_, i) => i);
  const identity = Buffer.from(secret).toString('base64');
  const { client } = make({ e2ee: { identity, transform: 'streams', workerUrl: '/w.js' } });
  assert.deepEqual(client.options.e2ee, { identity: secret, transform: 'streams', workerUrl: '/w.js' });
  assert.ok(client.options.e2ee.identity instanceof Uint8Array);
  assert.deepEqual(make({ e2ee: {} }).client.options.e2ee, {});
  const bridge = new AurixBridge({ document: null, createClient: (o) => new FakeClient(o) });
  assert.throws(
    () => bridge.create(JSON.stringify({ apiUrl: 'a', wsUrl: 'w', token: 't', e2ee: { identity: 'AAEC' } })),
    /32-byte/,
  );
});

test('e2ee state, stats and rotation are exposed; events are queued with named fields', async () => {
  const { bridge, handle, client } = make({ e2ee: true });
  const v = (method, args = {}) => ok(bridge.invoke(handle, method, JSON.stringify(args))).value;
  assert.equal(v('e2eeAvailable'), false);
  assert.equal(v('e2eeTransformApi'), null);
  assert.equal(v('e2eeFingerprint'), null);
  assert.equal(v('e2eeIdentitySecret'), null);
  assert.equal(v('e2eeGeneration'), null);
  assert.equal(v('e2eeStats'), null);
  assert.deepEqual(v('e2eeDecryptablePeers'), []);
  assert.equal(v('isE2eePeerDecryptable', { userId: 'bob' }), false);

  const secret = Uint8Array.from({ length: 32 }, (_, i) => 255 - i);
  client.e2ee = {
    api: 'script',
    fingerprint: 'ab12',
    secret,
    generation: 2,
    stats: { framesE2ee: 10, undecryptable: 1, held: 0 },
    peers: new Map([['alice', 'cd34']]),
  };
  assert.equal(v('e2eeAvailable'), true);
  assert.equal(v('e2eeTransformApi'), 'script');
  assert.equal(v('e2eeFingerprint'), 'ab12');
  assert.deepEqual(Uint8Array.from(Buffer.from(v('e2eeIdentitySecret'), 'base64')), secret);
  assert.equal(v('e2eeGeneration'), 2);
  assert.deepEqual(v('e2eeStats'), { framesE2ee: 10, undecryptable: 1, held: 0 });
  assert.equal(v('e2eePeerFingerprint', { userId: 'alice' }), 'cd34');
  assert.equal(v('e2eePeerFingerprint', { userId: 'bob' }), null);
  assert.equal(v('isE2eePeerDecryptable', { userId: 'alice' }), true);
  assert.deepEqual(v('e2eeDecryptablePeers'), ['alice']);
  assert.equal(v('isChannelEncrypted', { channelId: 'secret' }), true);
  assert.equal(v('isChannelEncrypted', { channelId: 'open' }), false);
  assert.match(JSON.parse(bridge.invoke(handle, 'isChannelEncrypted', '{}')).error.message, /channelId/);

  const stats = JSON.parse(bridge.invoke(handle, 'refreshE2eeStats', '{}', 11));
  assert.equal(stats.pending, true);
  const rotate = JSON.parse(bridge.invoke(handle, 'rotateE2eeKey', '{}', 12));
  assert.equal(rotate.pending, true);
  await tick();
  assert.deepEqual(drain(bridge, handle), [
    { type: 'result', rid: 11, ok: true, value: { framesE2ee: 10, undecryptable: 1, held: 0 } },
    { type: 'result', rid: 12, ok: true, value: 3 },
  ]);

  client.emit('e2eePeerKey', 'alice', 'cd34', undefined);
  client.emit('e2eePeerKey', 'alice', 'ef56', 'cd34');
  client.emit('e2eePeerDecryptable', 'alice', true);
  client.emit('e2eeKeyRotated', 4);
  assert.deepEqual(drain(bridge, handle), [
    { type: 'e2eePeerKey', userId: 'alice', fingerprint: 'cd34', previousFingerprint: null },
    { type: 'e2eePeerKey', userId: 'alice', fingerprint: 'ef56', previousFingerprint: 'cd34' },
    { type: 'e2eePeerDecryptable', userId: 'alice', decryptable: true },
    { type: 'e2eeKeyRotated', generation: 4 },
  ]);
});

test('priority/ducking, voice effects and visemes are mapped; viseme frames are queued only on request', async () => {
  const { bridge, handle, client } = make({ visemes: true, voiceEffects: 'robot' });
  assert.equal(client.options.visemes, true);
  assert.equal(client.options.voiceEffects, 'robot');
  const v = (method, args = {}) => ok(bridge.invoke(handle, method, JSON.stringify(args))).value;

  v('setPriority', { channelId: 'raid', priority: true });
  v('setPriority', { channelId: 'raid', priority: false, userId: 'bob' });
  assert.deepEqual(client.calls.slice(-2), [
    ['setPriority', 'raid', true, undefined],
    ['setPriority', 'raid', false, 'bob'],
  ]);
  assert.equal(v('isPriority', { channelId: 'raid' }), true);
  assert.equal(v('isDuckingActive', { channelId: 'lobby' }), false);
  assert.deepEqual(v('channelDucking', { channelId: 'raid' }), { gain: 0.3, attackMs: 40, releaseMs: 300, holdMs: 200, moderators: true });
  assert.equal(v('channelDucking', { channelId: 'lobby' }), null);
  assert.match(JSON.parse(bridge.invoke(handle, 'setPriority', '{"channelId":"raid"}')).error.message, /priority/);

  assert.equal(typeof v('supportsVoiceEffects'), 'boolean');
  assert.equal(typeof v('supportsVisemes'), 'boolean');
  assert.equal(JSON.parse(bridge.invoke(handle, 'setVoiceEffects', '{"effects":"radio"}', 21)).pending, true);
  assert.equal(JSON.parse(bridge.invoke(handle, 'setVoiceEffects', '{"effects":{"pitchSemitones":4}}', 22)).pending, true);
  assert.equal(JSON.parse(bridge.invoke(handle, 'setVoiceEffects', '{"effects":null}', 23)).pending, true);
  assert.match(JSON.parse(bridge.invoke(handle, 'setVoiceEffects', '{"effects":7}')).error.message, /preset/);
  assert.deepEqual(client.calls.slice(-3), [
    ['setVoiceEffects', 'radio'],
    ['setVoiceEffects', { pitchSemitones: 4 }],
    ['setVoiceEffects', undefined],
  ]);
  assert.deepEqual(v('voiceEffects'), { ringModHz: 0 });

  assert.equal(v('visemesEnabled'), false);
  assert.equal(v('localVisemes'), null);
  assert.equal(JSON.parse(bridge.invoke(handle, 'setVisemes', '{"enabled":true}', 24)).pending, true);
  assert.equal(v('visemesEnabled'), true);
  assert.deepEqual(v('localVisemes'), { dominant: 'silence', mouthOpen: 0, sequence: 0 });
  assert.deepEqual(v('participantVisemes', { userId: 'alice' }), { dominant: 'AA', mouthOpen: 0.7, sequence: 3 });
  assert.equal(v('participantVisemes', { userId: 'bob' }), null);
  await tick();
  assert.deepEqual(drain(bridge, handle).map((e) => e.rid), [21, 22, 23, 24]);

  const cfg = { gain: 0.3, attackMs: 40, releaseMs: 300, holdMs: 200, moderators: true };
  client.emit('participantPriorityChanged', 'raid', 'bob', true);
  client.emit('duckingChanged', 'raid', true, cfg);
  client.emit('participantVisemes', 'alice', { dominant: 'OH', mouthOpen: 0.5, sequence: 9 });
  client.emit('localVisemes', { dominant: 'E', mouthOpen: 0.2, sequence: 1 });
  assert.deepEqual(drain(bridge, handle), [
    { type: 'participantPriorityChanged', channelId: 'raid', userId: 'bob', priority: true },
    { type: 'duckingChanged', channelId: 'raid', active: true, config: cfg },
  ]);

  const chatty = make({ visemeEvents: true });
  chatty.client.emit('participantVisemes', 'alice', { dominant: 'OH', mouthOpen: 0.5, sequence: 9 });
  chatty.client.emit('localVisemes', { dominant: 'E', mouthOpen: 0.2, sequence: 1 });
  assert.deepEqual(drain(chatty.bridge, chatty.handle), [
    { type: 'participantVisemes', userId: 'alice', frame: { dominant: 'OH', mouthOpen: 0.5, sequence: 9 } },
    { type: 'localVisemes', frame: { dominant: 'E', mouthOpen: 0.2, sequence: 1 } },
  ]);
});

test('sync methods answer inline, unknown methods and handles fail cleanly', () => {
  const { bridge, handle, client } = make();
  assert.deepEqual(ok(bridge.invoke(handle, 'joinedChannels', '{}')).value, ['c1']);
  assert.deepEqual(ok(bridge.invoke(handle, 'sessionInfo', '')).value, null);
  ok(bridge.invoke(handle, 'setMuted', JSON.stringify({ muted: true })));
  assert.equal(client.isMuted, true);
  assert.equal(ok(bridge.invoke(handle, 'isMuted', '{}')).value, true);
  ok(bridge.invoke(handle, 'setParticipantMuted', JSON.stringify({ userId: 'u2', muted: true })));
  assert.deepEqual(client.calls.at(-1), ['setParticipantMuted', 'u2', true, undefined]);
  const bad = JSON.parse(bridge.invoke(handle, 'setMuted', JSON.stringify({ muted: 'yes' })));
  assert.equal(bad.ok, false);
  assert.match(bad.error.message, /muted must be a boolean/);
  assert.match(JSON.parse(bridge.invoke(handle, 'nope', '{}')).error.message, /unknown method/);
  assert.match(JSON.parse(bridge.invoke(99, 'isMuted', '{}')).error.message, /unknown handle/);
  assert.equal(bridge.drain(handle), '[]');
});

test('transmission accepts SDK and wire spellings', () => {
  const { bridge, handle, client } = make();
  ok(bridge.invoke(handle, 'setTransmission', JSON.stringify({ mode: { mode: 'single', channel_id: 'c9' } })));
  ok(bridge.invoke(handle, 'setTransmission', JSON.stringify({ mode: { type: 'none' } })));
  assert.deepEqual(client.calls.map((c) => c[1]), [{ type: 'single', channelId: 'c9' }, { type: 'none' }]);
  assert.equal(JSON.parse(bridge.invoke(handle, 'setTransmission', JSON.stringify({ mode: { type: 'single' } }))).ok, false);
});

test('promises settle as result events keyed by rid; rid 0 failures become error events', async () => {
  const { bridge, handle, client } = make();
  assert.deepEqual(ok(bridge.invoke(handle, 'connect', '{}', 7)), { ok: true, pending: true });
  assert.equal(client.connectionState, 'connecting');
  client.resolveConnect({ sessionId: 's1', ssrc: 5, resumed: false });
  await tick();
  assert.deepEqual(drain(bridge, handle), [{ type: 'result', rid: 7, ok: true, value: { sessionId: 's1', ssrc: 5, resumed: false } }]);

  ok(bridge.invoke(handle, 'joinChannel', JSON.stringify({ channelId: 'full' }), 8));
  ok(bridge.invoke(handle, 'joinChannel', JSON.stringify({ channelId: 'full' }), 0));
  await tick();
  const events = drain(bridge, handle);
  assert.equal(events.length, 2);
  assert.deepEqual(events[0], { type: 'result', rid: 8, ok: false, error: { message: 'channel is full', name: 'Error', code: 'CHANNEL_FULL' } });
  assert.equal(events[1].type, 'error');
  assert.equal(events[1].method, 'joinChannel');
  assert.equal(events[1].error.code, 'CHANNEL_FULL');
});

test('chat, speech and positional arguments are mapped to the client signatures', async () => {
  const { bridge, handle, client } = make();
  ok(bridge.invoke(handle, 'sendMessage', JSON.stringify({ channelId: 'c1', text: 'hi', metadata: { k: 1 }, clientRef: 'x' }), 1));
  ok(bridge.invoke(handle, 'history', JSON.stringify({ userId: 'u2', before: 'cur', limit: 10 }), 2));
  ok(bridge.invoke(handle, 'speak', JSON.stringify({ text: 'hello', channelId: 'c1', destination: 'local' }), 3));
  ok(bridge.invoke(handle, 'updatePosition', JSON.stringify({ channelId: 'c1', position: { x: 1, y: 2, z: 3 }, orientation: { yaw: 0.5 } })));
  assert.equal(JSON.parse(bridge.invoke(handle, 'history', '{}', 4)).ok, false);
  await tick();
  assert.deepEqual(client.calls, [
    ['sendMessage', 'c1', 'hi', { metadata: { k: 1 }, clientRef: 'x' }],
    ['history', { userId: 'u2' }, { before: 'cur', limit: 10 }],
    ['speak', 'hello', { channelId: 'c1', destination: 'local' }],
    ['updatePosition', 'c1', { x: 1, y: 2, z: 3 }, { yaw: 0.5 }],
  ]);
  const results = drain(bridge, handle);
  assert.equal(results[0].value.sentAt, '1970-01-01T00:00:00.000Z');
  assert.deepEqual(results[1].value, { messages: [] });
  assert.deepEqual(results[2].value, { requestId: 'r1', clientRef: 'auto' });
});

test('client events are queued in order with named fields', () => {
  const { bridge, handle, client } = make();
  client.emit('connectionState', 'connected');
  client.emit('participantJoined', 'c1', { userId: 'u2', displayName: 'Bob' });
  client.emit('speaking', 'c1', 'u2', true);
  client.emit('channelFocusChanged', undefined);
  client.emit('failedToRecover', new Error('gave up'));
  client.emit('localEnergy', { rms: 0.5 });
  client.emit('message', { type: 'Pong' });
  assert.equal(bridge.pending(handle), 5);
  assert.deepEqual(drain(bridge, handle), [
    { type: 'connectionState', state: 'connected' },
    { type: 'participantJoined', channelId: 'c1', participant: { userId: 'u2', displayName: 'Bob' } },
    { type: 'speaking', channelId: 'c1', userId: 'u2', speaking: true },
    { type: 'channelFocusChanged', channelId: null },
    { type: 'failedToRecover', error: { message: 'gave up', name: 'Error' } },
  ]);
  assert.equal(bridge.drain(handle), '[]');
});

test('rawMessages and localEnergyEvents opt in to the chatty events', () => {
  const { bridge, handle, client } = make({ rawMessages: true, localEnergyEvents: true });
  client.emit('localEnergy', { rms: 0.5 });
  client.emit('message', { type: 'Pong' });
  assert.deepEqual(drain(bridge, handle).map((e) => e.type), ['localEnergy', 'message']);
});

test('the queue is bounded; dropped events are reported once', () => {
  const { bridge, handle, client } = make({}, { maxQueuedEvents: 16 });
  for (let i = 0; i < 20; i++) client.emit('speaking', 'c1', `u${i}`, true);
  const events = drain(bridge, handle);
  assert.deepEqual(events[0], { type: 'overflow', dropped: 4 });
  assert.equal(events.length, 17);
  assert.equal(events[1].userId, 'u4');
  client.emit('speaking', 'c1', 'u', false);
  assert.equal(drain(bridge, handle).length, 1);
});

test('token callbacks are inverted into tokenRequest events answered by provideToken', async () => {
  const { bridge, handle, client } = make({ refreshToken: true, joinToken: true });
  assert.equal(typeof client.options.refreshToken, 'function');
  ok(bridge.invoke(handle, 'joinChannel', JSON.stringify({ channelId: 'c1' }), 5));
  await tick();
  assert.deepEqual(drain(bridge, handle), [{ type: 'tokenRequest', requestId: 1, kind: 'join', channelId: 'c1' }]);
  ok(bridge.invoke(handle, 'provideToken', JSON.stringify({ requestId: 1, token: 'jt' })));
  await tick();
  const [result] = drain(bridge, handle);
  assert.equal(result.rid, 5);
  assert.equal(result.value[0].token, 'jt');

  const refresh = client.options.refreshToken();
  assert.deepEqual(drain(bridge, handle), [{ type: 'tokenRequest', requestId: 2, kind: 'refresh', channelId: null }]);
  ok(bridge.invoke(handle, 'provideToken', JSON.stringify({ requestId: 2, error: 'logged out' })));
  await assert.rejects(refresh, /logged out/);
  assert.equal(JSON.parse(bridge.invoke(handle, 'provideToken', JSON.stringify({ requestId: 2, token: 'x' }))).ok, false);
});

test('destroy disconnects a live client, rejects pending token requests and frees the handle', async () => {
  const { bridge, handle, client } = make({ refreshToken: true });
  ok(bridge.invoke(handle, 'connect', '{}', 1));
  const refresh = client.options.refreshToken();
  bridge.destroy(handle);
  assert.deepEqual(client.calls.at(-1), ['disconnect', 'client destroyed']);
  await assert.rejects(refresh, /destroyed/);
  assert.equal(bridge.size, 0);
  assert.equal(JSON.parse(bridge.invoke(handle, 'isMuted', '{}')).ok, false);
  bridge.destroy(handle);
});

test('remote audio without a document is reported, not crashed on', () => {
  const { bridge, handle, client } = make();
  client.emit('remoteStream', {});
  assert.deepEqual(drain(bridge, handle), [{ type: 'remoteAudio', playing: false, reason: 'no document' }]);
});

test('WebTransport media reports the Web Audio graph as remoteAudio (no MediaStream involved)', async () => {
  const { bridge, handle, client } = make();
  client.emit('mediaTransport', 'webtransport');
  await tick();
  assert.deepEqual(drain(bridge, handle), [
    { type: 'mediaTransport', transport: 'webtransport' },
    { type: 'remoteAudio', playing: true },
  ]);
  assert.deepEqual(client.calls.at(-1), ['resumeAudio']);

  client.audioRunning = false;
  client.emit('mediaTransport', 'webtransport');
  await tick();
  assert.deepEqual(drain(bridge, handle), [
    { type: 'mediaTransport', transport: 'webtransport' },
    { type: 'remoteAudio', playing: false, reason: 'audio context suspended (autoplay policy)' },
  ]);

  client.emit('mediaTransport', 'webrtc');
  await tick();
  assert.deepEqual(drain(bridge, handle), [{ type: 'mediaTransport', transport: 'webrtc' }], 'WebRTC playback is reported from remoteStream instead');
});

test('the browser bundle exposes the SDK as window.AurixWebSdk without a module system', () => {
  const source = readFileSync(new URL('../dist/aurix-web-sdk.js', import.meta.url), 'utf8');
  const ctx = vm.createContext({});
  vm.runInContext(source, ctx);
  const sdk = ctx.AurixWebSdk;
  assert.equal(typeof sdk.AurixBridge, 'function');
  assert.equal(typeof sdk.AurixClient, 'function');
  assert.equal(typeof sdk.transmissionToWire, 'function');
  assert.match(sdk.version, /^\d+\.\d+\.\d+/);
  const bridge = new sdk.AurixBridge({ document: null, createClient: (o) => new FakeClient(o) });
  const h = bridge.create(JSON.stringify({ apiUrl: 'http://api', wsUrl: 'ws://ws', token: 't' }));
  assert.deepEqual(JSON.parse(bridge.invoke(h, 'joinedChannels', '{}')), { ok: true, value: ['c1'] });
});
