import test from 'node:test';
import assert from 'node:assert/strict';
import { FakeSocket, FakePeerConnection, FakeMediaStream, FakeTrack, ack, brief, joined } from './helpers/fake-webrtc.mjs';
import { FakeAudioContext } from './helpers/fake-audio.mjs';
import { FakeNode, FakeWebTransport, installWebTransportGlobals, resetWebTransportFakes, settle, until } from './helpers/fake-webtransport.mjs';
import { AurixClient } from '../dist/client.js';
import { AurxWebSocketTunnel } from '../dist/ws-tunnel.js';
import { AurxPacketType } from '../dist/aurx.js';

installWebTransportGlobals();

const SESSION = '0192b7c4-5d2e-7c3a-9f11-1234567890ab';
const CHANNEL = '0192b7c4-5d2e-7c3a-9f11-abcdef012345';
const KEY = Uint8Array.from({ length: 32 }, (_, i) => (i * 7 + 3) & 0xff);
const KEY_B64 = Buffer.from(KEY).toString('base64');
const WT = { urls: ['https://media.example:4443/aurx'], cert_sha256: ['ab'.repeat(32)] };

const session = (ssrc = 7) => ({ sessionId: SESSION, ssrc, masterKey: KEY });
const silentEvents = (over = {}) => ({ onAudio() {}, onBitrate() {}, onClosed() {}, onError() {}, ...over });

/** Wires a `FakeSocket` to a `FakeNode`: binary frames up go to the node, its answers come back as binary frames. */
function attachNode(sock, node) {
  const carrier = { deliver: (bytes) => sock.receiveBinary(bytes) };
  sock.onbinary = (bytes) => void node.onDatagram(carrier, bytes);
  return carrier;
}

const liveClients = new Set();

test.beforeEach(() => {
  resetWebTransportFakes();
  FakeAudioContext.all.length = 0;
  FakePeerConnection.all.length = 0;
});

test.afterEach(() => {
  for (const client of liveClients) client.disconnect();
  liveClients.clear();
});

// ── AurxWebSocketTunnel ──

test('binds the session over the open socket and carries audio both ways', async () => {
  const node = new FakeNode(KEY, 7);
  const sock = new FakeSocket('wss://a/ws', []);
  const carrier = attachNode(sock, node);
  const audio = [];
  const tunnel = new AurxWebSocketTunnel(session(), silentEvents({ onAudio: (a) => audio.push(a) }), sock, { heartbeatIntervalMs: 0 });
  sock.onmessage = (ev) => tunnel.onFrame(ev.data);
  await tunnel.connect();
  assert.equal(tunnel.isOpen, true);
  assert.equal(tunnel.connectedUrl, 'wss://a/ws');
  assert.equal(node.binds.length, 1);
  assert.equal(node.bound, carrier);

  tunnel.sendAudio(Uint8Array.of(1, 2, 3), 0x1234, 960, { energy: 0.5 });
  await until(() => node.audio.length === 1, 2000, 'uplink frame');
  assert.equal(node.audio[0].channelIdHash, 0x1234);
  assert.deepEqual([...node.audio[0].frame], [1, 2, 3]);

  carrier.deliver(await node.seal({ packetType: AurxPacketType.Audio, flags: 0, sequence: node.nextSequence(), timestamp: 960, ssrc: 99, channelIdHash: 0x1234 }, Uint8Array.of(9, 9)));
  await until(() => audio.length === 1, 2000, 'downlink frame');
  assert.equal(audio[0].ssrc, 99);
  const s = tunnel.stats();
  assert.equal(s.packetsSent, 2, 'bind + audio');
  assert.equal(s.audioPacketsReceived, 1);
  assert.equal(s.url, 'wss://a/ws');
  tunnel.close();
  assert.equal(tunnel.isOpen, false);
  assert.equal(sock.readyState, FakeSocket.OPEN, 'the control socket is not ours to close');
});

test('refuses a closed socket, times out without a bind ack, and a dying socket reports onClosed once', async () => {
  const closedSock = new FakeSocket('wss://a/ws', []);
  closedSock.close();
  await assert.rejects(new AurxWebSocketTunnel(session(), silentEvents(), closedSock).connect(), /not open/);

  const mute = new FakeSocket('wss://a/ws', []);
  attachNode(mute, new FakeNode(KEY, 7, { ackBind: false }));
  await assert.rejects(new AurxWebSocketTunnel(session(), silentEvents(), mute, { connectTimeoutMs: 100 }).connect(), /WebSocket media tunnel unavailable: .*timed out/);

  const sock = new FakeSocket('wss://a/ws', []);
  attachNode(sock, new FakeNode(KEY, 7));
  const closed = [];
  const tunnel = new AurxWebSocketTunnel(session(), silentEvents({ onClosed: (r) => closed.push(r) }), sock, { heartbeatIntervalMs: 0 });
  sock.onmessage = (ev) => tunnel.onFrame(ev.data);
  await tunnel.connect();
  tunnel.socketClosed('websocket closed (1006)');
  tunnel.socketClosed('again');
  assert.deepEqual(closed, ['websocket closed (1006)']);
  assert.equal(tunnel.isOpen, false);
});

test('drops frames instead of queueing behind a stalled socket', async () => {
  const node = new FakeNode(KEY, 7);
  const sock = new FakeSocket('wss://a/ws', []);
  attachNode(sock, node);
  const tunnel = new AurxWebSocketTunnel(session(), silentEvents(), sock, { heartbeatIntervalMs: 0 });
  sock.onmessage = (ev) => tunnel.onFrame(ev.data);
  await tunnel.connect();
  sock.bufferedAmount = 1 << 20;
  tunnel.sendAudio(Uint8Array.of(1), 1, 960);
  await settle();
  assert.equal(node.audio.length, 0);
  assert.equal(tunnel.stats().packetsDroppedLocally, 1);
  sock.bufferedAmount = 0;
  tunnel.sendAudio(Uint8Array.of(1), 1, 1920);
  await until(() => node.audio.length === 1, 2000, 'frame after the backlog cleared');
  tunnel.close();
});

// ── AurixClient ──

async function connectClient(opts = {}, ackExtra = {}, node = new FakeNode(KEY, 7)) {
  const ctx = new FakeAudioContext();
  const client = new AurixClient({
    apiUrl: 'http://api',
    wsUrl: 'wss://a/ws',
    token: 't',
    autoReconnect: true,
    pingIntervalMs: 0,
    qualityReportIntervalMs: 0,
    useTurn: false,
    localVoiceActivity: false,
    localStream: new FakeMediaStream([new FakeTrack()]),
    audioContext: ctx,
    webTransport: { connectTimeoutMs: 500, heartbeatIntervalMs: 0 },
    webRtcConnectTimeoutMs: 200,
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
  attachNode(sock, node);
  sock.onsent = (msg) => {
    if (msg.type === 'WebRtcOffer') queueMicrotask(() => sock.receive({ type: 'WebRtcAnswer', data: { sdp: 'v=0\r\nanswer' } }));
  };
  sock.receive(ack({ session_id: SESSION, media_key: KEY_B64, media_tunnel: true, ...ackExtra }));
  await connecting;
  return { client, sock, ctx, errors, transports, node };
}

const captureNode = () => {
  let latest;
  for (const ctx of FakeAudioContext.all) {
    for (const n of ctx.created) if (n.kind === 'worklet:aurix-aurx-capture') latest = n;
  }
  return latest;
};

test('client: transport "websocket" binds over the control socket, no WebRTC, no WebTransport', async () => {
  FakeWebTransport.node = new FakeNode(KEY, 7);
  const { client, sock, node, errors, transports } = await connectClient({ transport: 'websocket' }, { webtransport: WT });
  await until(() => client.connectionState === 'media-connected', 5000, 'media-connected');
  assert.equal(client.mediaTransport, 'websocket');
  assert.deepEqual(transports, ['websocket']);
  assert.equal(FakePeerConnection.all.length, 0);
  assert.equal(FakeWebTransport.all.length, 0);
  assert.equal(node.binds.length, 1);
  assert.equal(sock.sent.some((m) => m.type === 'WebRtcOffer'), false);
  assert.deepEqual(errors, []);
  await joined(client, sock, CHANNEL, [brief('me', 7)]);
  captureNode().render([new Float32Array(960).fill(0.5)]);
  await until(() => node.audio.length === 1, 5000, 'uplink frame via the socket');
  assert.equal((await client.getStats()).transport, 'websocket');
  client.disconnect();
  await settle();
  assert.equal(client.mediaTransport, undefined);
});

test('client: "websocket" fails without the node offer; "auto" prefers WebRTC while it connects', async () => {
  await assert.rejects(connectClient({ transport: 'websocket' }, { media_tunnel: false }), /does not offer the WebSocket media tunnel/);

  const { client, sock, node } = await connectClient({}, {});
  await until(() => sock.sent.some((m) => m.type === 'WebRtcOffer'), 5000, 'offer');
  FakePeerConnection.all.at(-1).connect();
  await until(() => client.connectionState === 'media-connected', 5000, 'media-connected over WebRTC');
  assert.equal(client.mediaTransport, 'webrtc');
  assert.equal(node.binds.length, 0, 'no tunnel bind while WebRTC works');
  client.disconnect();
});

test('client: "auto" moves to the WebSocket tunnel when WebRTC never connects, and after an ICE failure', async () => {
  const { client, sock, node, errors, transports } = await connectClient();
  await until(() => sock.sent.some((m) => m.type === 'WebRtcOffer'), 5000, 'offer');
  const pc = FakePeerConnection.all.at(-1);
  await until(() => client.connectionState === 'media-connected' && client.mediaTransport === 'websocket', 5000, 'tunnel after the connect timeout');
  assert.equal(pc.closed, true, 'the stalled peer connection is released');
  assert.equal(node.binds.length, 1);
  assert.deepEqual(transports, ['websocket']);
  assert.ok(errors.some((e) => /did not connect within 200 ms/.test(e.message)));
  assert.equal(sock.sent.filter((m) => m.type === 'WebRtcOffer').length, 1, 'WebRTC is not retried once it failed here');
  client.disconnect();

  const b = await connectClient();
  await until(() => b.sock.sent.some((m) => m.type === 'WebRtcOffer'), 5000, 'offer');
  const pc2 = FakePeerConnection.all.at(-1);
  pc2.connect();
  await until(() => b.client.mediaTransport === 'webrtc', 5000, 'WebRTC up');
  pc2.connectionState = 'failed';
  pc2.onconnectionstatechange();
  await until(() => b.client.connectionState === 'media-connected' && b.client.mediaTransport === 'websocket', 5000, 'tunnel after ICE failure');
  assert.deepEqual(b.transports, ['webrtc', 'websocket']);
  b.client.disconnect();
});

test('client: WebTransport is still first in "auto"; the tunnel only follows a WebRTC failure', async () => {
  FakeWebTransport.node = new FakeNode(KEY, 7);
  const { client, node, transports } = await connectClient({}, { webtransport: WT });
  await until(() => client.connectionState === 'media-connected', 5000, 'media-connected');
  assert.equal(client.mediaTransport, 'webtransport');
  assert.deepEqual(transports, ['webtransport']);
  assert.equal(node.binds.length, 0);
  client.disconnect();
});

test('client: the tunnel is rebound on the new socket after a reconnect', async () => {
  const node = new FakeNode(KEY, 7);
  const { client, sock, errors } = await connectClient({ transport: 'websocket', reconnect: { initialDelayMs: 1, maxDelayMs: 1, jitter: 0 } }, {}, node);
  await until(() => client.connectionState === 'media-connected', 5000, 'media-connected');
  await joined(client, sock, CHANNEL, [brief('me', 7)]);
  captureNode().render([new Float32Array(960).fill(0.5)]);
  await until(() => node.audio.length === 1, 5000, 'first frame');

  sock.close(1006, 'gone');
  await until(() => FakeSocket.last !== sock, 5000, 'new socket');
  const sock2 = FakeSocket.last;
  attachNode(sock2, node);
  sock2.receive(ack({ session_id: SESSION, media_key: KEY_B64, media_tunnel: true, resumed: true }));
  await until(() => node.binds.length === 2, 5000, 'rebind over the new socket');
  await until(() => client.connectionState === 'media-connected' && client.mediaTransport === 'websocket', 5000, 'media back');
  captureNode().render([new Float32Array(960).fill(0.5)]);
  await until(() => node.audio.length === 2, 5000, 'second frame');
  assert.ok(node.audio[1].sequence > node.audio[0].sequence, 'sequence numbers never restart mid-session');
  assert.ok(!errors.some((e) => /media closed/.test(e.message)), 'the socket loss is a reconnect, not a media-path failure');
  client.disconnect();
});
