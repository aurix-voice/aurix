import test from 'node:test';
import assert from 'node:assert/strict';
import { FakeSocket, FakePeerConnection, FakeMediaStream, FakeTrack, ack, brief, joined } from './helpers/fake-webrtc.mjs';
import { FakeAudioContext } from './helpers/fake-audio.mjs';
import {
  FakeAudioDecoder,
  FakeAudioEncoder,
  FakeEncodedAudioChunk,
  FakeNode,
  FakeWebTransport,
  installWebTransportGlobals,
  resetWebTransportFakes,
  settle,
  until,
} from './helpers/fake-webtransport.mjs';
import { AurixClient } from '../dist/client.js';
import { AurxWebTransport, certificateHashes, detectWebTransportSupport } from '../dist/webtransport.js';
import { channelIdHash } from '../dist/aurx.js';

installWebTransportGlobals();

const SESSION = '0192b7c4-5d2e-7c3a-9f11-1234567890ab';
const CHANNEL = '0192b7c4-5d2e-7c3a-9f11-abcdef012345';
const CHANNEL_2 = '0192b7c4-5d2e-7c3a-9f11-abcdef999999';
const KEY = Uint8Array.from({ length: 32 }, (_, i) => (i * 7 + 3) & 0xff);
const KEY_B64 = Buffer.from(KEY).toString('base64');
const PIN = 'ab'.repeat(32);
const WT = { urls: ['https://media.example:4443/aurx', 'https://media2.example:4443/aurx'], cert_sha256: [PIN, 'nope', '12'.repeat(31)] };

const session = (ssrc = 7) => ({ sessionId: SESSION, ssrc, masterKey: KEY });
const silentEvents = (over = {}) => ({ onAudio() {}, onBitrate() {}, onClosed() {}, onError() {}, ...over });

const liveClients = new Set();

test.beforeEach(() => {
  resetWebTransportFakes();
  FakeAudioContext.all.length = 0;
});

test.afterEach(() => {
  for (const client of liveClients) client.disconnect();
  liveClients.clear();
});

// ── Support + pins ──

test('detects WebTransport datagram + WebCrypto support', () => {
  assert.deepEqual(detectWebTransportSupport(), { ok: true, webTransport: true, datagrams: true, crypto: true });
  const saved = globalThis.WebTransport;
  delete globalThis.WebTransport;
  assert.equal(detectWebTransportSupport().ok, false);
  globalThis.WebTransport = saved;
});

test('certificate pins: only well-formed sha-256 hex reaches serverCertificateHashes', () => {
  const hashes = certificateHashes(WT);
  assert.equal(hashes.length, 1);
  assert.equal(hashes[0].algorithm, 'sha-256');
  assert.equal(Buffer.from(hashes[0].value).toString('hex'), PIN);
  assert.equal(certificateHashes({ urls: WT.urls }), undefined);
  assert.equal(certificateHashes({ urls: WT.urls, cert_sha256: [] }), undefined);
});

// ── AurxWebTransport ──

test('binds the session over the first reachable URL and passes the pins', async () => {
  const node = new FakeNode(KEY, 7);
  FakeWebTransport.node = node;
  const wt = new AurxWebTransport(session(), silentEvents());
  await wt.connect(WT);
  assert.equal(wt.isOpen, true);
  assert.equal(FakeWebTransport.all.length, 1);
  assert.equal(FakeWebTransport.all[0].url, WT.urls[0]);
  assert.equal(FakeWebTransport.all[0].options.serverCertificateHashes.length, 1);
  assert.equal(node.binds.length, 1);
  assert.equal(Buffer.from(node.binds[0].payload.subarray(0, 16)).toString('hex'), SESSION.replaceAll('-', ''));
  assert.equal(wt.stats().url, WT.urls[0]);
  wt.close();
  assert.equal(wt.isOpen, false);
  assert.notEqual(FakeWebTransport.all[0].closedWith, undefined);
});

test('falls through to the next URL when the first handshake is refused', async () => {
  FakeWebTransport.node = new FakeNode(KEY, 7);
  FakeWebTransport.behaviour = (url) => (url === WT.urls[0] ? 'refuse' : 'accept');
  const wt = new AurxWebTransport(session(), silentEvents());
  await wt.connect(WT);
  assert.equal(wt.stats().url, WT.urls[1]);
  assert.deepEqual(
    FakeWebTransport.all.map((t) => t.url),
    WT.urls,
  );
  wt.close();
});

test('gives up with every URL in the error when none binds', async () => {
  FakeWebTransport.node = new FakeNode(KEY, 7, { ackBind: false });
  FakeWebTransport.behaviour = (url) => (url === WT.urls[0] ? 'refuse' : 'accept');
  const wt = new AurxWebTransport(session(), silentEvents(), { connectTimeoutMs: 40 });
  await assert.rejects(wt.connect(WT), (e) => {
    assert.match(e.message, /WebTransport media path unavailable/);
    assert.match(e.message, /media\.example.*refused/);
    assert.match(e.message, /media2\.example.*SessionBind.*timed out/);
    return true;
  });
  assert.equal(wt.isOpen, false);
  for (const t of FakeWebTransport.all) assert.notEqual(t.closedWith, undefined, `${t.url} left open`);
  const node = FakeWebTransport.node;
  const bindsAfterFailure = node.binds.length;
  await new Promise((r) => setTimeout(r, 1200));
  assert.equal(node.binds.length, bindsAfterFailure, 'no SessionBind retries keep running after connect() failed');
});

test('rejects unusable input up front', async () => {
  await assert.rejects(new AurxWebTransport(session(), silentEvents()).connect({ urls: [] }), /no WebTransport URL/);
  const saved = globalThis.WebTransport;
  delete globalThis.WebTransport;
  try {
    await assert.rejects(new AurxWebTransport(session(), silentEvents()).connect(WT), /not available/);
  } finally {
    globalThis.WebTransport = saved;
  }
});

test('seals uplink audio the node opens: channel hash, energy byte, sequence, e2ee flag', async () => {
  const node = new FakeNode(KEY, 7);
  FakeWebTransport.node = node;
  const wt = new AurxWebTransport(session(), silentEvents(), { startSequence: 1000 });
  await wt.connect(WT);
  const hash = channelIdHash(CHANNEL);
  wt.sendAudio(Uint8Array.from([0x78, 1, 2, 3]), hash, 960, { energy: 0.1 });
  wt.sendAudio(Uint8Array.from([0x78, 4]), hash, 1920, { e2ee: true });
  await until(() => node.audio.length === 2, 5000, 'two uplink frames');
  const [a, b] = node.audio;
  assert.equal(a.channelIdHash, hash);
  assert.equal(a.timestamp, 960);
  assert.equal(a.level, 20);
  assert.deepEqual(Array.from(a.frame), [0x78, 1, 2, 3]);
  assert.equal(a.e2ee, false);
  assert.equal(b.e2ee, true);
  assert.equal(b.level, undefined);
  assert.equal(b.sequence, a.sequence + 1);
  assert.ok(a.sequence > 1000, 'sequence continues from startSequence');
  assert.equal(wt.nextSequence, b.sequence + 1);
  const s = wt.stats();
  assert.equal(s.packetsSent, node.binds.length + 2);
  assert.equal(node.rejected, 0);
  wt.close();
});

test('drops frames that cannot fit a datagram instead of fragmenting', async () => {
  const node = new FakeNode(KEY, 7);
  FakeWebTransport.node = node;
  FakeWebTransport.maxDatagramSize = 200;
  try {
    const wt = new AurxWebTransport(session(), silentEvents());
    await wt.connect(WT);
    wt.sendAudio(new Uint8Array(300), 1, 0);
    wt.sendAudio(new Uint8Array(100), 1, 0);
    await until(() => node.audio.length === 1, 5000, 'the small frame');
    assert.equal(wt.stats().packetsDroppedLocally, 1);
    assert.equal(wt.stats().maxDatagramSize, 200);
    wt.close();
  } finally {
    FakeWebTransport.maxDatagramSize = 1200;
  }
});

test('opens downlink audio, counts loss per SSRC, rejects replays and foreign keys', async () => {
  const node = new FakeNode(KEY, 7);
  FakeWebTransport.node = node;
  const got = [];
  const wt = new AurxWebTransport(session(), silentEvents({ onAudio: (a) => got.push(a) }));
  await wt.connect(WT);
  const t = FakeWebTransport.all[0];
  const p1 = await node.audioPacket(42, 1, Uint8Array.from([0x78, 9]), { gain: 0.5, direction: { azimuth: Math.PI / 2, elevation: 0 }, channelIdHash: 5 });
  t.deliver(p1);
  t.deliver(await node.audioPacket(42, 2, Uint8Array.from([0x7c, 9]), { mixed: true }));
  t.deliver(p1); // replay
  t.deliver(await node.audioPacket(42, 5, Uint8Array.from([0x78, 1]))); // 3,4 lost
  t.deliver(await node.audioPacket(43, 1, Uint8Array.from([0x78, 1]), { e2ee: true }));
  const other = new FakeNode(Uint8Array.from(KEY, (b) => b ^ 0xff), 7);
  t.deliver(await other.audioPacket(44, 1, Uint8Array.from([0x78, 1])));
  t.deliver(Uint8Array.from([1, 2, 3])); // garbage
  await until(() => got.length === 4 && wt.stats().packetsRejected === 3, 5000, 'four accepted, three rejected');
  assert.equal(got[0].ssrc, 42);
  assert.equal(got[0].gain, 0.5);
  assert.ok(Math.abs(got[0].direction.azimuth - Math.PI / 2) < 0.02);
  assert.equal(got[0].channelIdHash, 5);
  assert.deepEqual(Array.from(got[0].frame), [0x78, 9]);
  assert.equal(got[1].mixed, true);
  assert.equal(got[1].gain, 1);
  assert.equal(got[3].e2ee, true);
  const s = wt.stats();
  assert.equal(s.audioPacketsReceived, 4);
  assert.equal(s.audioPacketsLost, 2);
  assert.equal(s.packetsRejected, 3);
  wt.forgetSsrc(42);
  assert.equal(wt.stats().audioPacketsLost, 2, 'forgotten SSRC loss is kept in the totals');
  wt.close();
});

test('heartbeats measure RTT and their loss closes the path', async () => {
  const node = new FakeNode(KEY, 7);
  FakeWebTransport.node = node;
  let closed;
  const wt = new AurxWebTransport(session(), silentEvents({ onClosed: (r) => (closed = r) }), { heartbeatIntervalMs: 10, heartbeatLossLimit: 3 });
  await wt.connect(WT);
  await until(() => node.heartbeats.length >= 2 && wt.stats().rttMs !== undefined, 5000, 'heartbeat RTT');
  assert.equal(closed, undefined);
  FakeWebTransport.node = undefined; // the node stops answering
  await until(() => closed !== undefined, 5000, 'heartbeat loss');
  assert.match(closed, /no heartbeat answer/);
  assert.equal(wt.isOpen, false);
  assert.equal(wt.stats().heartbeatsMissed, 3);
});

test('heartbeatIntervalMs 0 sends no heartbeats and never declares the path dead', async () => {
  const node = new FakeNode(KEY, 7);
  FakeWebTransport.node = node;
  let closed;
  const wt = new AurxWebTransport(session(), silentEvents({ onClosed: (r) => (closed = r) }), { heartbeatIntervalMs: 0, heartbeatLossLimit: 1 });
  await wt.connect(WT);
  await new Promise((r) => setTimeout(r, 30));
  assert.equal(node.heartbeats.length, 0);
  assert.equal(wt.stats().rttMs, undefined);
  assert.equal(closed, undefined);
  assert.ok(wt.isOpen);
  wt.close();
});

test('server SessionClose and a dropped connection report onClosed once', async () => {
  const node = new FakeNode(KEY, 7);
  FakeWebTransport.node = node;
  const closes = [];
  let wt = new AurxWebTransport(session(), silentEvents({ onClosed: (r) => closes.push(r) }));
  await wt.connect(WT);
  FakeWebTransport.all[0].deliver(await node.sessionClose());
  await until(() => closes.length === 1, 5000, 'SessionClose');
  assert.match(closes[0], /closed by the server/);
  wt.close();
  await settle();
  assert.equal(closes.length, 1);

  wt = new AurxWebTransport(session(), silentEvents({ onClosed: (r) => closes.push(r) }));
  await wt.connect(WT);
  FakeWebTransport.all[1].drop('network gone');
  await until(() => closes.length === 2, 5000, 'drop');
  assert.match(closes[1], /network gone/);
});

test('BitrateCommand reaches onBitrate', async () => {
  const node = new FakeNode(KEY, 7);
  FakeWebTransport.node = node;
  let bps;
  const wt = new AurxWebTransport(session(), silentEvents({ onBitrate: (b) => (bps = b) }));
  await wt.connect(WT);
  FakeWebTransport.all[0].deliver(await node.bitrateCommand(24_000));
  await until(() => bps !== undefined, 5000, 'bitrate');
  assert.equal(bps, 24_000);
  wt.close();
});

// ── AurixClient over WebTransport ──

const wtAck = (extra = {}) => ack({ session_id: SESSION, media_key: KEY_B64, webtransport: WT, ...extra });

async function connectClient(opts = {}, ackExtra = {}) {
  const ctx = new FakeAudioContext();
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
    audioContext: ctx,
    webTransport: { connectTimeoutMs: 500, heartbeatIntervalMs: 0 },
    ...opts,
  });
  const errors = [];
  const transports = [];
  client.on('error', (e) => errors.push(e));
  client.on('mediaTransport', (t) => transports.push(t));
  liveClients.add(client);
  FakeSocket.last = undefined;
  const connecting = client.connect();
  await until(() => FakeSocket.last !== undefined, 5000, 'control socket');
  const sock = FakeSocket.last;
  sock.onsent = (msg) => {
    if (msg.type === 'WebRtcOffer') queueMicrotask(() => sock.receive({ type: 'WebRtcAnswer', data: { sdp: 'v=0\r\nanswer' } }));
  };
  sock.receive(wtAck(ackExtra));
  await connecting;
  return { client, sock, ctx, errors, transports };
}

const captureNode = () => {
  let latest;
  for (const ctx of FakeAudioContext.all) {
    for (const n of ctx.created) if (n.kind === 'worklet:aurix-aurx-capture') latest = n;
  }
  return latest;
};

const renderFrame = (level) => {
  const node = captureNode();
  assert.ok(node, 'capture worklet is running');
  node.render([new Float32Array(960).fill(level)]);
};

test('client: picks WebTransport when advertised and supported, no WebRTC negotiation', async () => {
  const node = new FakeNode(KEY, 7);
  FakeWebTransport.node = node;
  const pcsBefore = FakePeerConnection.all.length;
  const { client, sock, errors, transports } = await connectClient();
  await until(() => client.connectionState === 'media-connected', 5000, 'media-connected');
  assert.equal(client.mediaTransport, 'webtransport');
  assert.deepEqual(transports, ['webtransport']);
  assert.equal(FakePeerConnection.all.length, pcsBefore, 'no RTCPeerConnection');
  assert.equal(node.binds.length, 1);
  assert.deepEqual(errors, []);
  assert.equal(sock.sent.some((m) => m.type === 'WebRtcOffer'), false);
  const queues = FakeWebTransport.all[0].datagrams;
  assert.ok(queues.outgoingHighWaterMark > 1, 'browser may buffer several outgoing datagrams');
  assert.ok(queues.incomingHighWaterMark > 1, 'browser buffers incoming datagrams while the page is busy');
  assert.ok(queues.outgoingMaxAge > 0 && queues.incomingMaxAge > 0, 'stale datagrams expire in the browser');
  const info = client.sessionInfo;
  assert.deepEqual(info.webTransport.urls, WT.urls);
  assert.notEqual(info.webTransport.urls, WT.urls, 'advertisement is copied');
  client.disconnect();
  await settle();
  assert.notEqual(FakeWebTransport.all[0].closedWith, undefined, 'WebTransport closed on disconnect');
  assert.equal(client.mediaTransport, undefined);
});

test('client: quality reports and lastStats work without an RTCPeerConnection', async () => {
  FakeWebTransport.node = new FakeNode(KEY, 7);
  const { client, sock, errors } = await connectClient();
  await until(() => client.connectionState === 'media-connected', 5000, 'media-connected');
  assert.equal(client.lastStats, undefined);
  await client.reportQuality();
  const report = sock.sent.find((m) => m.type === 'QualityReport');
  assert.ok(report, 'QualityReport sent over the control socket');
  assert.equal(typeof report.data.rtt_ms, 'number');
  assert.equal(typeof client.lastStats?.rttMs, 'number');
  assert.deepEqual(errors, []);
  client.disconnect();
  await settle();
});

test('client: uplink goes to joined speaker channels only, mute pauses it, transmission mode routes it', async () => {
  const node = new FakeNode(KEY, 7);
  FakeWebTransport.node = node;
  const { client, sock } = await connectClient();
  await until(() => client.connectionState === 'media-connected', 5000, 'media-connected');

  renderFrame(0.5);
  await settle();
  assert.equal(node.audio.length, 0, 'nothing is sent before joining a channel');

  await joined(client, sock, CHANNEL, [brief('me', 7), brief('alice', 8)]);
  await joined(client, sock, CHANNEL_2, [brief('me', 7)], { role: 'listener' });
  renderFrame(0.5);
  await until(() => node.audio.length === 1, 5000, 'one frame on the speaker channel');
  assert.equal(node.audio[0].channelIdHash, channelIdHash(CHANNEL));
  assert.ok(node.audio[0].level < 127, 'energy byte carries the RMS');
  assert.equal(node.audio[0].frame[0] & 0x04, 0, 'mono TOC');

  client.setMuted(true);
  renderFrame(0.5);
  await settle();
  assert.equal(node.audio.length, 1, 'muted: no frames on the wire');
  client.setMuted(false);

  await joined(client, sock, CHANNEL_2.replace('999999', '777777'), [brief('me', 7)]);
  client.setTransmission({ type: 'single', channelId: CHANNEL });
  renderFrame(0.5);
  await until(() => node.audio.length === 2, 5000, 'single-channel frame');
  assert.equal(node.audio[1].channelIdHash, channelIdHash(CHANNEL));

  client.setTransmission({ type: 'all' });
  renderFrame(0.5);
  await until(() => node.audio.length === 4, 5000, 'frames for both speaker channels');
  assert.deepEqual(
    node.audio
      .slice(2)
      .map((a) => a.channelIdHash)
      .sort(),
    [channelIdHash(CHANNEL), channelIdHash(CHANNEL_2.replace('999999', '777777'))].sort(),
  );
  assert.equal(node.audio[2].sequence + 1, node.audio[3].sequence);

  client.setTransmission({ type: 'none' });
  renderFrame(0.5);
  await settle();
  assert.equal(node.audio.length, 4);
  client.disconnect();
});

test('client: capture energy follows the encoder when it re-stamps outputs, DTX empties never hit the wire', async () => {
  const node = new FakeNode(KEY, 7);
  FakeWebTransport.node = node;
  const { client, sock, errors } = await connectClient();
  await until(() => client.connectionState === 'media-connected', 5000, 'media-connected');
  await joined(client, sock, CHANNEL, [brief('me', 7)]);
  const enc = FakeAudioEncoder.all.at(-1);
  const output = enc.output;
  let stamped = 0;
  // Chromium: output timestamps count the encoder's own frames, a silent frame under DTX is a 0-byte chunk.
  enc.output = (chunk) => {
    const data = chunk.data[1] === 0 ? new Uint8Array(0) : chunk.data;
    output(new FakeEncodedAudioChunk({ type: 'key', timestamp: 600_000 + 20_000 * stamped++, data }));
  };
  renderFrame(0.5);
  renderFrame(0);
  renderFrame(0.25);
  await until(() => node.audio.length === 2, 5000, 'two audible frames');
  await settle();
  assert.equal(node.audio.length, 2, 'the DTX frame is not sent');
  assert.ok(node.audio[0].level < node.audio[1].level && node.audio[1].level < 127, `levels track the input: ${node.audio.map((a) => a.level)}`);
  assert.deepEqual(errors, []);
  client.disconnect();
});

test('client: an empty downlink frame is accepted and skipped, not fed to the decoder', async () => {
  const node = new FakeNode(KEY, 7);
  FakeWebTransport.node = node;
  const { client, sock, errors } = await connectClient();
  await until(() => client.connectionState === 'media-connected', 5000, 'media-connected');
  await joined(client, sock, CHANNEL, [brief('me', 7), brief('alice', 8)]);
  const t = FakeWebTransport.all[0];
  const decodersBefore = FakeAudioDecoder.all.length;
  t.deliver(await node.audioPacket(8, 1, new Uint8Array(0)));
  t.deliver(await node.audioPacket(8, 2, Uint8Array.from([0x78, 200])));
  await until(() => FakeAudioDecoder.all.length === decodersBefore + 1 && FakeAudioDecoder.all.at(-1).decoded.length === 1, 5000, 'one decoded frame');
  await settle();
  const stats = await client.getStats();
  assert.equal(stats.packetsReceived, 2);
  assert.equal(stats.packetsDiscarded, 0);
  assert.deepEqual(errors, []);
  client.disconnect();
});
test('client: downlink frames are decoded per SSRC, placed by server gain/direction and dropped when the speaker leaves', async () => {
  const node = new FakeNode(KEY, 7);
  FakeWebTransport.node = node;
  const { client, sock, ctx } = await connectClient();
  await until(() => client.connectionState === 'media-connected', 5000, 'media-connected');
  await joined(client, sock, CHANNEL, [brief('me', 7), brief('alice', 8), brief('bob', 9)]);
  const layouts = [];
  client.on('participantStreams', (s) => layouts.push(s));
  assert.deepEqual(client.getParticipantStreams(), [], 'no slots before the first frame');
  const t = FakeWebTransport.all[0];
  const decodersBefore = FakeAudioDecoder.all.length;
  t.deliver(await node.audioPacket(8, 1, Uint8Array.from([0x78, 200]), { gain: 0.5, direction: { azimuth: -Math.PI / 2, elevation: 0 } }));
  t.deliver(await node.audioPacket(9, 1, Uint8Array.from([0x7c, 100])));
  t.deliver(await node.audioPacket(8, 2, Uint8Array.from([0x78, 200]), { gain: 0.5, direction: { azimuth: -Math.PI / 2, elevation: 0 } }));
  await until(
    () => FakeAudioDecoder.all.length === decodersBefore + 2 && FakeAudioDecoder.all.at(-1).decoded.length + FakeAudioDecoder.all.at(-2).decoded.length === 3,
    5000,
    'two decoders fed three frames',
  );
  const [alice, bob] = FakeAudioDecoder.all.slice(-2).sort((a, b) => a.config.numberOfChannels - b.config.numberOfChannels);
  assert.equal(alice.config.numberOfChannels, 1);
  assert.equal(bob.config.numberOfChannels, 2, 'stereo TOC opens a stereo decoder');
  assert.equal(alice.decoded.length, 2);
  const players = ctx.created.filter((n) => n.kind === 'worklet:aurix-aurx-player');
  assert.equal(players.length, 2, 'one player node per SSRC on the renderer context');
  await settle();
  assert.deepEqual(
    client.getParticipantStreams().sort((a, b) => a.mid.localeCompare(b.mid)),
    [
      { mid: 'wt:8', userId: 'alice', stream: undefined, live: true },
      { mid: 'wt:9', userId: 'bob', stream: undefined, live: true },
    ],
    'every rendered SSRC is a per-participant slot',
  );
  assert.equal(layouts.length, 2, 'one participantStreams event per new speaker');
  assert.equal(client.isParticipantSpatialized('alice'), true, 'directional frames pan through the renderer');
  const stats = await client.getStats();
  assert.equal(stats.transport, 'webtransport');
  assert.equal(stats.packetsReceived, 3);

  sock.receive({ type: 'ParticipantLeft', data: { channel_id: CHANNEL, user_id: 'alice' } });
  await settle();
  assert.equal(alice.state, 'closed', 'departed speaker: decoder closed');
  assert.equal(bob.state, 'configured');
  assert.deepEqual(client.getParticipantStreams().map((s) => s.userId), ['bob'], 'departed speaker leaves the layout');
  assert.equal(layouts.length, 3);
  t.deliver(await node.audioPacket(9, 2, Uint8Array.from([0x7c, 100])));
  await until(() => bob.decoded.length === 2, 5000, 'bob still plays');
  client.disconnect();
  await settle();
  assert.equal(bob.state, 'closed', 'disconnect closes every decoder');
  assert.deepEqual(client.getParticipantStreams(), []);
  assert.equal(layouts.at(-1).length, 0, 'teardown empties the layout');
});

test('client: BitrateCommand and audio policy reconfigure the WebCodecs encoder', async () => {
  const node = new FakeNode(KEY, 7);
  FakeWebTransport.node = node;
  const { client, sock } = await connectClient({
    opus: { stereo: true },
    webTransport: { connectTimeoutMs: 500, heartbeatIntervalMs: 0, opus: { expectedLossPct: 25 } },
  });
  await until(() => client.connectionState === 'media-connected', 5000, 'media-connected');
  const enc = FakeAudioEncoder.all.at(-1);
  assert.equal(enc.config.opus.packetlossperc, 25, 'webTransport.opus overrides apply');
  assert.equal(enc.config.numberOfChannels, 1, 'stereo capture waits for a channel policy that allows it');
  FakeWebTransport.all[0].deliver(await node.bitrateCommand(20_000));
  await until(() => enc.config.bitrate === 20_000, 5000, 'bitrate applied');
  await joined(client, sock, CHANNEL, [brief('me', 7)], { audio: { stereo: true, bitrate_bps: 96_000, fec: false, dtx: false } });
  await until(() => FakeAudioEncoder.all.at(-1) !== enc && FakeAudioEncoder.all.at(-1).config.numberOfChannels === 2, 5000, 'stereo restarts the encoder');
  assert.equal(enc.state, 'closed');
  const stereo = FakeAudioEncoder.all.at(-1);
  assert.equal(stereo.config.opus.useinbandfec, false);
  assert.equal(stereo.config.opus.packetlossperc, 25, 'explicit overrides survive policy changes');
  client.disconnect();
});

test('client: auto policy falls back to WebRTC when the WebTransport path fails', async () => {
  FakeWebTransport.node = new FakeNode(KEY, 7, { ackBind: false });
  const { client, sock, errors, transports } = await connectClient();
  await until(() => FakePeerConnection.all.length > 0 && sock.sent.some((m) => m.type === 'WebRtcOffer'), 5000, 'WebRTC offer');
  FakePeerConnection.all.at(-1).connect();
  await until(() => client.connectionState === 'media-connected', 5000, 'media-connected over WebRTC');
  assert.equal(client.mediaTransport, 'webrtc');
  assert.deepEqual(transports, ['webrtc']);
  assert.ok(errors.some((e) => /falling back to WebRTC/.test(e.message)), 'fallback is reported');
  assert.equal((await client.getStats()).transport, 'webrtc');
  client.disconnect();
});

test('client: WebRTC stats add up the per-participant inbound-rtp tracks (jitter = worst active track)', async () => {
  FakeWebTransport.node = new FakeNode(KEY, 7, { ackBind: false });
  const { client, sock } = await connectClient();
  await until(() => FakePeerConnection.all.length > 0 && sock.sent.some((m) => m.type === 'WebRtcOffer'), 5000, 'WebRTC offer');
  const pc = FakePeerConnection.all.at(-1);
  pc.connect();
  await until(() => client.connectionState === 'media-connected', 5000, 'media-connected over WebRTC');
  pc.getStats = async () =>
    new Map([
      ['in-mixed', { type: 'inbound-rtp', kind: 'audio', packetsReceived: 0, packetsLost: 0, bytesReceived: 0, jitter: 0.9 }],
      ['in-a', { type: 'inbound-rtp', kind: 'audio', packetsReceived: 100, packetsLost: 2, bytesReceived: 8000, jitter: 0.004, concealedSamples: 960, jitterBufferDelay: 2, jitterBufferEmittedCount: 100 }],
      ['in-b', { type: 'inbound-rtp', kind: 'audio', packetsReceived: 50, packetsLost: 1, bytesReceived: 4000, jitter: 0.01, packetsDiscarded: 3, jitterBufferDelay: 3, jitterBufferEmittedCount: 100 }],
      ['in-video', { type: 'inbound-rtp', kind: 'video', packetsReceived: 999 }],
      ['out', { type: 'outbound-rtp', kind: 'audio', packetsSent: 70, bytesSent: 5600 }],
    ]);
  const s = await client.getStats();
  assert.equal(s.transport, 'webrtc');
  assert.equal(s.packetsReceived, 150);
  assert.equal(s.packetsLost, 3);
  assert.equal(s.bytesReceived, 12000);
  assert.equal(s.concealedSamples, 960);
  assert.equal(s.packetsDiscarded, 3);
  assert.equal(s.jitterMs, 10);
  assert.equal(s.jitterBufferDelayMs, 25);
  assert.equal(s.packetsSent, 70);
  client.disconnect();
});

test('client: transport "webrtc" never touches WebTransport; "webtransport" never falls back', async () => {
  FakeWebTransport.node = new FakeNode(KEY, 7);
  const a = await connectClient({ transport: 'webrtc' });
  await until(() => a.sock.sent.some((m) => m.type === 'WebRtcOffer'), 5000, 'offer');
  assert.equal(FakeWebTransport.all.length, 0);
  a.client.disconnect();

  FakeWebTransport.behaviour = () => 'refuse';
  const b = await assert.rejects(connectClient({ transport: 'webtransport' }), /WebTransport media path unavailable/).then(() => FakeSocket.last);
  await settle();
  assert.equal(b.sent.some((m) => m.type === 'WebRtcOffer'), false, 'no WebRTC fallback in strict mode');
  assert.equal(FakeWebTransport.all.length, 2, 'both advertised URLs were tried');

  FakeWebTransport.behaviour = () => 'accept';
  FakeWebTransport.all.length = 0;
  await assert.rejects(connectClient({ transport: 'webtransport' }, { webtransport: undefined }), /WebTransport unavailable/);
  const c = FakeSocket.last;
  assert.equal(c.sent.some((m) => m.type === 'WebRtcOffer'), false);
  assert.equal(FakeWebTransport.all.length, 0);
});

test('client: a lost WebTransport path is re-established and the sequence keeps climbing', async () => {
  const node = new FakeNode(KEY, 7);
  FakeWebTransport.node = node;
  const { client, sock, errors } = await connectClient();
  await until(() => client.connectionState === 'media-connected', 5000, 'media-connected');
  await joined(client, sock, CHANNEL, [brief('me', 7)]);
  renderFrame(0.5);
  await until(() => node.audio.length === 1, 5000, 'first frame');
  const first = FakeWebTransport.all[0];
  first.drop('idle timeout');
  await until(() => FakeWebTransport.all.length === 2 && node.bound === FakeWebTransport.all[1], 5000, 'rebind on a fresh WebTransport');
  await until(() => client.connectionState === 'media-connected' && client.mediaTransport === 'webtransport', 5000, 'media back');
  assert.ok(errors.some((e) => /WebTransport media closed: .*idle timeout/.test(e.message)));
  renderFrame(0.5);
  await until(() => node.audio.length === 2, 5000, 'second frame');
  assert.ok(node.audio[1].sequence > node.audio[0].sequence, 'sequence numbers never restart mid-session');
  client.disconnect();
});
