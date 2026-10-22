import { test } from 'node:test';
import assert from 'node:assert/strict';
import { brief, connected, joined } from './helpers/fake-webrtc.mjs';

test('capture_unavailable flows from the join ack and CaptureStateChanged into the roster', async () => {
  const { client, sock } = await connected();
  const events = [];
  client.on('participantCaptureChanged', (ch, p, available) => events.push([ch, p.userId, available]));
  await joined(client, sock, 'c1', [brief('me', 7), { ...brief('alice', 8), capture_unavailable: true }, brief('bob', 9)]);
  const find = (u) => client.participants('c1').find((p) => p.userId === u);
  assert.equal(find('alice').captureUnavailable, true);
  assert.equal(find('bob').captureUnavailable, false, 'absent flag = has a microphone');

  sock.receive({ type: 'CaptureStateChanged', data: { channel_id: 'c1', user_id: 'alice', available: true } });
  sock.receive({ type: 'CaptureStateChanged', data: { channel_id: 'c1', user_id: 'bob', available: false } });
  assert.equal(find('alice').captureUnavailable, false);
  assert.equal(find('bob').captureUnavailable, true);
  assert.deepEqual(events, [
    ['c1', 'alice', true],
    ['c1', 'bob', false],
  ]);
});

test('setCaptureAvailable tells the server once per change and reads back', async () => {
  const { client, sock } = await connected();
  assert.equal(client.captureAvailable, true);
  const before = sock.sent.length;
  client.setCaptureAvailable(true);
  assert.equal(sock.sent.length, before, 'no-op when unchanged');
  client.setCaptureAvailable(false);
  client.setCaptureAvailable(false);
  assert.deepEqual(sock.sent.slice(before), [{ type: 'SetCaptureAvailable', data: { available: false } }]);
  assert.equal(client.captureAvailable, false);
  client.setCaptureAvailable(true);
  assert.equal(client.captureAvailable, true);
});
