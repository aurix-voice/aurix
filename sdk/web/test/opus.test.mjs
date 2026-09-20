import { test } from 'node:test';
import assert from 'node:assert/strict';
import {
  DEFAULT_AUDIO_POLICY,
  applyOpusSenderPreferences,
  mergeAllAudioPolicies,
  mergeAudioPolicies,
  negotiatedOpusPreferences,
  opusPlaybackRateHz,
  parseAudioPolicy,
  resolveOpusSenderPreferences,
} from '../dist/opus.js';

// Pinned in aurix-common's `audio_policy_wire_shape` test.
const WIRE = {
  bitrate_bps: 24000,
  min_bitrate_bps: 8000,
  fec: true,
  dtx: false,
  max_bandwidth: 'wideband',
  complexity: 5,
  signal: 'voice',
  stereo: false,
};

test('policy parses the server wire shape and fills defaults', () => {
  const p = parseAudioPolicy(WIRE);
  assert.deepEqual(p, {
    bitrateBps: 24000,
    minBitrateBps: 8000,
    fec: true,
    dtx: false,
    maxBandwidth: 'wideband',
    complexity: 5,
    signal: 'voice',
    stereo: false,
  });
  assert.equal(parseAudioPolicy({ stereo: true }).stereo, true);
  const sparse = parseAudioPolicy({ bitrate_bps: 16000, signal: 'music', complexity: null });
  assert.equal(sparse.bitrateBps, 16000);
  assert.equal(sparse.signal, 'music');
  assert.equal(sparse.complexity, undefined);
  assert.equal(sparse.maxBandwidth, 'fullband');
  assert.equal(sparse.dtx, true);
  assert.deepEqual(parseAudioPolicy(undefined), DEFAULT_AUDIO_POLICY);
  // garbage is ignored, not thrown
  assert.equal(parseAudioPolicy({ max_bandwidth: 'ultra', signal: 42 }).maxBandwidth, 'fullband');
  assert.equal(opusPlaybackRateHz('superwideband'), 24000);
});

test('merge matches the native/Unity semantics', () => {
  const quiet = { bitrateBps: 16000, minBitrateBps: 8000, fec: false, dtx: true, maxBandwidth: 'wideband', complexity: 3, signal: 'voice', stereo: false };
  const music = { bitrateBps: 96000, minBitrateBps: 32000, fec: true, dtx: false, maxBandwidth: 'fullband', signal: 'music', stereo: true };
  const m = mergeAudioPolicies(quiet, music);
  assert.deepEqual(m, { bitrateBps: 96000, minBitrateBps: 32000, fec: true, dtx: false, maxBandwidth: 'fullband', signal: 'music', stereo: true, complexity: 3 });
  assert.deepEqual(mergeAudioPolicies(music, quiet), m);
  assert.deepEqual(mergeAllAudioPolicies([]), DEFAULT_AUDIO_POLICY);
  assert.deepEqual(mergeAllAudioPolicies([quiet]), quiet);
  assert.equal(mergeAudioPolicies({ ...quiet, complexity: 3 }, { ...music, complexity: 8 }).complexity, 8);
});

test('sender preferences: options pin, policy fills, bitrate command caps', () => {
  const policy = parseAudioPolicy(WIRE);
  assert.deepEqual(resolveOpusSenderPreferences(undefined, policy), {
    maxBitrateBps: 24000,
    fec: true,
    dtx: false,
    maxPlaybackRateHz: 16000,
  });
  // explicit options win over the policy; cbr is local only
  assert.deepEqual(resolveOpusSenderPreferences({ maxBitrateBps: 12000, dtx: true, cbr: true }, policy), {
    maxBitrateBps: 12000,
    fec: true,
    dtx: true,
    maxPlaybackRateHz: 16000,
    cbr: true,
  });
  // the server's transient command lowers the ceiling but never raises it above the policy
  assert.equal(resolveOpusSenderPreferences(undefined, policy, 16000).maxBitrateBps, 16000);
  assert.equal(resolveOpusSenderPreferences(undefined, policy, 64000).maxBitrateBps, 24000);
  assert.equal(resolveOpusSenderPreferences(undefined, undefined, 1000).maxBitrateBps, 6000);
  // not following the policy: only the explicit options remain
  assert.deepEqual(resolveOpusSenderPreferences({ followChannelPolicy: false, fec: false }, policy), { fec: false });
  assert.deepEqual(resolveOpusSenderPreferences({ followChannelPolicy: false }, policy, 16000), { maxBitrateBps: 16000 });
  assert.deepEqual(resolveOpusSenderPreferences(undefined, undefined), {});
  // stereo uplink: opt-in, and only where the channel policy allows it
  assert.equal(resolveOpusSenderPreferences({ stereo: true }, policy).stereo, false);
  assert.equal(resolveOpusSenderPreferences({ stereo: true }, undefined).stereo, false);
  assert.equal(resolveOpusSenderPreferences({ stereo: true }, { ...policy, stereo: true }).stereo, true);
  assert.equal(resolveOpusSenderPreferences({ stereo: true, followChannelPolicy: false }, policy).stereo, true);
  assert.equal(resolveOpusSenderPreferences({ stereo: false }, { ...policy, stereo: true }).stereo, false);
  assert.equal(resolveOpusSenderPreferences(undefined, { ...policy, stereo: true }).stereo, undefined);
});

const ANSWER = [
  'v=0',
  'o=- 1 1 IN IP4 0.0.0.0',
  's=-',
  't=0 0',
  'm=audio 9 UDP/TLS/RTP/SAVPF 111 63',
  'a=rtpmap:111 opus/48000/2',
  'a=fmtp:111 minptime=10;useinbandfec=1;sprop-stereo=1',
  'a=rtpmap:63 red/48000/2',
  'a=fmtp:63 111/111',
].join('\r\n');

test('answer fmtp is rewritten for the Opus payload only, preserving other parameters', () => {
  const out = applyOpusSenderPreferences(ANSWER, {
    maxBitrateBps: 24000,
    fec: false,
    dtx: true,
    maxPlaybackRateHz: 16000,
    cbr: true,
  });
  const lines = out.split('\r\n');
  const fmtp = lines.find((l) => l.startsWith('a=fmtp:111 '));
  assert.equal(
    fmtp,
    'a=fmtp:111 minptime=10;sprop-stereo=1;useinbandfec=0;usedtx=1;maxaveragebitrate=24000;maxplaybackrate=16000;cbr=1',
  );
  assert.ok(lines.includes('a=fmtp:63 111/111'));
  assert.ok(out.includes('\r\n') && !out.includes('\n\n'));
  assert.deepEqual(negotiatedOpusPreferences(out), {
    fec: false,
    dtx: true,
    maxBitrateBps: 24000,
    maxPlaybackRateHz: 16000,
    cbr: true,
  });
  // nothing requested → untouched; no Opus → untouched
  assert.equal(applyOpusSenderPreferences(ANSWER, {}), ANSWER);
  assert.equal(applyOpusSenderPreferences('m=audio 9 RTP/AVP 0\na=rtpmap:0 PCMU/8000', { fec: true }), 'm=audio 9 RTP/AVP 0\na=rtpmap:0 PCMU/8000');
  // an Opus payload without any fmtp line gets one right after its rtpmap
  const bare = 'm=audio 9 UDP/TLS/RTP/SAVPF 111\na=rtpmap:111 opus/48000/2\na=sendrecv';
  assert.equal(
    applyOpusSenderPreferences(bare, { dtx: false }),
    'm=audio 9 UDP/TLS/RTP/SAVPF 111\na=rtpmap:111 opus/48000/2\na=fmtp:111 usedtx=0\na=sendrecv',
  );
  assert.deepEqual(negotiatedOpusPreferences(ANSWER), { fec: true });
  const stereo = applyOpusSenderPreferences(ANSWER, { stereo: true });
  assert.match(stereo, /a=fmtp:111 minptime=10;useinbandfec=1;sprop-stereo=1;stereo=1/);
  assert.equal(negotiatedOpusPreferences(stereo).stereo, true);
  assert.equal(negotiatedOpusPreferences(applyOpusSenderPreferences(stereo, { stereo: false })).stereo, false);
  assert.deepEqual(negotiatedOpusPreferences(undefined), {});
});
