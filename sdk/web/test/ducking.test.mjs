import { test } from 'node:test';
import assert from 'node:assert/strict';
import { brief, connected, joined } from './helpers/fake-webrtc.mjs';

const ducking = { gain: 0.2, attack_ms: 50, release_ms: 300, hold_ms: 0, moderators: true };

/** `[volume, duck]` gains on the graph behind `mid`. */
const gainsOf = (ctx, mid) => {
  const source = ctx.created.find((n) => n.kind === 'source' && n.stream.mid === mid);
  const volume = [...source.outputs][0];
  const duck = [...volume.outputs][0];
  return [volume.gain.value, duck.gain.value];
};

const layout = (sock, ...pairs) =>
  sock.receive({ type: 'ParticipantStreams', data: { streams: pairs.map(([mid, user_id]) => ({ mid, user_id })) } });

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

test('join ack / roster / PriorityChanged expose explicit priority and the channel ducking config', async () => {
  const { client, sock } = await connected();
  const events = [];
  client.on('participantPriorityChanged', (...a) => events.push(a));
  await joined(client, sock, 'c1', [brief('me', 7), { ...brief('alice', 8), is_priority: true }, { ...brief('mod', 9), role: 'moderator' }], {
    ducking,
    priority: false,
  });
  assert.deepEqual(client.getChannelDucking('c1'), { gain: 0.2, attackMs: 50, releaseMs: 300, holdMs: 0, moderators: true });
  assert.equal(client.isPriority('c1'), false);
  assert.equal(client.participants('c1').find((p) => p.userId === 'alice').priority, true);
  assert.equal(client.participants('c1').find((p) => p.userId === 'mod').priority, false, 'moderators are not marked priority; the ducking config decides');
  assert.equal(client.isDuckingActive('c1'), false, 'nobody speaks yet');

  client.setPriority('c1', true);
  assert.deepEqual(sock.sent.at(-1), { type: 'SetPriority', data: { channel_id: 'c1', user_id: null, priority: true } });
  client.setPriority('c1', false, 'alice');
  assert.deepEqual(sock.sent.at(-1), { type: 'SetPriority', data: { channel_id: 'c1', user_id: 'alice', priority: false } });

  sock.receive({ type: 'PriorityChanged', data: { channel_id: 'c1', user_id: 'me', priority: true } });
  assert.equal(client.isPriority('c1'), true);
  sock.receive({ type: 'PriorityChanged', data: { channel_id: 'c1', user_id: 'alice', priority: false } });
  assert.equal(client.participants('c1').find((p) => p.userId === 'alice').priority, false);
  assert.deepEqual(events, [
    ['c1', 'me', true],
    ['c1', 'alice', false],
  ]);

  // Channels without ducking never duck, whatever the roles say.
  await joined(client, sock, 'plain', [brief('me', 7), { ...brief('boss', 10), is_priority: true }]);
  assert.equal(client.getChannelDucking('plain'), undefined);
  sock.receive({ type: 'SpeakingStateChanged', data: { channel_id: 'plain', user_id: 'boss', speaking: true } });
  assert.equal(client.isDuckingActive('plain'), false);
});

test('a speaking priority member ducks the other dedicated tracks (not their own), fires transition-only game events, and coexists with volume / mute / focus', async () => {
  const { client, sock, pc, ctx } = await connected({ participantStreams: 2 });
  pc.arrive('0');
  pc.arrive('1');
  pc.arrive('2');
  const game = [];
  client.on('duckingChanged', (...a) => game.push(a));
  await joined(client, sock, 'c1', [brief('me', 7), { ...brief('alice', 8), is_priority: true }, brief('bob', 9)], { ducking });
  layout(sock, ['1', 'alice'], ['2', 'bob']);
  assert.deepEqual(gainsOf(ctx, '1'), [1, 1]);
  assert.deepEqual(gainsOf(ctx, '2'), [1, 1]);

  sock.receive({ type: 'SpeakingStateChanged', data: { channel_id: 'c1', user_id: 'alice', speaking: true } });
  assert.equal(client.isDuckingActive('c1'), true);
  assert.deepEqual(gainsOf(ctx, '1'), [1, 1], 'the priority voice is never ducked');
  assert.deepEqual(gainsOf(ctx, '2'), [1, 0.2], 'bob is ducked to the channel gain');
  assert.deepEqual(game, [['c1', true, client.getChannelDucking('c1')]]);

  // Speaking flaps of the priority member while active do not re-fire the game hook.
  sock.receive({ type: 'SpeakingStateChanged', data: { channel_id: 'c1', user_id: 'alice', speaking: true } });
  assert.equal(game.length, 1);

  // Local controls stack on top of ducking on their own gain node.
  client.setParticipantVolume('bob', 0.5);
  assert.deepEqual(gainsOf(ctx, '2'), [0.5, 0.2]);
  client.setParticipantMuted('bob', true, 'c1');
  assert.deepEqual(gainsOf(ctx, '2'), [0, 0.2]);
  client.setParticipantMuted('bob', false, 'c1');
  await joined(client, sock, 'other', [brief('me', 7)]);
  client.setChannelFocus('other');
  assert.deepEqual(gainsOf(ctx, '2'), [0.5 * 0.4, 0.2], 'unfocused gain multiplies the volume, ducking stays separate');
  client.setChannelFocus(undefined);
  assert.deepEqual(gainsOf(ctx, '2'), [0.5, 0.2]);

  // Bob is ducked but that never makes him a priority member: his speech ducks nobody.
  sock.receive({ type: 'SpeakingStateChanged', data: { channel_id: 'c1', user_id: 'bob', speaking: true } });
  assert.deepEqual(gainsOf(ctx, '1'), [1, 1]);

  sock.receive({ type: 'SpeakingStateChanged', data: { channel_id: 'c1', user_id: 'alice', speaking: false } });
  assert.equal(client.isDuckingActive('c1'), false, 'hold_ms 0: released at once');
  assert.deepEqual(gainsOf(ctx, '2'), [0.5, 1]);
  assert.deepEqual(game.at(-1), ['c1', false, client.getChannelDucking('c1')]);
  assert.equal(game.length, 2);

  // Two simultaneous priority speakers: ducking holds until the last one stops.
  sock.receive({ type: 'PriorityChanged', data: { channel_id: 'c1', user_id: 'bob', priority: true } });
  sock.receive({ type: 'SpeakingStateChanged', data: { channel_id: 'c1', user_id: 'alice', speaking: true } });
  sock.receive({ type: 'SpeakingStateChanged', data: { channel_id: 'c1', user_id: 'bob', speaking: true } });
  assert.equal(game.length, 3);
  assert.deepEqual(gainsOf(ctx, '2'), [0.5, 1], 'bob is a priority member now: not ducked');
  sock.receive({ type: 'SpeakingStateChanged', data: { channel_id: 'c1', user_id: 'alice', speaking: false } });
  assert.equal(client.isDuckingActive('c1'), true, 'bob still speaks');
  sock.receive({ type: 'SpeakingStateChanged', data: { channel_id: 'c1', user_id: 'bob', speaking: false } });
  assert.equal(client.isDuckingActive('c1'), false);
  assert.equal(game.length, 4);
});

test('hold_ms delays the release, leaving the channel drops it at once, moderators duck only when configured, and our own priority speech ducks the others locally', async () => {
  const { client, sock, pc, ctx } = await connected({ participantStreams: 2 });
  pc.arrive('0');
  pc.arrive('1');
  pc.arrive('2');
  const game = [];
  client.on('duckingChanged', (ch, active) => game.push([ch, active]));
  await joined(client, sock, 'c1', [brief('me', 7), { ...brief('mod', 8), role: 'moderator' }, brief('bob', 9)], {
    ducking: { ...ducking, hold_ms: 30 },
  });
  layout(sock, ['1', 'mod'], ['2', 'bob']);

  sock.receive({ type: 'SpeakingStateChanged', data: { channel_id: 'c1', user_id: 'mod', speaking: true } });
  assert.equal(client.isDuckingActive('c1'), true, 'moderators: true → the moderator role ducks');
  sock.receive({ type: 'SpeakingStateChanged', data: { channel_id: 'c1', user_id: 'mod', speaking: false } });
  assert.equal(client.isDuckingActive('c1'), true, 'held');
  assert.deepEqual(gainsOf(ctx, '2'), [1, 0.2]);
  sock.receive({ type: 'SpeakingStateChanged', data: { channel_id: 'c1', user_id: 'mod', speaking: true } });
  await sleep(45);
  assert.equal(client.isDuckingActive('c1'), true, 'resumed speech within the hold cancels the release');
  assert.deepEqual(game, [['c1', true]]);
  sock.receive({ type: 'SpeakingStateChanged', data: { channel_id: 'c1', user_id: 'mod', speaking: false } });
  await sleep(45);
  assert.equal(client.isDuckingActive('c1'), false);
  assert.deepEqual(gainsOf(ctx, '2'), [1, 1]);
  assert.deepEqual(game, [
    ['c1', true],
    ['c1', false],
  ]);

  // Leaving mid-hold releases immediately (no stale timer firing into a channel we left).
  sock.receive({ type: 'SpeakingStateChanged', data: { channel_id: 'c1', user_id: 'mod', speaking: true } });
  sock.receive({ type: 'SpeakingStateChanged', data: { channel_id: 'c1', user_id: 'mod', speaking: false } });
  assert.equal(client.isDuckingActive('c1'), true);
  const leaving = client.leaveChannel('c1');
  await sleep(0);
  sock.receive({ type: 'ChannelLeft', data: { channel_id: 'c1' } });
  await leaving;
  assert.equal(client.isDuckingActive('c1'), false);
  assert.equal(game.length, 4);
  await sleep(45);
  assert.equal(game.length, 4, 'the hold timer was cancelled');

  // moderators: false → the role alone does not duck; explicit priority does.
  await joined(client, sock, 'c2', [brief('me', 7), { ...brief('mod', 8), role: 'moderator' }, brief('bob', 9)], {
    ducking: { ...ducking, moderators: false },
  });
  layout(sock, ['1', 'mod'], ['2', 'bob']);
  sock.receive({ type: 'SpeakingStateChanged', data: { channel_id: 'c2', user_id: 'mod', speaking: true } });
  assert.equal(client.isDuckingActive('c2'), false);
  sock.receive({ type: 'PriorityChanged', data: { channel_id: 'c2', user_id: 'mod', priority: true } });
  assert.equal(client.isDuckingActive('c2'), true, 'PriorityChanged on a speaking member re-evaluates');
  sock.receive({ type: 'PriorityChanged', data: { channel_id: 'c2', user_id: 'mod', priority: false } });
  sock.receive({ type: 'SpeakingStateChanged', data: { channel_id: 'c2', user_id: 'mod', speaking: false } });
  await sleep(45);
  assert.equal(client.isDuckingActive('c2'), false);

  // Our own priority speech: the server ducks the mix for us, so the tracks follow — but the
  // game hook is about *others* ducking us and stays quiet.
  const before = game.length;
  sock.receive({ type: 'PriorityChanged', data: { channel_id: 'c2', user_id: 'me', priority: true } });
  sock.receive({ type: 'SpeakingStateChanged', data: { channel_id: 'c2', user_id: 'me', speaking: true } });
  assert.deepEqual(gainsOf(ctx, '2'), [1, 0.2], 'bob’s track ducks under our own priority speech');
  assert.deepEqual(gainsOf(ctx, '1'), [1, 0.2]);
  assert.equal(client.isDuckingActive('c2'), false);
  assert.equal(game.length, before);
  sock.receive({ type: 'SpeakingStateChanged', data: { channel_id: 'c2', user_id: 'me', speaking: false } });
  assert.deepEqual(gainsOf(ctx, '2'), [1, 1]);

  // Session gone: everything released.
  sock.receive({ type: 'PriorityChanged', data: { channel_id: 'c2', user_id: 'bob', priority: true } });
  sock.receive({ type: 'SpeakingStateChanged', data: { channel_id: 'c2', user_id: 'bob', speaking: true } });
  assert.equal(client.isDuckingActive('c2'), true);
  await client.disconnect();
  assert.equal(client.isDuckingActive('c2'), false);
  assert.deepEqual(game.at(-1), ['c2', false]);
});
