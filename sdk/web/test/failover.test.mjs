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
