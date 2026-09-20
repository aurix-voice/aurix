import { test } from 'node:test';
import assert from 'node:assert/strict';
import { AurixClient } from '../dist/client.js';
import { FakeAudioContext, FakeAudioWorkletNode, flushPorts } from './helpers/fake-audio.mjs';
import { brief, connected, joined } from './helpers/fake-webrtc.mjs';

globalThis.AudioContext = FakeAudioContext;
globalThis.AudioWorkletNode = FakeAudioWorkletNode;

const FRAME = 960;
const tone = (hz, n = FRAME, amp = 0.3) => Float32Array.from({ length: n }, (_, i) => Math.sin((2 * Math.PI * hz * i) / 48000) * amp);
const rms = (buf) => Math.sqrt(buf.reduce((s, v) => s + v * v, 0) / buf.length);
const kind = (ctx, k) => ctx.created.filter((n) => n.kind === k);

/** Pulse train through two resonators — a synthetic /aa/ (see visemes.test.mjs). */
function vowel(frames) {
  const len = frames * FRAME;
  const out = new Float32Array(len);
  for (const [formant, gain] of [
    [750, 1],
    [1250, 0.5],
  ]) {
    const r = Math.exp((-Math.PI * 80) / 48000);
    const a1 = -2 * r * Math.cos((2 * Math.PI * formant) / 48000);
    const a2 = r * r;
    let y1 = 0;
    let y2 = 0;
    for (let n = 0; n < len; n++) {
      const y = (n % 369 === 0 ? 1 : 0) - a1 * y1 - a2 * y2;
      y2 = y1;
      y1 = y;
      out[n] += y * gain;
    }
  }
  const peak = out.reduce((m, s) => Math.max(m, Math.abs(s)), 0);
  for (let i = 0; i < len; i++) out[i] *= 0.4 / peak;
  return out;
}

/** The microphone graph of the input pipeline (the context the client created for it). */
function micGraph() {
  const ctx = FakeAudioContext.all.findLast((c) => c.created.some((n) => n.kind === 'mediaStreamDestination'));
  const destination = kind(ctx, 'mediaStreamDestination')[0];
  const source = kind(ctx, 'source')[0];
  const gain = [...source.outputs][0];
  const micSwitch = [...gain.outputs][0];
  const injectGain = kind(ctx, 'gain').find((g) => g !== gain && g !== micSwitch && g.outputs.has(destination));
  return { ctx, destination, source, gain, micSwitch, injectGain };
}

test('capabilities reflect the AudioWorklet globals', () => {
  assert.equal(AurixClient.supportsVoiceEffects(), true);
  assert.equal(AurixClient.supportsVisemes(), true);
  const saved = globalThis.AudioWorkletNode;
  delete globalThis.AudioWorkletNode;
  assert.equal(AurixClient.supportsVoiceEffects(), false);
  assert.equal(AurixClient.supportsVisemes(), false);
  globalThis.AudioWorkletNode = saved;
});

test('voice effects run in a worklet on the microphone path only: after the gain, before the encoder, never on injected audio; presets swap in place and bypass removes the node', async () => {
  const { client } = await connected({ voiceEffects: 'robot' });
  assert.deepEqual(client.voiceEffects, { ...client.voiceEffects, ringModHz: 60, pitchSemitones: 0 });
  await flushPorts();
  await flushPorts();
  const { ctx, destination, micSwitch, injectGain } = micGraph();
  const effects = kind(ctx, 'worklet:aurix-voice-effects');
  assert.equal(effects.length, 1, 'one effects node created from the constructor option');
  const node = effects[0];
  assert.deepEqual([...micSwitch.outputs], [node], 'mic → effects');
  assert.ok(node.outputs.has(destination), 'effects → encoder track');
  assert.ok(injectGain.outputs.has(destination) && !injectGain.outputs.has(node), 'injection bypasses the effects');

  const dry = tone(220);
  const wet = node.render([dry], 1)[0];
  assert.ok(rms(wet) > 0.05, 'audio passes');
  let diff = 0;
  for (let i = 0; i < FRAME; i++) diff = Math.max(diff, Math.abs(wet[i] - dry[i]));
  assert.ok(diff > 0.1, `robot ring-mod changes the signal (max diff ${diff})`);

  // Switching presets re-parameterises the running node instead of rebuilding the graph.
  await client.setVoiceEffects('radio');
  await flushPorts();
  assert.equal(kind(ctx, 'worklet:aurix-voice-effects').length, 1);
  assert.equal(client.voiceEffects.highpassHz, 400);
  const radio = node.render([tone(100, FRAME * 4)], 1)[0].subarray(FRAME * 3);
  assert.ok(rms(radio) < rms(tone(100)) * 0.5, 'radio highpass attenuates 100 Hz');

  // Explicit params: NaN / out-of-range values are sanitised.
  await client.setVoiceEffects({ pitchSemitones: 99, reverbMix: Number.NaN });
  assert.equal(client.voiceEffects.pitchSemitones, 24);
  assert.equal(client.voiceEffects.reverbMix, 0);

  // Bypass: the node is gone and the microphone feeds the encoder directly again.
  await client.setVoiceEffects(undefined);
  assert.deepEqual([...micSwitch.outputs], [destination]);
  assert.equal(node.port.closed, true);
  await client.setVoiceEffects({ tremoloHz: 5 });
  assert.deepEqual([...micSwitch.outputs], [destination], 'tremolo without depth is bypass, no node created');

  // Back on: a fresh node.
  await client.setVoiceEffects('ghost');
  const again = kind(ctx, 'worklet:aurix-voice-effects');
  assert.equal(again.length, 2);
  assert.deepEqual([...micSwitch.outputs], [again[1]]);
  await client.disconnect();
});

test('visemes: participant frames come off each dedicated track after decoding, the local frame off the processed microphone; layout changes reset, leaving clears', async () => {
  const { client, sock, pc, ctx } = await connected({ participantStreams: 2, visemes: true });
  pc.arrive('0');
  pc.arrive('1');
  pc.arrive('2');
  await flushPorts();
  await flushPorts();
  assert.equal(client.visemesEnabled, true);

  const taps = kind(ctx, 'worklet:aurix-visemes');
  assert.equal(taps.length, 2, 'one tap per dedicated track (the mixed track is not analysable)');
  const sourceOf = (tap) => ctx.created.find((n) => n.kind === 'source' && n.outputs.has(tap));
  const mids = taps.map((t) => sourceOf(t).stream.mid).sort();
  assert.deepEqual(mids, ['1', '2']);
  const tapFor = (mid) => taps.find((t) => sourceOf(t).stream.mid === mid);

  await joined(client, sock, 'c1', [brief('me', 7), brief('alice', 8), brief('bob', 9)]);
  const frames = [];
  client.on('participantVisemes', (u, f) => frames.push([u, f]));
  sock.receive({ type: 'ParticipantStreams', data: { streams: [{ mid: '1', user_id: 'alice' }, { mid: '2', user_id: null }] } });

  const aa = vowel(12);
  for (let off = 0; off < aa.length; off += 128) tapFor('1').render([aa.subarray(off, off + 128)], 0);
  await flushPorts();
  assert.equal(frames.length, 12, 'one frame per 20 ms');
  assert.ok(frames.every(([u]) => u === 'alice'));
  const last = client.getParticipantVisemes('alice');
  assert.equal(last.dominant, 'aa');
  assert.ok(last.mouthOpen > 0.15);
  assert.equal(last.sequence, 12);
  assert.equal(client.getParticipantVisemes('bob'), undefined, 'no dedicated track → no frames');

  // Audio on an unassigned track is nobody's: dropped, not attributed.
  frames.length = 0;
  tapFor('2').render([aa.subarray(0, 128)], 0);
  for (let off = 128; off < FRAME; off += 128) tapFor('2').render([aa.subarray(off, off + 128)], 0);
  await flushPorts();
  assert.equal(frames.length, 0);

  // The track changes hands: alice's frame is forgotten and bob's analysis starts clean.
  sock.receive({ type: 'ParticipantStreams', data: { streams: [{ mid: '1', user_id: 'bob' }, { mid: '2', user_id: null }] } });
  await flushPorts();
  assert.equal(client.getParticipantVisemes('alice'), undefined);
  for (let off = 0; off < FRAME; off += 128) tapFor('1').render([new Float32Array(128)], 0);
  await flushPorts();
  const bob = client.getParticipantVisemes('bob');
  assert.equal(bob.dominant, 'sil');
  assert.equal(bob.mouthOpen, 0, 'reset: no smoothing tail from alice’s vowel');

  // Local lip-sync off the processed microphone (a tap after the effects slot).
  const local = [];
  client.on('localVisemes', (f) => local.push(f));
  const mic = micGraph();
  const micTap = kind(mic.ctx, 'worklet:aurix-visemes')[0];
  assert.ok(micTap, 'microphone tap present');
  assert.ok(mic.micSwitch.outputs.has(micTap), 'tapped after the gain');
  assert.ok(!mic.injectGain.outputs.has(micTap), 'injected audio is not lip-synced');
  for (let off = 0; off < aa.length; off += 128) micTap.render([aa.subarray(off, off + 128)], 0);
  await flushPorts();
  assert.equal(local.length, 12);
  assert.equal(client.getLocalVisemes().dominant, 'aa');

  // Effects on: the local tap moves behind the effects node so it sees what the others hear.
  await client.setVoiceEffects('monster');
  await flushPorts();
  const fx = kind(mic.ctx, 'worklet:aurix-voice-effects')[0];
  assert.ok(fx.outputs.has(micTap) && !mic.micSwitch.outputs.has(micTap), 'tap follows the effects');
  assert.ok(fx.outputs.has(mic.destination));

  // Off: taps are removed and frames cleared.
  await client.setVisemes(false);
  assert.equal(client.getParticipantVisemes('bob'), undefined);
  assert.equal(client.getLocalVisemes(), undefined);
  assert.ok(taps.every((t) => !sourceOf(t)), 'taps disconnected from the tracks');
  assert.equal(fx.outputs.has(micTap), false);

  // On again later: taps come back for the tracks that exist now.
  await client.setVisemes(true);
  await flushPorts();
  assert.equal(kind(ctx, 'worklet:aurix-visemes').length, 4);
  assert.equal(kind(ctx, 'worklet:aurix-visemes').filter((t) => sourceOf(t)).length, 2);

  await client.disconnect();
  assert.equal(client.getParticipantVisemes('bob'), undefined);
});

test('without AudioWorklet support enabling visemes / effects rejects instead of silently doing nothing', async () => {
  const saved = globalThis.AudioWorkletNode;
  delete globalThis.AudioWorkletNode;
  try {
    const { client } = await connected();
    await assert.rejects(client.setVisemes(true), /AudioWorklet is unavailable/);
    assert.equal(client.visemesEnabled, false);
    await assert.rejects(client.setVoiceEffects('robot'), /AudioWorklet is unavailable/);
    await client.setVoiceEffects(undefined);
    await client.disconnect();
  } finally {
    globalThis.AudioWorkletNode = saved;
  }
});
