import { test } from 'node:test';
import assert from 'node:assert/strict';
import { reconnectEndpoint } from '../dist/client.js';
import { parseServerMessage } from '../dist/protocol.js';

test('reconnects try the session node first, then rotate through the advertised failover nodes', () => {
  const failover = ['wss://b/ws', 'wss://c/ws'];
  const order = [1, 2, 3, 4, 5, 6, 7].map((a) => reconnectEndpoint('wss://a/ws', failover, a));
  assert.deepEqual(order, ['wss://a/ws', 'wss://b/ws', 'wss://c/ws', 'wss://a/ws', 'wss://b/ws', 'wss://c/ws', 'wss://a/ws']);
  assert.equal(reconnectEndpoint('wss://a/ws', failover, 0), 'wss://a/ws');
  assert.ok([1, 2, 3, 4].every((a) => reconnectEndpoint('wss://a/ws', [], a) === 'wss://a/ws'));
});

test('SessionInitAck carries migrated and failover; older servers default to neither', () => {
  const migrated = parseServerMessage(
    JSON.stringify({
      type: 'SessionInitAck',
      data: {
        session_id: 's1',
        ssrc: 7,
        media_addr: '10.0.0.2:40000',
        media_key: 'AAAA',
        resumed: true,
        migrated: true,
        failover: ['wss://a/ws', 'wss://c/ws'],
      },
    }),
  );
  assert.equal(migrated.type, 'SessionInitAck');
  assert.equal(migrated.data.migrated, true);
  assert.deepEqual(migrated.data.failover, ['wss://a/ws', 'wss://c/ws']);

  const legacy = parseServerMessage(
    JSON.stringify({
      type: 'SessionInitAck',
      data: { session_id: 's1', ssrc: 7, media_addr: '10.0.0.2:40000', media_key: 'AAAA' },
    }),
  );
  assert.equal(legacy.data.migrated, undefined);
  assert.equal(legacy.data.failover, undefined);
});

test('a rejected handshake fails connect() and settles connectionState to failed (no stuck "connecting")', async () => {
  class RefusingSocket {
    static OPEN = 1;
    static CLOSED = 3;
    constructor() {
      this.readyState = RefusingSocket.CLOSED;
      queueMicrotask(() => {
        this.onerror?.({});
        this.onclose?.({ code: 1006, reason: '' });
      });
    }
    send() {}
    close() {}
  }
  globalThis.WebSocket = RefusingSocket;
  const { AurixClient } = await import('../dist/client.js');
  const client = new AurixClient({ apiUrl: 'http://api', wsUrl: 'ws://dead/ws', token: 't', autoReconnect: false, pingIntervalMs: 0 });
  client.on('error', () => {});
  const states = [];
  client.on('connectionState', (s) => states.push(s));
  await assert.rejects(client.connect(), /websocket error/);
  assert.deepEqual(states, ['connecting', 'failed']);
  assert.equal(client.connectionState, 'failed');
  // The client is reusable: a second connect() starts over from "connecting".
  await assert.rejects(client.connect(), /websocket error/);
  assert.deepEqual(states.slice(2), ['connecting', 'failed']);
});
