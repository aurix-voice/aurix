import { test } from 'node:test';
import assert from 'node:assert/strict';
import {
  SpatialRenderer,
  directionFromListener,
  distanceGain,
  pannerPosition,
  renderParams,
} from '../dist/spatial.js';
import { FakeAudioContext } from './helpers/fake-audio.mjs';

const cfg = (extra = {}) => ({
  near_distance: 1,
  far_distance: 50,
  rolloff: 'logarithmic',
  max_radius: 100,
  directional: true,
  coordinate_system: 'left_handed',
  ...extra,
});

const rounded = (x) => Math.round(x) || 0;
const close = (a, b, eps = 1e-6) => assert.ok(Math.abs(a - b) < eps, `${a} ≉ ${b}`);

test('distance rolloff mirrors the server curves', () => {
  const log = cfg();
  assert.equal(distanceGain(log, 0.5), 1);
  assert.equal(distanceGain(log, 1), 1);
  close(distanceGain(log, 4), 0.25);
  assert.equal(distanceGain(log, 50), 0);
  assert.equal(distanceGain(log, 101), 0);
  const lin = cfg({ rolloff: 'linear', near_distance: 10, far_distance: 20 });
  close(distanceGain(lin, 15), 0.5);
  const spline = cfg({ rolloff: 'custom_spline', near_distance: 0, far_distance: 10 });
  close(distanceGain(spline, 5), 1 - 0.125);
  // Beyond far but within max: silent, like the server which skips such receivers.
  assert.equal(distanceGain(cfg({ far_distance: 20 }), 30), 0);
});

test('direction follows the listener frame and the channel handedness', () => {
  const listener = { x: 0, y: 0, z: 0 };
  // Unity-style left-handed: forward +Z, up +Y, right is +X.
  const facingZ = { forward_x: 0, forward_y: 0, forward_z: 1, up_x: 0, up_y: 1, up_z: 0 };
  const right = directionFromListener(listener, facingZ, { x: 5, y: 0, z: 0 }, 'left_handed');
  close(right.azimuth, Math.PI / 2);
  close(right.elevation, 0);
  // Same layout in a right-handed world: +X is the listener's left.
  const left = directionFromListener(listener, facingZ, { x: 5, y: 0, z: 0 }, 'right_handed');
  close(left.azimuth, -Math.PI / 2);
  const ahead = directionFromListener(listener, facingZ, { x: 0, y: 0, z: 3 }, 'left_handed');
  close(ahead.azimuth, 0);
  const behind = directionFromListener(listener, facingZ, { x: 0, y: 0, z: -3 }, 'left_handed');
  close(Math.abs(behind.azimuth), Math.PI);
  const above = directionFromListener(listener, facingZ, { x: 0, y: 2, z: 2 }, 'left_handed');
  close(above.elevation, Math.PI / 4);
  // Up not orthogonal to forward is re-orthogonalised, like the server.
  const tilted = { ...facingZ, up_y: 1, up_z: 0.5 };
  close(directionFromListener(listener, tilted, { x: 0, y: 0, z: 3 }, 'left_handed').elevation, 0);
  // Degenerate cases.
  assert.equal(directionFromListener(listener, facingZ, listener, 'left_handed'), undefined);
  assert.equal(
    directionFromListener(listener, { ...facingZ, forward_z: 0 }, { x: 1, y: 0, z: 0 }, 'left_handed'),
    undefined,
  );
  // Web Audio frame: right → +X, ahead → -Z, above → +Y.
  assert.deepEqual(pannerPosition({ azimuth: Math.PI / 2, elevation: 0 }).map(rounded), [1, 0, 0]);
  assert.deepEqual(pannerPosition({ azimuth: 0, elevation: 0 }).map(rounded), [0, 0, -1]);
  assert.deepEqual(pannerPosition({ azimuth: 0, elevation: Math.PI / 2 }).map(rounded), [0, 1, 0]);
});

test('render parameters combine volume, mute, focus and position like the server mix', () => {
  assert.deepEqual(renderParams({ volume: 1, silenced: true, focusFactor: 1 }), { gain: 0 });
  assert.deepEqual(renderParams({ volume: 0, silenced: false, focusFactor: 1 }), { gain: 0 });
  assert.deepEqual(renderParams({ volume: 2, silenced: false, focusFactor: 0.5 }), { gain: 1 });
  const facingZ = { forward_x: 0, forward_y: 0, forward_z: 1, up_x: 0, up_y: 1, up_z: 0 };
  const spatial = renderParams({
    volume: 1,
    silenced: false,
    focusFactor: 1,
    positional: { config: cfg(), listener: { x: 0, y: 0, z: 0 }, orientation: facingZ, source: { x: 4, y: 0, z: 0 } },
  });
  close(spatial.gain, 0.25);
  close(spatial.direction.azimuth, Math.PI / 2);
  // Out of range: inaudible (the server would not even forward the frame).
  const far = renderParams({
    volume: 1,
    silenced: false,
    focusFactor: 1,
    positional: { config: cfg(), listener: { x: 0, y: 0, z: 0 }, orientation: facingZ, source: { x: 60, y: 0, z: 0 } },
  });
  assert.deepEqual(far, { gain: 0 });
  // Non-directional channel: attenuation only, no panner.
  const flat = renderParams({
    volume: 1,
    silenced: false,
    focusFactor: 1,
    positional: {
      config: cfg({ directional: false }),
      listener: { x: 0, y: 0, z: 0 },
      orientation: facingZ,
      source: { x: 4, y: 0, z: 0 },
    },
  });
  assert.equal(flat.direction, undefined);
  close(flat.gain, 0.25);
  // Co-located speaker: centred but still directional, like `Direction::AHEAD` on the server.
  const onTop = renderParams({
    volume: 1,
    silenced: false,
    focusFactor: 1,
    positional: { config: cfg(), listener: { x: 0, y: 0, z: 0 }, orientation: facingZ, source: { x: 0, y: 0, z: 0 } },
  });
  assert.deepEqual(onTop, { gain: 1, direction: { azimuth: 0, elevation: 0 } });
});

/** Follow the audible path from a track's source to the destination. */
function pathOf(ctx, mid, renderer) {
  const source = ctx.created.find((n) => n.kind === 'source' && n.stream?.mid === mid);
  const kinds = [];
  let cur = source;
  while (cur && cur.outputs.size > 0) {
    assert.equal(cur.outputs.size, 1, `fan-out from ${cur.kind}`);
    cur = [...cur.outputs][0];
    kinds.push(cur.kind);
  }
  void renderer;
  return kinds;
}

test('renderer routes each track through gain and, when directional, an HRTF panner', () => {
  const ctx = new FakeAudioContext();
  const r = new SpatialRenderer(ctx, { document: null });
  r.addTrack('1', { mid: '1' });
  assert.deepEqual(r.mids, ['1']);
  // Idle slot: muted, straight to master.
  assert.deepEqual(pathOf(ctx, '1'), ['gain', 'gain', 'destination']);
  const gain = ctx.created.find((n) => n.kind === 'gain' && n !== ctx.created[1]);
  assert.equal(gain.gain.value, 0);

  r.render('1', { gain: 0.5 });
  assert.equal(gain.gain.value, 0.5);
  assert.equal(r.isSpatial('1'), false);

  r.render('1', { gain: 0.25, direction: { azimuth: Math.PI / 2, elevation: 0 } });
  assert.equal(r.isSpatial('1'), true);
  assert.deepEqual(pathOf(ctx, '1'), ['gain', 'panner', 'gain', 'destination']);
  const panner = ctx.created.find((n) => n.kind === 'panner');
  assert.equal(panner.panningModel, 'HRTF');
  assert.equal(panner.rolloffFactor, 0, 'distance is the gain node’s job');
  close(panner.positionX.value, 1);
  close(panner.positionZ.value, 0);
  assert.equal(gain.gain.value, 0.25);

  // Back to a non-positional participant on the same mid: panner bypassed, not duplicated.
  r.render('1', { gain: 1 });
  assert.equal(r.isSpatial('1'), false);
  assert.deepEqual(pathOf(ctx, '1'), ['gain', 'gain', 'destination']);
  r.render('1', { gain: 1, direction: { azimuth: 0, elevation: 0 } });
  assert.equal(ctx.created.filter((n) => n.kind === 'panner').length, 1, 'panner reused');

  // Silence routes around the panner too (no HRTF work for inaudible voices).
  r.render('1', { gain: 0, direction: { azimuth: 0, elevation: 0 } });
  assert.equal(r.isSpatial('1'), false);

  r.render('missing', { gain: 1 }); // unknown mid is a no-op
  r.removeTrack('1');
  assert.deepEqual(r.mids, []);
  assert.deepEqual(pathOf(ctx, '1'), []);
});

test('renderer master follows output volume, mute, sink and lifecycle', async () => {
  const ctx = new FakeAudioContext();
  const r = new SpatialRenderer(ctx, { panningModel: 'equalpower', document: null });
  const master = ctx.created[1];
  assert.equal(master.kind, 'gain');
  assert.deepEqual([...master.outputs][0], ctx.destination);
  r.setMasterVolume(0.3);
  close(master.gain.value, 0.3);
  r.setMasterMuted(true);
  assert.equal(master.gain.value, 0);
  r.setMasterMuted(false);
  close(master.gain.value, 0.3);
  await r.setSinkId('spk-2');
  assert.equal(ctx.sinkId, 'spk-2');
  await r.setSinkId(undefined);
  assert.equal(ctx.sinkId, '');

  assert.equal(await r.resume(), true);
  assert.equal(ctx.state, 'running');
  r.addTrack('a', { mid: 'a' });
  r.addTrack('b', { mid: 'b' });
  r.render('b', { gain: 1, direction: { azimuth: 0, elevation: 0 } });
  assert.equal(ctx.created.find((n) => n.kind === 'panner').panningModel, 'equalpower');
  r.clear();
  assert.deepEqual(r.mids, []);
  await r.close();
  assert.equal(ctx.closed, true);
  assert.equal(master.outputs.size, 0);
});

test('renderer without setSinkId support ignores device selection', async () => {
  const ctx = new FakeAudioContext();
  ctx.setSinkId = undefined;
  const r = new SpatialRenderer(ctx, { document: null });
  await r.setSinkId('x');
  assert.equal(ctx.sinkId, undefined);
});
