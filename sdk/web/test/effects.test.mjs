import { test } from 'node:test';
import assert from 'node:assert/strict';
import vm from 'node:vm';
import {
  VOICE_EFFECTS_BYPASS,
  VOICE_EFFECT_PRESETS,
  VoiceEffectChain,
  isVoiceEffectsBypass,
  sanitizeVoiceEffects,
  voiceEffectPreset,
  voiceEffectsWorkletSource,
} from '../dist/effects.js';
import { FakeAudioWorkletProcessor } from './helpers/fake-audio.mjs';

const SR = 48000;
const rms = (a) => Math.sqrt(a.reduce((s, v) => s + v * v, 0) / a.length);
const sine = (hz, n, amp = 0.5, phase = 0) => Float32Array.from({ length: n }, (_, i) => amp * Math.sin(phase + (2 * Math.PI * hz * i) / SR));

/** Push `mono` through a fresh chain in 128-sample quanta (like the worklet) and return the output. */
function run(params, mono, channels = 1) {
  const chain = new VoiceEffectChain(sanitizeVoiceEffects(params), SR);
  const out = Array.from({ length: channels }, () => new Float32Array(mono.length));
  for (let off = 0; off < mono.length; off += 128) {
    const n = Math.min(128, mono.length - off);
    const block = out.map((_, c) => (c === 0 ? mono.slice(off, off + n) : new Float32Array(n)));
    chain.process(block, n);
    block.forEach((b, c) => out[c].set(b, off));
  }
  return out;
}

test('sanitizer clamps into the documented ranges and treats NaN / missing / zero as off', () => {
  assert.deepEqual(sanitizeVoiceEffects(undefined), VOICE_EFFECTS_BYPASS);
  assert.deepEqual(sanitizeVoiceEffects({}), VOICE_EFFECTS_BYPASS);
  const s = sanitizeVoiceEffects({
    highpassHz: 5,
    lowpassHz: 1e9,
    formantSemitones: 40,
    pitchSemitones: -99,
    ringModHz: NaN,
    distortionDrive: 0.2,
    tremoloHz: 100,
    tremoloDepth: 3,
    staticLevel: -1,
    reverbMix: 2,
    reverbSize: Infinity,
    reverbDamping: 0.5,
  });
  assert.equal(s.highpassHz, 20, 'corner floor');
  assert.equal(s.lowpassHz, 20000, 'corner ceiling');
  assert.equal(s.formantSemitones, 12);
  assert.equal(s.pitchSemitones, -24);
  assert.equal(s.ringModHz, 0);
  assert.equal(s.distortionDrive, 1, 'a positive drive below 1 is unity drive, not off');
  assert.equal(s.tremoloHz, 20);
  assert.equal(s.tremoloDepth, 1);
  assert.equal(s.staticLevel, 0);
  assert.equal(s.reverbMix, 1);
  assert.equal(s.reverbSize, 0, 'non-finite → off');
  assert.equal(s.reverbDamping, 0.5);
  assert.equal(sanitizeVoiceEffects({ distortionDrive: 0 }).distortionDrive, 0);
  assert.equal(sanitizeVoiceEffects({ highpassHz: -3 }).highpassHz, 0);
  assert.equal(isVoiceEffectsBypass(undefined), true);
  assert.equal(isVoiceEffectsBypass({ tremoloHz: 5 }), true, 'a rate without depth does nothing');
  assert.equal(isVoiceEffectsBypass({ tremoloHz: 5, tremoloDepth: 0.2 }), false);
  assert.equal(isVoiceEffectsBypass({ reverbMix: 0.3 }), false);
});

test('presets mirror the native EffectPreset tuning and are case-insensitive', () => {
  assert.deepEqual(VOICE_EFFECT_PRESETS, ['robot', 'monster', 'radio', 'helium', 'ghost']);
  assert.deepEqual(voiceEffectPreset('Robot'), { ...VOICE_EFFECTS_BYPASS, highpassHz: 200, lowpassHz: 4000, ringModHz: 60, distortionDrive: 2 });
  assert.deepEqual(voiceEffectPreset(' helium '), { ...VOICE_EFFECTS_BYPASS, formantSemitones: 6, pitchSemitones: 6 });
  assert.equal(voiceEffectPreset('monster').pitchSemitones, -7);
  assert.equal(voiceEffectPreset('radio').staticLevel, 0.03);
  assert.equal(voiceEffectPreset('ghost').reverbSize, 0.9);
  for (const name of VOICE_EFFECT_PRESETS) assert.equal(isVoiceEffectsBypass(voiceEffectPreset(name)), false);
  assert.throws(() => voiceEffectPreset('dalek'), RangeError);
});

test('bypass chain has no stages and leaves audio untouched; a configured chain changes it', () => {
  const chain = new VoiceEffectChain(VOICE_EFFECTS_BYPASS, SR);
  assert.equal(chain.isBypass, true);
  assert.equal(chain.stages.length, 0);
  const input = sine(220, 4096);
  const [same] = run({}, input);
  assert.deepEqual(Array.from(same), Array.from(input));
  for (const name of VOICE_EFFECT_PRESETS) {
    const [out] = run(voiceEffectPreset(name), input);
    assert.ok(out.every((v) => Number.isFinite(v)), `${name}: finite output`);
    let diff = 0;
    for (let i = 0; i < out.length; i++) diff = Math.max(diff, Math.abs(out[i] - input[i]));
    assert.ok(diff > 0.05, `${name} audibly alters the signal (max diff ${diff})`);
    assert.ok(rms(out) > 0.02, `${name} does not silence the voice (rms ${rms(out)})`);
  }
});

test('filters attenuate outside the pass band', () => {
  const [hp] = run({ highpassHz: 1000 }, sine(100, 8192));
  assert.ok(rms(hp.subarray(4096)) < 0.05, `100 Hz through a 1 kHz high-pass: ${rms(hp.subarray(4096))}`);
  const [hpPass] = run({ highpassHz: 100 }, sine(2000, 8192));
  assert.ok(rms(hpPass.subarray(4096)) > 0.3);
  const [lp] = run({ lowpassHz: 500 }, sine(6000, 8192));
  assert.ok(rms(lp.subarray(4096)) < 0.05, `6 kHz through a 500 Hz low-pass: ${rms(lp.subarray(4096))}`);
});

test('pitch shift moves the fundamental by the requested interval', () => {
  const n = 32768;
  const [out] = run({ pitchSemitones: 12 }, sine(200, n));
  // Count zero crossings on the steady tail: 200 Hz → 400 Hz doubles them.
  const crossings = (a) => {
    let c = 0;
    for (let i = 1; i < a.length; i++) if ((a[i - 1] < 0) !== (a[i] < 0)) c++;
    return c;
  };
  const tail = out.subarray(n / 2);
  const hz = (crossings(tail) / 2) * (SR / tail.length);
  assert.ok(Math.abs(hz - 400) < 40, `octave up lands near 400 Hz, got ${hz.toFixed(1)}`);
  const [down] = run({ pitchSemitones: -12 }, sine(400, n));
  const hzDown = (crossings(down.subarray(n / 2)) / 2) * (SR / (n / 2));
  assert.ok(Math.abs(hzDown - 200) < 30, `octave down lands near 200 Hz, got ${hzDown.toFixed(1)}`);
});

test('tremolo modulates the amplitude at the requested rate', () => {
  const n = SR; // one second, 4 Hz tremolo → 4 troughs
  const [out] = run({ tremoloHz: 4, tremoloDepth: 1 }, sine(440, n, 0.5));
  const env = [];
  for (let i = 0; i < n; i += 480) env.push(rms(out.subarray(i, i + 480)));
  const lo = Math.min(...env.slice(5));
  const hi = Math.max(...env.slice(5));
  assert.ok(hi > 0.3 && lo < 0.05, `envelope swings between ${lo.toFixed(3)} and ${hi.toFixed(3)}`);
});

test('distortion saturates, ring modulation and reverb change the spectrum, static adds noise only with voice', () => {
  const [dist] = run({ distortionDrive: 20 }, sine(300, 4096, 0.9));
  assert.ok(Math.max(...dist.map(Math.abs)) <= 1.0001, 'saturated output is bounded');
  assert.ok(rms(dist) > rms(sine(300, 4096, 0.9)) * 0.9, 'drive keeps the level up');

  const [ring] = run({ ringModHz: 500 }, sine(300, 4096));
  assert.ok(rms(ring) > 0.05);
  assert.notDeepEqual(Array.from(ring.subarray(0, 100)), Array.from(sine(300, 4096).subarray(0, 100)));

  const impulse = new Float32Array(SR / 2);
  impulse[0] = 1;
  const [rev] = run({ reverbMix: 0.8, reverbSize: 0.9, reverbDamping: 0.2 }, impulse);
  assert.ok(rms(rev.subarray(SR / 10, SR / 5)) > 1e-4, 'reverb tail rings after the impulse');
  assert.ok(rev.every((v) => Math.abs(v) < 4), 'feedback network is stable');

  const silence = new Float32Array(4096);
  const [quiet] = run({ staticLevel: 0.5 }, silence);
  assert.equal(rms(quiet), 0, 'static is gated: nothing without voice');
  const [noisy] = run({ staticLevel: 0.5 }, sine(300, 4096));
  assert.ok(rms(noisy) > rms(sine(300, 4096)) * 0.9);
});

test('stereo processing keeps channels independent (no bleed) and reset clears state', () => {
  const n = 8192;
  for (const name of VOICE_EFFECT_PRESETS) {
    const chain = new VoiceEffectChain(voiceEffectPreset(name), SR);
    const left = sine(300, n, 0.6);
    const right = new Float32Array(n);
    for (let off = 0; off < n; off += 128) {
      const block = [left.subarray(off, off + 128), right.subarray(off, off + 128)];
      chain.process(block, 128);
    }
    const hiss = voiceEffectPreset(name).staticLevel;
    if (hiss === 0) assert.equal(rms(right), 0, `${name}: a silent right channel stays silent (rms ${rms(right)})`);
    else assert.ok(rms(right) <= hiss, `${name}: the right channel carries only the (mono) hiss, no voice (rms ${rms(right)})`);
    assert.ok(rms(left) > 0.02, `${name}: left carries the voice`);
  }

  // Deterministic stages: the same input after `reset()` yields the same output as a fresh chain.
  const params = { pitchSemitones: 5, reverbMix: 0.5, reverbSize: 0.5, lowpassHz: 3000, tremoloHz: 3, tremoloDepth: 0.4 };
  const input = sine(250, 4096);
  const fresh = new VoiceEffectChain(sanitizeVoiceEffects(params), SR);
  const a = input.slice();
  fresh.process([a], a.length);
  const reused = new VoiceEffectChain(sanitizeVoiceEffects(params), SR);
  const warm = sine(700, 4096);
  reused.process([warm], warm.length);
  reused.reset();
  const b = input.slice();
  reused.process([b], b.length);
  assert.deepEqual(Array.from(b), Array.from(a));
});

test('the serialised worklet module is self-contained: registers the processor and effects audio', async () => {
  const registered = new Map();
  const scope = {
    sampleRate: SR,
    AudioWorkletProcessor: FakeAudioWorkletProcessor,
    registerProcessor: (name, cls) => registered.set(name, cls),
    Float32Array,
    Math,
    Array,
    Object,
    Number,
  };
  scope.globalThis = scope;
  vm.runInNewContext(voiceEffectsWorkletSource(), scope);
  const Processor = registered.get('aurix-voice-effects');
  assert.ok(Processor, 'processor registered');
  const p = new Processor();

  // Without params: pass-through, mono duplicated onto a stereo output.
  const input = sine(300, 128);
  let out = [new Float32Array(128), new Float32Array(128)];
  p.process([[input]], [out]);
  assert.deepEqual(Array.from(out[0]), Array.from(input));
  assert.deepEqual(Array.from(out[1]), Array.from(input));

  // Params arrive through the port: the chain runs.
  p.port.onmessage({ data: { type: 'params', params: voiceEffectPreset('robot') } });
  out = [new Float32Array(128)];
  p.process([[input]], [out]);
  assert.notDeepEqual(Array.from(out[0]), Array.from(input));
  assert.ok(out[0].every(Number.isFinite));

  // Bypass params drop the chain again.
  p.port.onmessage({ data: { type: 'params', params: VOICE_EFFECTS_BYPASS } });
  out = [new Float32Array(128)];
  p.process([[input]], [out]);
  assert.deepEqual(Array.from(out[0]), Array.from(input));

  // Missing input (microphone not yet connected) keeps the processor alive.
  assert.equal(p.process([[]], [[new Float32Array(128)]]), true);
});
