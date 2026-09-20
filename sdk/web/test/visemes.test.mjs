import { test } from 'node:test';
import assert from 'node:assert/strict';
import vm from 'node:vm';
import { VISEMES, VisemeAnalyzer, silentVisemeFrame, visemeWorkletSource } from '../dist/visemes.js';
import { BiquadStage } from '../dist/effects.js';
import { FakeAudioWorkletProcessor } from './helpers/fake-audio.mjs';

const SR = 48000;
const FRAME = SR / 50;

/** Pulse train at `f0` through two resonators — a synthetic vowel (as in the native tests). */
function vowel(f0, f1, f2, frames) {
  const period = Math.floor(SR / f0);
  const len = frames * FRAME;
  const out = new Float32Array(len);
  for (const [formant, gain] of [
    [f1, 1],
    [f2, 0.5],
  ]) {
    const r = Math.exp((-Math.PI * 80) / SR);
    const theta = (2 * Math.PI * formant) / SR;
    const a1 = -2 * r * Math.cos(theta);
    const a2 = r * r;
    let y1 = 0;
    let y2 = 0;
    for (let n = 0; n < len; n++) {
      const x = n % period === 0 ? 1 : 0;
      const y = x - a1 * y1 - a2 * y2;
      y2 = y1;
      y1 = y;
      out[n] += y * gain;
    }
  }
  const peak = out.reduce((m, s) => Math.max(m, Math.abs(s)), 0);
  for (let i = 0; i < len; i++) out[i] *= 0.4 / peak;
  return out;
}

function noise(seed, len, amp) {
  const out = new Float32Array(len);
  for (let i = 0; i < len; i++) {
    seed.v = (Math.imul(seed.v, 1664525) + 1013904223) >>> 0;
    out[i] = (((seed.v >>> 8) / 16777216) * 2 - 1) * amp;
  }
  return out;
}

function filtered(pcm, ...stages) {
  const out = pcm.slice();
  for (let off = 0; off < out.length; off += FRAME) {
    const block = [out.subarray(off, off + FRAME)];
    for (const s of stages) s.process(block, block[0].length);
  }
  return out;
}

function analyse(pcm, channels = 1) {
  const a = new VisemeAnalyzer(SR);
  const step = FRAME * channels;
  for (let off = 0; off + step <= pcm.length; off += step) a.push(pcm.subarray(off, off + step), channels);
  return a.frame();
}

test('silence and sub-floor hiss are a closed mouth; the default frame is silence', () => {
  const d = silentVisemeFrame();
  assert.equal(d.dominant, 'sil');
  assert.equal(d.weights[0], 1);
  const f = analyse(new Float32Array(FRAME * 5));
  assert.equal(f.dominant, 'sil');
  assert.ok(f.mouthOpen < 0.01);
  assert.equal(f.sequence, 5);
  assert.equal(analyse(noise({ v: 7 }, FRAME * 5, 0.001)).dominant, 'sil');
});

test('vowels land on their formant buckets regardless of pitch; open vowels open the mouth wider', () => {
  for (const [expect, f1, f2] of [
    ['aa', 750, 1250],
    ['E', 520, 1900],
    ['ih', 330, 2350],
    ['oh', 520, 900],
    ['ou', 330, 780],
  ]) {
    const f = analyse(vowel(130, f1, f2, 12));
    assert.equal(f.dominant, expect, `${f1}/${f2} Hz → ${f.weights.map((w) => w.toFixed(2))}`);
    assert.ok(f.confidence > 0.2, `${expect} confidence ${f.confidence}`);
    assert.ok(f.mouthOpen > 0.15, `${expect} mouth ${f.mouthOpen}`);
    const sum = f.weights.reduce((s, w) => s + w, 0);
    assert.ok(Math.abs(sum - 1) < 0.05, `weights sum ${sum}`);
    assert.equal(f.weights.length, VISEMES.length);
  }
  assert.equal(analyse(vowel(220, 750, 1250, 12)).dominant, 'aa');
  const aa = analyse(vowel(130, 750, 1250, 12)).mouthOpen;
  const ou = analyse(vowel(130, 330, 780, 12)).mouthOpen;
  assert.ok(aa > ou * 1.5, `aa ${aa} vs ou ${ou}`);
});

test('fricatives and hums have their own buckets', () => {
  const seed = { v: 1 };
  const bright = filtered(noise(seed, FRAME * 6, 0.2), new BiquadStage('highpass', 4000, 0.707, SR));
  const ss = analyse(bright);
  assert.equal(ss.dominant, 'SS', `${ss.weights}`);
  const dull = filtered(
    noise(seed, FRAME * 6, 0.05),
    new BiquadStage('lowpass', 3000, 0.707, SR),
    new BiquadStage('highpass', 1500, 0.707, SR),
  );
  const ff = analyse(dull);
  assert.ok(ff.dominant === 'FF' || ff.dominant === 'SS', `${ff.weights}`);
  assert.ok(ff.weights[VISEMES.indexOf('FF')] > 0.3, `${ff.weights}`);
  const hum = Float32Array.from({ length: FRAME * 6 }, (_, n) => Math.sin((2 * Math.PI * 140 * n) / SR) * 0.05);
  const pp = analyse(hum);
  assert.equal(pp.dominant, 'PP', `${pp.weights}`);
  assert.ok(pp.mouthOpen < 0.05);
});

test('weights smooth, release to silence, reset clears, stereo input is downmixed', () => {
  const a = new VisemeAnalyzer(SR);
  const aa = vowel(130, 750, 1250, 6);
  for (let off = 0; off < aa.length; off += FRAME) a.push(aa.subarray(off, off + FRAME), 1);
  const open = a.frame().mouthOpen;
  assert.ok(open > 0.15);
  a.push(new Float32Array(FRAME), 1);
  const after = a.frame();
  assert.ok(after.mouthOpen < open && after.mouthOpen > 0, 'one silent frame releases, not snaps');
  assert.ok(after.weights[0] > 0.2 && after.weights[0] < 0.9, `silence weight ramps: ${after.weights[0]}`);
  for (let i = 0; i < 12; i++) a.push(new Float32Array(FRAME), 1);
  assert.equal(a.frame().dominant, 'sil');
  assert.ok(a.frame().mouthOpen < 0.01);
  assert.equal(a.frame().sequence, 19);

  for (let off = 0; off < aa.length; off += FRAME) a.push(aa.subarray(off, off + FRAME), 1);
  a.reset();
  const r = a.frame();
  assert.equal(r.dominant, 'sil');
  assert.equal(r.mouthOpen, 0);
  assert.equal(r.sequence, 25, 'the counter survives reset: "unchanged = no new audio" keeps holding');

  // Interleaved stereo with the voice on the left only: still a vowel (downmix, not channel 0 only).
  const stereo = new Float32Array(aa.length * 2);
  for (let i = 0; i < aa.length; i++) stereo[2 * i + 1] = aa[i] * 2;
  assert.equal(analyse(stereo, 2).dominant, 'aa');

  // Other sample rates: frame length follows, classification holds.
  const a44 = new VisemeAnalyzer(44100);
  assert.equal(a44.frameSamples, 882);
});

test('the serialised worklet module registers a sink that frames 128-sample quanta into 20 ms analyses', () => {
  const registered = new Map();
  const scope = {
    sampleRate: SR,
    AudioWorkletProcessor: FakeAudioWorkletProcessor,
    registerProcessor: (name, cls) => registered.set(name, cls),
    Float32Array,
    Uint32Array,
    Math,
    Array,
    Object,
    Number,
  };
  scope.globalThis = scope;
  vm.runInNewContext(visemeWorkletSource(), scope);
  const Processor = registered.get('aurix-visemes');
  assert.ok(Processor, 'processor registered');
  const p = new Processor();
  const frames = [];
  p.port.postMessage = (m) => frames.push(m);

  const aa = vowel(130, 750, 1250, 12);
  for (let off = 0; off < aa.length; off += 128) {
    const block = aa.subarray(off, Math.min(off + 128, aa.length));
    // Stereo input with a silent right channel: downmixed, still classified.
    assert.equal(p.process([[block, new Float32Array(block.length)]], [], {}), true);
  }
  assert.equal(frames.length, Math.floor(aa.length / FRAME), 'one frame per 20 ms');
  assert.ok(frames.every((f) => f.type === 'frame'));
  const last = frames.at(-1).frame;
  assert.equal(last.dominant, 'aa');
  assert.equal(last.sequence, frames.length);
  assert.ok(last.mouthOpen > 0.15);

  // Reset from the main thread clears smoothing and the partial buffer.
  p.port.onmessage({ data: { type: 'reset' } });
  frames.length = 0;
  p.process([[new Float32Array(FRAME)]], [], {});
  assert.equal(frames.length, 1);
  assert.equal(frames[0].frame.dominant, 'sil');
  assert.equal(frames[0].frame.sequence, last.sequence + 1);

  // No input yet (track not connected): stays alive, emits nothing.
  assert.equal(p.process([[]], [], {}), true);
});
