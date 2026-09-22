import { test } from 'node:test';
import assert from 'node:assert/strict';
import { AurxFlags, downlinkCodec, decodeG711, G711Upsampler, G711_FRAME_SIZES } from '../dist/index.js';

// Reference code points shared with `aurix_common::g711` (ITU-T G.711 tables).
test('μ-law and A-law reference code points decode to the ITU values', () => {
  assert.equal(decodeG711('pcmu', Uint8Array.from([0xff])), undefined, 'unsupported frame size is rejected');
  const frame = new Uint8Array(160);
  frame.set([0xff, 0x80, 0x00, 0xce]);
  const pcmu = decodeG711('pcmu', frame);
  assert.deepEqual(
    Array.from(pcmu.subarray(0, 4), (x) => Math.round(x * 32768)),
    [0, 32124, -32124, 988],
  );
  frame.set([0xd5, 0x55, 0xaa, 0x2a, 0xfa]);
  const pcma = decodeG711('pcma', frame);
  assert.deepEqual(
    Array.from(pcma.subarray(0, 5), (x) => Math.round(x * 32768)),
    [8, -8, 32256, -32256, 1008],
  );
  for (const n of G711_FRAME_SIZES) assert.equal(decodeG711('pcma', new Uint8Array(n)).length, n);
});

test('the upsampler is continuous across frames and lands on the input samples', () => {
  const up = new G711Upsampler(6);
  const a = up.process(Float32Array.from([0, 0.6]));
  assert.equal(a.length, 12);
  assert.ok(Math.abs(a[5]) < 1e-6, 'sixth output sample is the first input sample');
  assert.ok(Math.abs(a[11] - 0.6) < 1e-6);
  const b = up.process(Float32Array.from([0.0]));
  assert.ok(Math.abs(b[0] - 0.5) < 1e-6, 'ramps from the previous frame, not from zero');
  assert.ok(Math.abs(b[5]) < 1e-6);
});

test('the codec flags are exclusive and PCMA does not overlap an existing flag', () => {
  assert.equal(downlinkCodec(0), 'opus');
  assert.equal(downlinkCodec(AurxFlags.Pcmu | AurxFlags.E2ee), 'pcmu');
  assert.equal(downlinkCodec(AurxFlags.Pcma | AurxFlags.E2ee), 'pcma');
  const bits = Object.values(AurxFlags);
  assert.equal(new Set(bits).size, bits.length, 'every flag has its own bit');
  assert.equal(AurxFlags.Pcma, 0x0010);
});
