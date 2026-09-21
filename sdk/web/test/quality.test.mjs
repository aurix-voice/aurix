import { test } from 'node:test';
import assert from 'node:assert/strict';
import {
  LossWindow,
  RttTracker,
  assembleClientStats,
  barsFromR,
  lossPercent,
  mosFromR,
  networkQualityFromWire,
  rFactor,
} from '../dist/quality.js';

test('bars follow network conditions (same bands as the server)', () => {
  assert.equal(barsFromR(rFactor(20, 2, 0)), 5);
  assert.equal(barsFromR(rFactor(150, 20, 3)), 4);
  assert.equal(barsFromR(rFactor(250, 30, 3)), 3);
  assert.equal(barsFromR(rFactor(300, 40, 6)), 2);
  assert.equal(barsFromR(rFactor(500, 80, 20)), 1);
  assert.equal(barsFromR(rFactor(NaN, Infinity, NaN)), 1);
  assert.ok(mosFromR(93) > 4.3 && mosFromR(0) === 1);
  assert.ok(rFactor(0, 0, 0) <= 100 && rFactor(0, 0, 0) > 92);
});

test('loss is a percentage of expected packets', () => {
  assert.equal(lossPercent(0, 0), 0);
  assert.equal(lossPercent(0, 100), 0);
  assert.equal(lossPercent(5, 95), 5);
  assert.equal(lossPercent(10, 0), 100);
});

test('loss window reports the period, not the lifetime', () => {
  const w = new LossWindow();
  assert.equal(w.advance(0, 100), 0);
  assert.equal(w.advance(10, 190), 10);
  assert.equal(w.advance(10, 290), 0);
  // counters reset (new peer connection) must not go negative
  assert.equal(w.advance(0, 50), 0);
  w.reset();
  assert.equal(w.lossPercent, 0);
});

test('rtt tracker keeps min/avg/max', () => {
  const t = new RttTracker();
  t.record(30);
  t.record(50);
  t.record(10);
  t.record(NaN);
  t.record(-1);
  assert.equal(t.last, 10);
  assert.equal(t.min, 10);
  assert.equal(t.max, 50);
  assert.equal(t.avg, 30);
  assert.equal(t.samples, 3);
});

test('assembleClientStats normalizes WebRTC units', () => {
  const window = new LossWindow();
  const rtt = { last: 40, min: 20, avg: 35, max: 60 };
  const first = assembleClientStats(
    rtt,
    {
      inbound: {
        jitter: 0.004,
        packetsReceived: 1000,
        packetsLost: 0,
        bytesReceived: 80_000,
        concealedSamples: 960,
        packetsDiscarded: 2,
        jitterBufferDelay: 6,
        jitterBufferEmittedCount: 100,
      },
      outbound: { packetsSent: 900, bytesSent: 72_000 },
      remoteInbound: { jitter: 0.002, fractionLost: 0.02 },
      iceRttSeconds: 0.03,
    },
    window,
    undefined,
  );
  assert.equal(first.jitterMs, 4);
  assert.equal(first.iceRttMs, 30);
  assert.equal(first.jitterBufferDelayMs, 60);
  assert.equal(first.remoteJitterMs, 2);
  assert.equal(first.remoteLossPercent, 2);
  assert.equal(first.lossPercent, 0);
  assert.equal(first.packetsDiscarded, 2);
  assert.equal(first.concealedSamples, 960);
  assert.equal(first.bars, 5);
  assert.equal(first.server, undefined);

  // next period: 50 lost out of 500 expected → 10 %
  const second = assembleClientStats(
    rtt,
    { inbound: { jitter: 0.01, packetsReceived: 1450, packetsLost: 50 } },
    window,
    undefined,
  );
  assert.equal(second.lossPercent, 10);
  assert.equal(second.packetsLost, 50);
  assert.ok(second.bars < first.bars);
  // remote loss falls back to cumulative counters when fractionLost is absent
  const third = assembleClientStats(
    rtt,
    { outbound: { packetsSent: 1000 }, remoteInbound: { packetsLost: 100 } },
    new LossWindow(),
    undefined,
  );
  assert.ok(Math.abs(third.remoteLossPercent - 100 / 11) < 1e-9);
});

test('without media the snapshot rates the application RTT alone', () => {
  const s = assembleClientStats({ last: 0, min: 0, avg: 0, max: 0 }, {}, new LossWindow(), undefined);
  assert.equal(s.bars, 5);
  assert.equal(s.packetsReceived, 0);
  const slow = assembleClientStats({ last: 600, min: 600, avg: 600, max: 600 }, {}, new LossWindow(), undefined);
  assert.equal(slow.bars, 1);
});

test('server NetworkQuality is mapped from the wire shape', () => {
  const q = networkQualityFromWire({
    bars: 4,
    r_factor: 75.5,
    mos: 3.9,
    rtt_ms: 80,
    downlink_jitter_ms: 5,
    downlink_loss_percent: 1,
    uplink_jitter_ms: 3,
    uplink_loss_percent: 0.5,
    uplink_bitrate_kbps: 32,
    uplink_packets_received: 5000,
    uplink_packets_lost: 25,
  });
  assert.deepEqual(q, {
    bars: 4,
    rFactor: 75.5,
    mos: 3.9,
    rttMs: 80,
    downlinkJitterMs: 5,
    downlinkLossPercent: 1,
    uplinkJitterMs: 3,
    uplinkLossPercent: 0.5,
    receiversLossPercent: 0,
    uplinkBitrateKbps: 32,
    uplinkPacketsReceived: 5000,
    uplinkPacketsLost: 25,
  });
  const s = assembleClientStats({ last: 10, min: 10, avg: 10, max: 10 }, {}, new LossWindow(), q);
  assert.equal(s.server?.bars, 4);
  const withReceivers = networkQualityFromWire({
    bars: 3,
    r_factor: 70,
    mos: 3.6,
    rtt_ms: 80,
    downlink_jitter_ms: 5,
    downlink_loss_percent: 1,
    uplink_jitter_ms: 3,
    uplink_loss_percent: 0.5,
    receivers_loss_percent: 12.5,
    uplink_bitrate_kbps: 32,
    uplink_packets_received: 5000,
    uplink_packets_lost: 25,
  });
  assert.equal(withReceivers.receiversLossPercent, 12.5);
});
