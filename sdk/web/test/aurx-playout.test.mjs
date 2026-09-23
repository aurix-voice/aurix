// SPDX-FileCopyrightText: 2026 The Aurix Authors
// SPDX-License-Identifier: Apache-2.0
import test from 'node:test';
import assert from 'node:assert/strict';
import vm from 'node:vm';
import { FakeAudioWorkletProcessor } from './helpers/fake-audio.mjs';
import {
  AURX_FRAME_SAMPLES,
  AURX_PLAYOUT_MAX_FRAMES,
  AURX_PLAYOUT_MIN_FRAMES,
  AurxPlayback,
  aurxWorkletSource,
  playoutTargetFrames,
} from '../dist/aurx-audio.js';

const FRAME = AURX_FRAME_SAMPLES;
const QUANTUM = 128;

/** Instantiate the player processor from the worklet source with a synchronous message port. */
function player(sampleRate = 48000) {
  const registered = new Map();
  const scope = {
    sampleRate,
    currentTime: 0,
    AudioWorkletProcessor: FakeAudioWorkletProcessor,
    registerProcessor: (name, cls) => registered.set(name, cls),
    Float32Array,
    Math,
    Number,
    Array,
    Error,
    Infinity,
    console,
  };
  scope.globalThis = scope;
  vm.runInNewContext(aurxWorkletSource(), scope, { filename: 'aurx-audio-worklet.js' });
  const Player = registered.get('aurix-aurx-player');
  const p = new Player();
  const sent = [];
  p.port.postMessage = (m) => sent.push(m);
  const send = (data) => p.port.onmessage({ data });
  const render = () => {
    const l = new Float32Array(QUANTUM);
    const r = new Float32Array(QUANTUM);
    p.process([], [[l, r]]);
    return [l, r];
  };
  return { p, send, render, sent };
}

/** One 20 ms frame of a sine at `hz`, continuing from `phase` (returns the new phase). */
function sineFrame(hz, phase, amp = 0.5) {
  const l = new Float32Array(FRAME);
  for (let i = 0; i < FRAME; i++) l[i] = amp * Math.sin(phase + (2 * Math.PI * hz * i) / 48000);
  return { planes: [l, l.slice()], phase: phase + (2 * Math.PI * hz * FRAME) / 48000 };
}

const rms = (v) => Math.sqrt(v.reduce((a, x) => a + x * x, 0) / v.length);
const maxStep = (v) => {
  let m = 0;
  for (let i = 1; i < v.length; i++) m = Math.max(m, Math.abs(v[i] - v[i - 1]));
  return m;
};

test('player: primes at the target depth, plays, and honours a raised target', () => {
  const { p, send, render } = player();
  send({ type: 'target', samples: 2 * FRAME });
  let phase = 0;
  let f = sineFrame(440, phase);
  send({ type: 'pcm', planes: f.planes });
  assert.equal(rms(render()[0]), 0, 'one frame queued: still silent');
  f = sineFrame(440, f.phase);
  send({ type: 'pcm', planes: f.planes });
  assert.ok(rms(render()[0]) > 0.3, 'two frames queued: playing');
  assert.equal(p.queued, 2 * FRAME - QUANTUM);
  send({ type: 'target', samples: 4 * FRAME });
  // Raising the target does not stop the running stream; it only applies when re-priming.
  assert.ok(rms(render()[0]) > 0.3);
});

test('player: conceals an underrun with pitch-repeated history, fades out within 60 ms, blends back in', () => {
  const { p, send, render, sent } = player();
  send({ type: 'target', samples: 2 * FRAME });
  let f = { phase: 0 };
  for (let k = 0; k < 4; k++) {
    f = sineFrame(220, f.phase);
    send({ type: 'pcm', planes: f.planes });
  }
  let last = new Float32Array(QUANTUM);
  while (p.queued >= QUANTUM) last = render()[0];
  const lastSample = last[QUANTUM - 1];
  // Dry: the first concealed quantum continues the waveform (no click) at nearly full level.
  const [c0] = render();
  assert.equal(sent.filter((m) => m.type === 'underrun').length, 1);
  assert.ok(Math.abs(c0[0] - lastSample) < 0.05, `continuous across the splice (${c0[0]} vs ${lastSample})`);
  assert.ok(maxStep(c0) < 0.02, 'no discontinuity inside the concealed quantum');
  assert.ok(rms(c0) > 0.3, 'concealment keeps the level up at first');
  // ... and decays to silence by 60 ms.
  let quanta = 1;
  let out = c0;
  while (rms(out) > 1e-6 && quanta < 40) {
    out = render()[0];
    quanta++;
  }
  assert.ok(quanta <= Math.ceil((3 * FRAME) / QUANTUM) + 1, `silent after ${quanta} quanta`);
  const stats = sent.filter((m) => m.type === 'stats').at(-1) ?? null;
  // Stats are periodic; force one by rendering until it shows up.
  let reported = stats;
  for (let i = 0; i < 70 && !reported; i++) {
    render();
    reported = sent.filter((m) => m.type === 'stats').at(-1) ?? null;
  }
  assert.ok(reported && reported.concealed >= 3 * FRAME - QUANTUM && reported.concealed <= 3 * FRAME + QUANTUM, `concealed ≈ 60 ms (${reported?.concealed})`);
  assert.equal(reported.underruns, 1);
  // Audio resumes: after the target depth is queued again the real signal plays without a click.
  for (let k = 0; k < 2; k++) {
    f = sineFrame(220, f.phase);
    send({ type: 'pcm', planes: f.planes });
  }
  const [r0] = render();
  assert.ok(rms(r0) > 0.2, 'playing again');
  assert.ok(maxStep(r0) < 0.1, 'crossfade back into real audio has no step');
});

test('player: a queue held above the target is shortened one pitch period at a time, seamlessly', () => {
  const { p, send, render, sent } = player();
  send({ type: 'target', samples: 2 * FRAME });
  let f = { phase: 0 };
  const feed = (n) => {
    for (let k = 0; k < n; k++) {
      f = sineFrame(100, f.phase);
      send({ type: 'pcm', planes: f.planes });
    }
  };
  feed(6); // 120 ms queued against a 40 ms target
  // Keep the depth constant: one frame in per 7.5 quanta out.
  let outQuanta = 0;
  let trimmedAt = -1;
  let worst = 0;
  let lowPoint = Infinity;
  for (let i = 0; i < 1600; i++) {
    const [l] = render();
    worst = Math.max(worst, maxStep(l));
    outQuanta++;
    if (outQuanta % 15 === 0) feed(2);
    if (i >= 1300) lowPoint = Math.min(lowPoint, p.queued);
    const s = sent.filter((m) => m.type === 'stats').at(-1);
    if (s && s.trimmed > 0 && trimmedAt < 0) trimmedAt = i;
  }
  assert.ok(trimmedAt > 700 && trimmedAt < 1000, `first trim after ~2 s of excess depth (quantum ${trimmedAt})`);
  const trimmed = sent.filter((m) => m.type === 'stats').at(-1).trimmed;
  // 100 Hz at 48 kHz is a 480-sample period: every removed chunk is a whole number of periods.
  assert.ok(trimmed >= FRAME && trimmed % 480 === 0, `whole periods removed (${trimmed})`);
  assert.ok(lowPoint <= 3 * FRAME, `depth back within a frame of the target (${lowPoint})`);
  // A 100 Hz sine at 0.5 changes by at most ~0.007 per sample; a period-aligned splice keeps that.
  assert.ok(worst < 0.02, `no discontinuity across the trims (max step ${worst})`);
  assert.equal(sent.filter((m) => m.type === 'underrun').length, 0);
});

test('player: a burst beyond the buffer drops the oldest and reports it', () => {
  const { p, send, render, sent } = player();
  for (let k = 0; k < 20; k++) send({ type: 'pcm', planes: sineFrame(300, 0).planes });
  assert.equal(p.queued, 16 * FRAME);
  for (let i = 0; i < 64; i++) render();
  const s = sent.filter((m) => m.type === 'stats').at(-1);
  assert.equal(s.overflowDropped, 4 * FRAME);
});

test('playoutTargetFrames: covers the late peak with headroom, clamped to the bounds', () => {
  assert.equal(playoutTargetFrames(0, 0), AURX_PLAYOUT_MIN_FRAMES);
  assert.equal(playoutTargetFrames(20, 2), AURX_PLAYOUT_MIN_FRAMES); // 25 + 8 = 33 ms → 2 frames
  assert.equal(playoutTargetFrames(60, 5), 5); // 75 + 8 = 83 ms → 5 frames
  assert.equal(playoutTargetFrames(0, 30), 5); // jitter alone: 90 + 8 → 5 frames
  assert.equal(playoutTargetFrames(1000, 0), AURX_PLAYOUT_MAX_FRAMES);
  assert.equal(playoutTargetFrames(1000, 0, 2, 6), 6);
});

/** `AurxPlayback` against a fake worklet node: what the main thread tells the player. */
class RecordingNode {
  constructor() {
    this.messages = [];
    this.port = { postMessage: (m) => this.messages.push(m), onmessage: null, close() {} };
  }
  disconnect() {}
  targets() {
    return this.messages.filter((m) => m.type === 'target').map((m) => m.samples / FRAME);
  }
}

test('AurxPlayback: the target grows on late bursts and underruns, then decays back', () => {
  const nodes = [];
  globalThis.AudioWorkletNode = class {
    constructor() {
      const n = new RecordingNode();
      nodes.push(n);
      return n;
    }
  };
  try {
    const pb = new AurxPlayback({}, (e) => assert.fail(e.message));
    let clock = 0;
    pb.now = () => clock;
    pb.node(7);
    const node = nodes[0];
    assert.deepEqual(node.targets(), [2], 'starts at the minimum depth');
    // G.711 frames keep the decoder out of the picture: every push is one 20 ms frame.
    const pcma = new Uint8Array(160).fill(0xd5);
    let seq = 1;
    let ts = 0;
    const arrive = (gapMs) => {
      clock += gapMs;
      pb.push(7, pcma, seq++, ts, false, 'pcma');
      ts = (ts + FRAME) >>> 0;
    };
    for (let i = 0; i < 50; i++) arrive(20);
    assert.equal(node.targets().at(-1), 2, 'steady 20 ms cadence: minimum');
    assert.ok(pb.stats.jitterMs < 0.01);
    // A 100 ms stall followed by a burst of the frames that were held up.
    arrive(120);
    arrive(0);
    arrive(0);
    arrive(0);
    arrive(0);
    // 100 ms late × 1.25 + 8 = 133 ms → 7 frames.
    assert.equal(node.targets().at(-1), 7, `target after the burst: ${node.targets()}`);
    assert.equal(pb.stats.targetDelayMs, 140);
    assert.ok(pb.stats.jitterMs > 5);
    // Underrun reported by the worklet: one frame more than what ran dry.
    node.port.onmessage({ data: { type: 'underrun', count: 1 } });
    assert.equal(node.targets().at(-1), 8);
    assert.equal(pb.stats.underruns, 1);
    // Quiet cadence again: the peak decays and the target walks back down to the minimum.
    for (let i = 0; i < 3000; i++) arrive(20);
    assert.equal(node.targets().at(-1), 2, `decayed: ${node.targets()}`);
    // The sender pauses for a second (silence between two clips): no buffer could have covered
    // that, so it is not jitter — the target stays put and only the underrun adds one frame.
    arrive(1_020);
    assert.equal(node.targets().at(-1), 2, `stream pause is not jitter: ${node.targets()}`);
    assert.ok(pb.stats.jitterMs < 1);
    node.port.onmessage({ data: { type: 'underrun', count: 2 } });
    assert.equal(node.targets().at(-1), 3);
    for (let i = 0; i < 3000; i++) arrive(20);
    assert.equal(node.targets().at(-1), 2);
    const t = node.targets();
    for (let i = 1; i < t.length; i++) assert.ok(Math.abs(t[i] - t[i - 1]) >= 1, 'each message changes the target');
    node.port.onmessage({ data: { type: 'stats', queued: 3 * FRAME, target: 2 * FRAME, underruns: 1, concealed: 2880, trimmed: 960, overflowDropped: 0 } });
    const s = pb.stats;
    assert.equal(s.concealedSamples, 2880);
    assert.equal(s.trimmedSamples, 960);
    assert.equal(s.depthMs, 60);
  } finally {
    delete globalThis.AudioWorkletNode;
  }
});
