import { test } from 'node:test';
import assert from 'node:assert/strict';
import { encodeChatCursor } from '../dist/client.js';
import { parseServerMessage } from '../dist/protocol.js';

// Same vector as `chat_cursor_round_trips` in crates/aurix-common/src/protocol.rs.
const ID = '0192f3a4-b5c6-7d8e-9f01-23456789abcd';
const CURSOR = 'AAYXw7tTSoABkvOktcZ9jp8BI0VniavN';

test('cursor matches the server encoding, keeping sub-millisecond precision', () => {
  assert.equal(encodeChatCursor('2024-05-06T07:08:09.123456Z', ID), CURSOR);
  assert.equal(encodeChatCursor('2024-05-06T07:08:09.123456789Z', ID), CURSOR);
  assert.equal(encodeChatCursor('2024-05-06T09:08:09.123456+02:00', ID), CURSOR);
  assert.notEqual(encodeChatCursor('2024-05-06T07:08:09.123Z', ID), CURSOR);
  assert.equal(
    encodeChatCursor(new Date('2024-05-06T07:08:09.123Z'), ID),
    encodeChatCursor('2024-05-06T07:08:09.123000Z', ID),
  );
  assert.throws(() => encodeChatCursor('2024-05-06T07:08:09Z', 'nope'));
});

test('cursors of equal timestamps differ by id and are URL-safe', () => {
  const a = encodeChatCursor('2024-05-06T07:08:09Z', '00000000-0000-0000-0000-000000000001');
  const b = encodeChatCursor('2024-05-06T07:08:09Z', '00000000-0000-0000-0000-000000000002');
  assert.notEqual(a, b);
  assert.match(a, /^[A-Za-z0-9_-]+$/);
  assert.equal(a.length, 32);
});

test('history / read-marker payloads parse with optional fields absent', () => {
  const history = parseServerMessage(
    JSON.stringify({
      type: 'ChatHistoryResult',
      data: { channel_id: 'c1', messages: [], client_ref: 'h1' },
    }),
  );
  assert.equal(history.type, 'ChatHistoryResult');
  assert.equal(history.data.next_before, undefined);
  const marker = parseServerMessage(
    JSON.stringify({
      type: 'ChatReadMarker',
      data: {
        marker: {
          user_id: 'u1',
          peer_user_id: 'u2',
          message_id: ID,
          message_sent_at: '2024-05-06T07:08:09Z',
          read_at: '2024-05-06T07:08:10Z',
        },
      },
    }),
  );
  assert.equal(marker.type, 'ChatReadMarker');
  assert.equal(marker.data.marker.channel_id, undefined);
  const synced = parseServerMessage(
    JSON.stringify({ type: 'ChatInboxSynced', data: { delivered: 3, truncated: false } }),
  );
  assert.equal(synced.type, 'ChatInboxSynced');
  assert.equal(synced.data.delivered, 3);
});
