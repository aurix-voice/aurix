import { test } from 'node:test';
import assert from 'node:assert/strict';
import { channelInfoFromJoinAck } from '../dist/client.js';
import { parseServerMessage } from '../dist/protocol.js';

const brief = (user_id, role = 'speaker') => ({
  user_id,
  display_name: user_id,
  ssrc: 1,
  role,
  is_muted: false,
  is_speaking: false,
});

test('listener join ack exposes role, total count and hidden listeners', () => {
  const msg = parseServerMessage(
    JSON.stringify({
      type: 'ChannelJoinAck',
      data: {
        channel_id: 'c1',
        participants: [brief('speaker-1'), brief('moderator-1', 'moderator')],
        role: 'listener',
        participant_count: 1843,
        hidden_listeners: true,
        transcription: true,
        safety_voice: false,
      },
    }),
  );
  assert.equal(msg.type, 'ChannelJoinAck');
  const info = channelInfoFromJoinAck(msg.data);
  assert.deepEqual(info, {
    role: 'listener',
    participantCount: 1843,
    hiddenListeners: true,
    transcription: true,
    safetyVoice: false,
  });
  assert.equal(info.role !== 'listener', false);
});

test('legacy join ack without audience fields defaults to a visible speaker', () => {
  const msg = parseServerMessage(
    JSON.stringify({
      type: 'ChannelJoinAck',
      data: { channel_id: 'c1', participants: [brief('a'), brief('b'), brief('me')] },
    }),
  );
  const info = channelInfoFromJoinAck(msg.data);
  assert.deepEqual(info, {
    role: 'speaker',
    participantCount: 3,
    hiddenListeners: false,
    transcription: false,
    safetyVoice: false,
  });
});

test('participant_count wins over the visible roster even when smaller rosters are radius-scoped', () => {
  const info = channelInfoFromJoinAck({
    channel_id: 'c1',
    participants: [brief('me')],
    role: 'speaker',
    participant_count: 12,
    hidden_listeners: false,
    roster_radius: 25,
  });
  assert.equal(info.participantCount, 12);
  assert.equal(info.hiddenListeners, false);
});
