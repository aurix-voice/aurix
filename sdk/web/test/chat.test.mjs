import { test } from 'node:test';
import assert from 'node:assert/strict';
import { encodeChatCursor } from '../dist/client.js';
import { parseServerMessage } from '../dist/protocol.js';
import { connected } from './helpers/fake-webrtc.mjs';

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

const wire = (extra = {}) => ({
  id: ID,
  channel_id: 'c1',
  from_user_id: 'u1',
  display_name: 'u1',
  text: 'hello',
  sent_at: '2024-05-06T07:08:09Z',
  offline: false,
  reactions: [],
  ...extra,
});

test('edit / delete resolve from ChatMessageUpdated by client_ref; foreign updates only emit', async () => {
  const { client, sock } = await connected();
  const updates = [];
  client.on('chatMessageUpdated', (m) => updates.push(m));

  const editing = client.editMessage(ID, 'fixed');
  await new Promise((r) => setTimeout(r, 0));
  const sent = sock.sent.find((m) => m.type === 'ChatEdit');
  assert.equal(sent.data.message_id, ID);
  assert.equal(sent.data.text, 'fixed');
  assert.equal(typeof sent.data.client_ref, 'string');

  // Someone else's edit arrives first: emitted, but does not settle our request.
  sock.receive({
    type: 'ChatMessageUpdated',
    data: { message: wire({ id: '00000000-0000-0000-0000-000000000002', text: 'x', edited_at: '2024-05-06T07:09:09Z' }) },
  });
  sock.receive({
    type: 'ChatMessageUpdated',
    data: {
      message: wire({ text: 'fixed', edited_at: '2024-05-06T07:09:09Z', client_ref: sent.data.client_ref }),
    },
  });
  const edited = await editing;
  assert.equal(edited.text, 'fixed');
  assert.equal(edited.editedAt.toISOString(), '2024-05-06T07:09:09.000Z');
  assert.equal(edited.deletedAt, undefined);
  assert.equal(updates.length, 2);

  const deleting = client.deleteMessage(ID);
  await new Promise((r) => setTimeout(r, 0));
  const del = sock.sent.find((m) => m.type === 'ChatDelete');
  sock.receive({
    type: 'ChatMessageUpdated',
    data: {
      message: wire({
        text: '',
        deleted_at: '2024-05-06T07:10:09Z',
        deleted_by: 'u1',
        client_ref: del.data.client_ref,
      }),
    },
  });
  const tombstone = await deleting;
  assert.equal(tombstone.deletedAt.toISOString(), '2024-05-06T07:10:09.000Z');
  assert.equal(tombstone.deletedBy, 'u1');
  assert.equal(tombstone.text, '');
  client.disconnect();
});

test('reactions are fire-and-forget; tallies and changes are exposed as camelCase models', async () => {
  const { client, sock } = await connected();
  const changes = [];
  client.on('chatReactionChanged', (c) => changes.push(c));
  client.react(ID, '👍');
  client.react(ID, '👍', false);
  const sent = sock.sent.filter((m) => m.type === 'ChatReact');
  assert.deepEqual(
    sent.map((m) => m.data),
    [
      { message_id: ID, reaction: '👍', add: true },
      { message_id: ID, reaction: '👍', add: false },
    ],
  );
  sock.receive({
    type: 'ChatReactionChanged',
    data: {
      message_id: ID,
      channel_id: 'c1',
      message_from_user_id: 'u1',
      user_id: 'u2',
      reaction: '👍',
      added: true,
      count: 3,
      timestamp: '2024-05-06T07:11:09Z',
    },
  });
  assert.equal(changes.length, 1);
  assert.equal(changes[0].messageId, ID);
  assert.equal(changes[0].channelId, 'c1');
  assert.equal(changes[0].messageToUserId, undefined);
  assert.equal(changes[0].count, 3);
  assert.ok(changes[0].timestamp instanceof Date);

  const messages = [];
  client.on('chatMessage', (m) => messages.push(m));
  sock.receive({
    type: 'ChatMessageReceived',
    data: { message: wire({ reactions: [{ reaction: '👍', count: 3, user_ids: ['u2', 'u3'] }] }) },
  });
  assert.deepEqual(messages[0].reactions, [{ reaction: '👍', count: 3, userIds: ['u2', 'u3'] }]);
  client.disconnect();
});

test('search sends the scope + query and pages through next_before', async () => {
  const { client, sock } = await connected();
  const searching = client.search({ channelId: 'c1' }, 'hello', { fromUserId: 'u1', limit: 2 });
  await new Promise((r) => setTimeout(r, 0));
  const sent = sock.sent.find((m) => m.type === 'ChatSearch');
  assert.equal(sent.data.channel_id, 'c1');
  assert.equal(sent.data.user_id, undefined);
  assert.equal(sent.data.query, 'hello');
  assert.equal(sent.data.from_user_id, 'u1');
  assert.equal(sent.data.limit, 2);
  sock.receive({
    type: 'ChatSearchResult',
    data: {
      channel_id: 'c1',
      query: 'hello',
      messages: [wire(), wire({ id: '00000000-0000-0000-0000-000000000002' })],
      next_before: CURSOR,
      client_ref: sent.data.client_ref,
    },
  });
  const page = await searching;
  assert.equal(page.query, 'hello');
  assert.equal(page.messages.length, 2);
  assert.equal(page.nextBefore, CURSOR);

  const direct = client.search({ userId: 'u2' }, 'hello', { before: CURSOR });
  await new Promise((r) => setTimeout(r, 0));
  const sent2 = sock.sent.filter((m) => m.type === 'ChatSearch').at(-1);
  assert.equal(sent2.data.user_id, 'u2');
  assert.equal(sent2.data.before, CURSOR);
  sock.receive({
    type: 'ChatSearchResult',
    data: { user_id: 'u2', query: 'hello', messages: [], client_ref: sent2.data.client_ref },
  });
  const last = await direct;
  assert.equal(last.messages.length, 0);
  assert.equal(last.nextBefore, undefined);

  const pending = client.search({ channelId: 'c1' }, 'never');
  client.disconnect();
  await assert.rejects(pending);
});
