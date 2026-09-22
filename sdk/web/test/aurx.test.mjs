import { test } from 'node:test';
import assert from 'node:assert/strict';
import {
  AURX_HEADER_SIZE,
  AurxFlags,
  AurxKeys,
  AurxPacketType,
  ReplayWindow,
  SequenceLoss,
  audioHeader,
  audioLevelByte,
  channelIdHash,
  decodePacket,
  heartbeatHeader,
  parseDownlinkAudio,
  sessionBindHeader,
  sessionBindPayload,
} from '../dist/aurx.js';

// Vectors produced by the Rust reference (`aurix_common::protocol` with `MediaKeys::derive(&[7u8; 32])`).
const MASTER = new Uint8Array(32).fill(7);
const SSRC = 0x11223344;
const ID = '0192a1b2-c3d4-7e5f-8a9b-0c1d2e3f4a5b';
const V = {
  bind: '41555258023304000000002acfe5687b1122334400000000002075b2926e0192a1b2c3d47e5f8a9b0c1d2e3f4a5b0000018bcfe5687b000000000000002ab20b439b0f57c9928c47a39fd821cf0e',
  audio: '4155525802010c0100000001000003c011223344deadbeef00061c81a8879d0d693a856f8454e9e4da0ad81761922ab6a96e23a0',
  hb: '41555258022004010000000200001388112233440000000000000000000010c18cded68e76bb65aa8d97dde50d27',
  down: '4155525802011481000000070000078055667788deadbeef0006d13270689bda1e726600198b1937f696a2a37221af3b24eaf970',
  ack: '41555258023404010000002acfe568c811223344000000000008645f07538302e45d21afa5d3937ef0dc2c3c8b0c3527f96e0c7fc6a6',
  bc: '41555258027104010000000000000000112233440000000000048edd324bc5e6cb501516d9a8c1ef98bb4f9f310a6edae57e',
};

const hex = (b) => Array.from(b, (x) => x.toString(16).padStart(2, '0')).join('');
const bytes = (h) => Uint8Array.from(h.match(/../g), (x) => parseInt(x, 16));

test('channel id hash matches Rust crc32 of the UUID bytes', () => {
  assert.equal(channelIdHash(ID), 1883494404);
});

test('audio level byte matches Rust encode_audio_level', () => {
  assert.equal(audioLevelByte(0.5), 6);
  assert.equal(audioLevelByte(0.01), 40);
  assert.equal(audioLevelByte(0), 127);
});

test('SessionBind is authenticated (not encrypted) and byte-identical to Rust', async () => {
  const keys = await AurxKeys.derive(MASTER);
  const unixMs = 1_700_000_000_123;
  const pkt = await keys.signPlain(sessionBindHeader(SSRC, unixMs, 42), sessionBindPayload(ID, unixMs, 42));
  assert.equal(hex(pkt), V.bind);
});

test('SessionBind payload keeps the full 64-bit nonce', () => {
  const p = sessionBindPayload(ID, 1_700_000_000_123, 0x1_2345_6789);
  assert.equal(hex(p.subarray(24)), '0000000123456789');
  assert.throws(() => sessionBindPayload(ID, 1, -1), RangeError);
  assert.throws(() => sessionBindPayload(ID, 1, 2 ** 53), RangeError);
});

test('sealed audio with energy byte is byte-identical to Rust', async () => {
  const keys = await AurxKeys.derive(MASTER);
  const payload = new Uint8Array([33, 1, 2, 3, 4, 5]);
  const pkt = await keys.seal(audioHeader(SSRC, 1, 960, 0xdeadbeef, AurxFlags.Energy), payload);
  assert.equal(hex(pkt), V.audio);
});

test('sealed heartbeat is byte-identical to Rust', async () => {
  const keys = await AurxKeys.derive(MASTER);
  const pkt = await keys.seal(heartbeatHeader(SSRC, 2, 5000), new Uint8Array(0));
  assert.equal(hex(pkt), V.hb);
});

test('opens Rust-sealed downlink audio: volume + direction + frame', async () => {
  const keys = await AurxKeys.derive(MASTER);
  const data = bytes(V.down);
  const decoded = decodePacket(data);
  assert.ok(decoded);
  const opened = await keys.open(data, decoded);
  assert.ok(opened);
  const audio = parseDownlinkAudio(opened);
  assert.ok(audio);
  assert.equal(audio.ssrc, 0x55667788);
  assert.equal(audio.sequence, 7);
  assert.equal(audio.timestamp, 1920);
  assert.equal(audio.channelIdHash, 0xdeadbeef);
  assert.equal(audio.gain, 0.5);
  assert.ok(Math.abs(audio.direction.azimuth - Math.PI / 2) < 0.02);
  assert.ok(Math.abs(audio.direction.elevation + Math.PI / 4) < 0.02);
  assert.deepEqual(Array.from(audio.frame), [0xfc, 0xff, 0xfe]);
  assert.equal(audio.e2ee, false);
  assert.equal(audio.mixed, false);
});

test('opens SessionBindAck and BitrateCommand from Rust', async () => {
  const keys = await AurxKeys.derive(MASTER);
  const ack = decodePacket(bytes(V.ack));
  assert.equal(ack.header.packetType, AurxPacketType.SessionBindAck);
  assert.equal(ack.header.sequence, 42);
  assert.ok(await keys.open(bytes(V.ack), ack));
  const bc = decodePacket(bytes(V.bc));
  assert.equal(bc.header.packetType, AurxPacketType.BitrateCommand);
  const opened = await keys.open(bytes(V.bc), bc);
  assert.equal(new DataView(opened.payload.buffer, opened.payload.byteOffset).getUint32(0), 24000);
});

test('rejects tampered, truncated and wrong-key packets', async () => {
  const keys = await AurxKeys.derive(MASTER);
  const other = await AurxKeys.derive(new Uint8Array(32).fill(8));
  const data = bytes(V.audio);
  const decoded = decodePacket(data);
  assert.equal(await other.open(data, decoded), undefined);
  const flipped = data.slice();
  flipped[AURX_HEADER_SIZE] ^= 1;
  const d2 = decodePacket(flipped);
  assert.equal(d2, undefined); // CRC over the ciphertext fails first
  const tag = data.slice();
  tag[tag.length - 1] ^= 1;
  assert.equal(await keys.open(tag, decodePacket(tag)), undefined);
  assert.equal(decodePacket(data.subarray(0, data.length - 1)), undefined);
  assert.equal(decodePacket(new Uint8Array(AURX_HEADER_SIZE - 1)), undefined);
  const bad = data.slice();
  bad[0] = 0x42;
  assert.equal(decodePacket(bad), undefined);
});

test('replay window accepts new, rejects duplicates and too-old, tolerates reorder', () => {
  const w = new ReplayWindow(64);
  assert.equal(w.accept(100), true);
  assert.equal(w.accept(100), false);
  assert.equal(w.accept(99), true);
  assert.equal(w.accept(99), false);
  assert.equal(w.accept(200), true);
  assert.equal(w.accept(200 - 64), false);
  assert.equal(w.accept(200 - 63), true);
  const wrap = new ReplayWindow(64);
  assert.equal(wrap.accept(0xffff_fff0), true);
  assert.equal(wrap.accept(3), true); // 19 ahead across the 32-bit wrap
  assert.equal(wrap.accept(0xffff_fff0), false);
  assert.equal(wrap.accept(0xffff_ffff), true);
  assert.equal(wrap.accept(3 - 64), false);
});

test('sequence loss counts gaps once and ignores reordering within the gap', () => {
  const l = new SequenceLoss();
  for (const s of [10, 11, 13, 14, 12, 18]) l.observe(s);
  assert.equal(l.received, 6);
  assert.equal(l.lost, 3);
  l.observe(15);
  assert.equal(l.lost, 2);
  // Wrap of the 32-bit sequence is a gap of one, a restart of the sender is not loss.
  const w = new SequenceLoss();
  for (const s of [0xffff_fffe, 0xffff_ffff, 1]) w.observe(s);
  assert.equal(w.lost, 1);
  w.observe(500_000);
  assert.equal(w.lost, 0);
  assert.equal(w.received, 4);
});
