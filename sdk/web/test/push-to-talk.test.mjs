import { test } from 'node:test';
import assert from 'node:assert/strict';
import { attachPushToTalk } from '../dist/push_to_talk.js';

class FakeTarget {
  constructor() {
    this.listeners = new Map();
  }
  addEventListener(type, fn) {
    if (!this.listeners.has(type)) this.listeners.set(type, new Set());
    this.listeners.get(type).add(fn);
  }
  removeEventListener(type, fn) {
    this.listeners.get(type)?.delete(fn);
  }
  emit(type, ev = {}) {
    for (const fn of this.listeners.get(type) ?? []) fn(ev);
  }
  count() {
    let n = 0;
    for (const s of this.listeners.values()) n += s.size;
    return n;
  }
}

function fakeClient() {
  const c = { muted: false, calls: [] };
  c.setMuted = (m) => {
    c.muted = m;
    c.calls.push(m);
  };
  c.isMuted = () => c.muted;
  return c;
}

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

test('attaching mutes; holding the key transmits; release stops after the delay', async () => {
  const client = fakeClient();
  const target = new FakeTarget();
  const changes = [];
  const ptt = attachPushToTalk(client, { keys: ['KeyV'], target, releaseDelayMs: 20, onChange: (t) => changes.push(t) });
  assert.equal(client.muted, true, 'starts muted');
  assert.equal(ptt.transmitting, false);

  target.emit('keydown', { code: 'KeyV' });
  assert.equal(ptt.transmitting, true);
  assert.equal(client.muted, false);
  target.emit('keydown', { code: 'KeyV', repeat: true });
  assert.deepEqual(client.calls, [true, false], 'auto-repeat does not toggle');

  target.emit('keyup', { code: 'KeyV' });
  assert.equal(ptt.transmitting, true, 'still open during the release delay');
  await sleep(40);
  assert.equal(ptt.transmitting, false);
  assert.equal(client.muted, true);
  assert.deepEqual(changes, [true, false]);
  ptt.dispose();
});

test('pressing again inside the release delay keeps the mic open without a gap', async () => {
  const client = fakeClient();
  const target = new FakeTarget();
  const ptt = attachPushToTalk(client, { keys: ['Space'], target, releaseDelayMs: 30 });
  target.emit('keydown', { code: 'Space' });
  target.emit('keyup', { code: 'Space' });
  await sleep(10);
  target.emit('keydown', { code: 'Space' });
  await sleep(40);
  assert.equal(ptt.transmitting, true);
  assert.deepEqual(client.calls, [true, false], 'no mute/unmute flicker');
  ptt.dispose();
});

test('other keys are ignored; any of several bound keys transmits', () => {
  const client = fakeClient();
  const target = new FakeTarget();
  const ptt = attachPushToTalk(client, { keys: ['KeyV', 'ControlLeft'], target, releaseDelayMs: 0 });
  target.emit('keydown', { code: 'KeyX' });
  assert.equal(ptt.transmitting, false);
  target.emit('keydown', { code: 'ControlLeft' });
  assert.equal(ptt.transmitting, true);
  ptt.dispose();
});

test('toggle mode flips on each press and ignores keyup', () => {
  const client = fakeClient();
  const target = new FakeTarget();
  const ptt = attachPushToTalk(client, { keys: ['KeyV'], target, toggle: true });
  target.emit('keydown', { code: 'KeyV' });
  target.emit('keyup', { code: 'KeyV' });
  assert.equal(ptt.transmitting, true);
  assert.equal(client.muted, false);
  target.emit('keydown', { code: 'KeyV' });
  assert.equal(ptt.transmitting, false);
  assert.equal(client.muted, true);
  ptt.dispose();
});

test('window blur releases a held key', async () => {
  const client = fakeClient();
  const target = new FakeTarget();
  const ptt = attachPushToTalk(client, { keys: ['KeyV'], target, releaseDelayMs: 0 });
  target.emit('keydown', { code: 'KeyV' });
  target.emit('blur');
  await sleep(5);
  assert.equal(ptt.transmitting, false);
  assert.equal(client.muted, true);
  ptt.dispose();
});

test('ignoreWhenTyping skips editable targets, and can be turned off', () => {
  const input = { tagName: 'INPUT' };
  const client = fakeClient();
  const target = new FakeTarget();
  const ptt = attachPushToTalk(client, { keys: ['KeyV'], target });
  target.emit('keydown', { code: 'KeyV', target: input });
  assert.equal(ptt.transmitting, false);
  ptt.dispose();

  const client2 = fakeClient();
  const ptt2 = attachPushToTalk(client2, { keys: ['KeyV'], target, ignoreWhenTyping: false });
  target.emit('keydown', { code: 'KeyV', target: input });
  assert.equal(ptt2.transmitting, true);
  ptt2.dispose();
});

test('press(bool) drives hold mode for gamepads and on-screen buttons', async () => {
  const client = fakeClient();
  const ptt = attachPushToTalk(client, { keys: [], target: new FakeTarget(), releaseDelayMs: 10 });
  ptt.press(true);
  assert.equal(ptt.transmitting, true);
  ptt.press(false);
  assert.equal(ptt.transmitting, true);
  await sleep(25);
  assert.equal(ptt.transmitting, false);
  ptt.dispose();
});

test('press(true) in toggle mode flips; press(false) is ignored', () => {
  const client = fakeClient();
  const ptt = attachPushToTalk(client, { keys: [], target: new FakeTarget(), toggle: true });
  ptt.press(true);
  ptt.press(false);
  assert.equal(ptt.transmitting, true);
  ptt.press(true);
  assert.equal(ptt.transmitting, false);
  ptt.dispose();
});

test('dispose unbinds listeners, cancels the release timer and mutes', async () => {
  const client = fakeClient();
  const target = new FakeTarget();
  const ptt = attachPushToTalk(client, { keys: ['KeyV'], target, releaseDelayMs: 1000 });
  assert.equal(target.count(), 3);
  target.emit('keydown', { code: 'KeyV' });
  target.emit('keyup', { code: 'KeyV' });
  ptt.dispose();
  assert.equal(target.count(), 0);
  assert.equal(ptt.transmitting, false);
  assert.equal(client.muted, true);
  const calls = client.calls.length;
  target.emit('keydown', { code: 'KeyV' });
  assert.equal(client.calls.length, calls, 'no effect after dispose');
});

test('does not touch mute when the client is already in the wanted state', () => {
  const client = fakeClient();
  client.muted = true;
  attachPushToTalk(client, { keys: ['KeyV'], target: new FakeTarget() }).dispose();
  assert.deepEqual(client.calls, [], 'already muted on attach and on dispose');
});
