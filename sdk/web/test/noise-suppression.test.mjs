import { test } from 'node:test';
import assert from 'node:assert/strict';
import { AurixClient } from '../dist/client.js';

class FakeSocket {
  static OPEN = 1;
  static CLOSED = 3;
  static last;
  constructor(url, protocols) {
    this.url = url;
    this.protocols = protocols;
    this.readyState = FakeSocket.OPEN;
    this.sent = [];
    FakeSocket.last = this;
    queueMicrotask(() => this.onopen?.({}));
  }
  send(data) {
    this.sent.push(JSON.parse(data));
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

const ack = (extra = {}) => ({
  type: 'SessionInitAck',
  data: { session_id: 's1', ssrc: 7, media_addr: '10.0.0.2:40000', media_key: 'AAAA', ...extra },
});

function newClient() {
  globalThis.WebSocket = FakeSocket;
  const client = new AurixClient({
    apiUrl: 'http://api',
    wsUrl: 'wss://a/ws',
    token: 't',
    autoReconnect: false,
    pingIntervalMs: 0,
  });
  client.on('error', () => {});
  return client;
}

async function connectWithAck(client, ackExtra) {
  const connecting = client.connect().catch(() => undefined);
  await new Promise((r) => setTimeout(r, 0));
  FakeSocket.last.receive(ack(ackExtra));
  const ready = new Promise((resolve) => client.on('sessionReady', resolve));
  await connecting;
  return ready;
}

test('SessionInitAck exposes the node-side noise suppression capability (false on older nodes)', async () => {
  const info = await connectWithAck(newClient(), { noise_suppression: true });
  assert.equal(info.noiseSuppression, true);
  const legacyInfo = await connectWithAck(newClient(), {});
  assert.equal(legacyInfo.noiseSuppression, false);
});

test('setServerNoiseSuppression is held client-side, replayed after the ack and confirmed by the server', async () => {
  const client = newClient();
  client.setServerNoiseSuppression(true);
  assert.equal(client.serverNoiseSuppression, false);

  await connectWithAck(client, { noise_suppression: true });
  const sock = FakeSocket.last;
  assert.deepEqual(
    sock.sent.filter((m) => m.type === 'SetNoiseSuppression'),
    [{ type: 'SetNoiseSuppression', data: { enabled: true } }],
  );

  const seen = [];
  client.on('serverNoiseSuppressionChanged', (enabled) => seen.push(enabled));
  sock.receive({ type: 'NoiseSuppressionChanged', data: { enabled: true } });
  sock.receive({ type: 'NoiseSuppressionChanged', data: { enabled: true } });
  assert.equal(client.serverNoiseSuppression, true);
  assert.deepEqual(seen, [true]);

  sock.receive({ type: 'NoiseSuppressionChanged', data: { enabled: false } });
  assert.deepEqual(seen, [true, false]);
  assert.equal(client.serverNoiseSuppression, false);
});

test('ReceiverPreferences reports whether a resumed session is still denoised', async () => {
  const client = newClient();
  await connectWithAck(client, { noise_suppression: true });
  const prefs = [];
  const seen = [];
  client.on('receiverPreferences', (p) => prefs.push(p.noiseSuppression));
  client.on('serverNoiseSuppressionChanged', (enabled) => seen.push(enabled));
  FakeSocket.last.receive({
    type: 'ReceiverPreferences',
    data: { blocked_users: [], local_mutes: [], volumes: [], noise_suppression: true },
  });
  FakeSocket.last.receive({
    type: 'ReceiverPreferences',
    data: { blocked_users: [], local_mutes: [], volumes: [] },
  });
  assert.deepEqual(prefs, [true, false]);
  assert.deepEqual(seen, [true, false]);
});
