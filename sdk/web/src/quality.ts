/**
 * Network-quality model shared with the server and the native SDKs (`aurix_common::types::quality`):
 * a simplified E-model rating `R`, a MOS estimate and 1–5 bars. Loss is always a
 * percentage (`0..=100`).
 */

/**
 * Transmission rating `R` in `0..=100`. Jitter counts double because the jitter buffer turns it
 * into delay; each percent of loss costs 2.5 points. Non-finite inputs are treated as
 * "unknown" (0 for delay, 100 for loss).
 */
export function rFactor(rttMs: number, jitterMs: number, lossPercent: number): number {
  const rtt = Number.isFinite(rttMs) ? Math.max(0, rttMs) : 0;
  const jitter = Number.isFinite(jitterMs) ? Math.max(0, jitterMs) : 0;
  const loss = Number.isFinite(lossPercent) ? clamp(lossPercent, 0, 100) : 100;
  const effectiveLatency = rtt + jitter * 2 + 10;
  const r = effectiveLatency < 160 ? 93.2 - effectiveLatency / 40 : 93.2 - (effectiveLatency - 120) / 10;
  return clamp(r - loss * 2.5, 0, 100);
}

/** Mean opinion score `1.0..=4.5` for a rating `R`. */
export function mosFromR(r: number): number {
  const x = clamp(r, 0, 100);
  return 1 + 0.035 * x + x * (x - 60) * (100 - x) * 7e-6;
}

/** `R ≥ 80` → 5 bars, `≥ 70` → 4, `≥ 60` → 3, `≥ 50` → 2, else 1. */
export function barsFromR(r: number): 1 | 2 | 3 | 4 | 5 {
  if (r >= 80) return 5;
  if (r >= 70) return 4;
  if (r >= 60) return 3;
  if (r >= 50) return 2;
  return 1;
}

/** Loss percentage from cumulative counters; `0` when nothing was expected. */
export function lossPercent(lost: number, received: number): number {
  const expected = lost + received;
  if (!(expected > 0)) return 0;
  return clamp((lost * 100) / expected, 0, 100);
}

function clamp(v: number, lo: number, hi: number): number {
  return Math.min(hi, Math.max(lo, v));
}

/** Server-side view of the connection (`NetworkQuality` control message). */
export interface NetworkQuality {
  /** 1 (unusable) … 5 (excellent), the worse of the two directions. */
  bars: number;
  rFactor: number;
  mos: number;
  /** Round trip as measured by this client and reported to the server. */
  rttMs: number;
  downlinkJitterMs: number;
  downlinkLossPercent: number;
  /** Inter-arrival jitter of this client's audio at the server (RFC 3550). */
  uplinkJitterMs: number;
  /** Sequence gaps in this client's packets over the last report interval. */
  uplinkLossPercent: number;
  uplinkBitrateKbps: number;
  uplinkPacketsReceived: number;
  uplinkPacketsLost: number;
}

export interface NetworkQualityWire {
  bars: number;
  r_factor: number;
  mos: number;
  rtt_ms: number;
  downlink_jitter_ms: number;
  downlink_loss_percent: number;
  uplink_jitter_ms: number;
  uplink_loss_percent: number;
  uplink_bitrate_kbps: number;
  uplink_packets_received: number;
  uplink_packets_lost: number;
}

export function networkQualityFromWire(w: NetworkQualityWire): NetworkQuality {
  return {
    bars: w.bars,
    rFactor: w.r_factor,
    mos: w.mos,
    rttMs: w.rtt_ms,
    downlinkJitterMs: w.downlink_jitter_ms,
    downlinkLossPercent: w.downlink_loss_percent,
    uplinkJitterMs: w.uplink_jitter_ms,
    uplinkLossPercent: w.uplink_loss_percent,
    uplinkBitrateKbps: w.uplink_bitrate_kbps,
    uplinkPacketsReceived: w.uplink_packets_received,
    uplinkPacketsLost: w.uplink_packets_lost,
  };
}

/**
 * One client-side statistics snapshot. Counters are cumulative for the peer connection;
 * `lossPercent`, `rFactor`, `mos` and `bars` describe the last `getStats()` period.
 */
export interface ClientStats {
  /** Application-level round trip from `Ping`/`Pong` (ms), with session min/avg/max. */
  rttMs: number;
  rttMinMs: number;
  rttAvgMs: number;
  rttMaxMs: number;
  /** ICE round trip from `candidate-pair.currentRoundTripTime` (ms), 0 if unknown. */
  iceRttMs: number;
  /** Inbound (downlink) audio, RFC 3550 jitter in ms. */
  jitterMs: number;
  /** Downlink loss over the last period, `0..=100`. */
  lossPercent: number;
  packetsReceived: number;
  packetsLost: number;
  bytesReceived: number;
  /** Frames the decoder concealed (PLC) and packets discarded by the jitter buffer. */
  concealedSamples: number;
  packetsDiscarded: number;
  /** Jitter buffer target/actual delay in ms (browser-reported averages). */
  jitterBufferDelayMs: number;
  /** Outbound (uplink) audio. */
  packetsSent: number;
  bytesSent: number;
  /** Uplink loss as reported by the SFU's receiver reports, `0..=100`. */
  remoteLossPercent: number;
  remoteJitterMs: number;
  /** Client-measured downlink quality. */
  rFactor: number;
  mos: number;
  bars: 1 | 2 | 3 | 4 | 5;
  /** Latest server-side quality (both directions), when received. */
  server?: NetworkQuality;
}

/** Per-period loss from cumulative `packetsLost`/`packetsReceived` counters. */
export class LossWindow {
  private prevLost = 0;
  private prevReceived = 0;
  /** Loss of the last period, `0..=100`. */
  lossPercent = 0;

  /** Advance with the current cumulative counters; returns the period loss. */
  advance(lost: number, received: number): number {
    const dLost = Math.max(0, lost - this.prevLost);
    const dRecv = Math.max(0, received - this.prevReceived);
    this.prevLost = lost;
    this.prevReceived = received;
    this.lossPercent = lossPercent(dLost, dRecv);
    return this.lossPercent;
  }

  reset(): void {
    this.prevLost = 0;
    this.prevReceived = 0;
    this.lossPercent = 0;
  }
}

/**
 * The subset of `RTCStatsReport` entries the snapshot is built from, as plain structural
 * types so the assembly can run (and be tested) outside a browser.
 */
export interface RtcStatsInput {
  inbound?: {
    jitter?: number;
    packetsReceived?: number;
    packetsLost?: number;
    bytesReceived?: number;
    concealedSamples?: number;
    packetsDiscarded?: number;
    jitterBufferDelay?: number;
    jitterBufferEmittedCount?: number;
  };
  outbound?: { packetsSent?: number; bytesSent?: number };
  remoteInbound?: { jitter?: number; packetsLost?: number; fractionLost?: number };
  /** `currentRoundTripTime` (seconds) of the selected ICE candidate pair. */
  iceRttSeconds?: number;
}

export interface RttSnapshot {
  last: number;
  min: number;
  avg: number;
  max: number;
}

/**
 * Build a `ClientStats` snapshot from WebRTC stats. `window` is advanced with the cumulative
 * inbound counters, so the loss percentage covers the period since the previous call. The
 * rating uses the application RTT when known, otherwise the ICE RTT.
 */
export function assembleClientStats(
  rtt: RttSnapshot,
  input: RtcStatsInput,
  window: LossWindow,
  server: NetworkQuality | undefined,
): ClientStats {
  const out: ClientStats = {
    rttMs: rtt.last,
    rttMinMs: rtt.min,
    rttAvgMs: rtt.avg,
    rttMaxMs: rtt.max,
    iceRttMs: input.iceRttSeconds !== undefined && Number.isFinite(input.iceRttSeconds) ? input.iceRttSeconds * 1000 : 0,
    jitterMs: 0,
    lossPercent: window.lossPercent,
    packetsReceived: 0,
    packetsLost: 0,
    bytesReceived: 0,
    concealedSamples: 0,
    packetsDiscarded: 0,
    jitterBufferDelayMs: 0,
    packetsSent: 0,
    bytesSent: 0,
    remoteLossPercent: 0,
    remoteJitterMs: 0,
    rFactor: 0,
    mos: 0,
    bars: 1,
    ...(server ? { server } : {}),
  };
  const i = input.inbound;
  if (i) {
    out.jitterMs = (i.jitter ?? 0) * 1000;
    out.packetsReceived = i.packetsReceived ?? 0;
    out.packetsLost = Math.max(0, i.packetsLost ?? 0);
    out.bytesReceived = i.bytesReceived ?? 0;
    out.concealedSamples = i.concealedSamples ?? 0;
    out.packetsDiscarded = i.packetsDiscarded ?? 0;
    const emitted = i.jitterBufferEmittedCount ?? 0;
    out.jitterBufferDelayMs = emitted > 0 ? ((i.jitterBufferDelay ?? 0) * 1000) / emitted : 0;
    out.lossPercent = window.advance(out.packetsLost, out.packetsReceived);
  }
  if (input.outbound) {
    out.packetsSent = input.outbound.packetsSent ?? 0;
    out.bytesSent = input.outbound.bytesSent ?? 0;
  }
  const r = input.remoteInbound;
  if (r) {
    out.remoteJitterMs = (r.jitter ?? 0) * 1000;
    out.remoteLossPercent =
      r.fractionLost !== undefined && Number.isFinite(r.fractionLost)
        ? clamp(r.fractionLost * 100, 0, 100)
        : lossPercent(Math.max(0, r.packetsLost ?? 0), out.packetsSent);
  }
  const rating = rFactor(out.rttMs > 0 ? out.rttMs : out.iceRttMs, out.jitterMs, out.lossPercent);
  out.rFactor = rating;
  out.mos = mosFromR(rating);
  out.bars = barsFromR(rating);
  return out;
}

/** Running min/avg/max of RTT samples over a session. */
export class RttTracker {
  last = 0;
  min = 0;
  max = 0;
  avg = 0;
  samples = 0;

  record(rttMs: number): void {
    if (!Number.isFinite(rttMs) || rttMs < 0) return;
    this.last = rttMs;
    if (this.samples === 0) {
      this.min = this.max = this.avg = rttMs;
    } else {
      this.min = Math.min(this.min, rttMs);
      this.max = Math.max(this.max, rttMs);
      this.avg += (rttMs - this.avg) / (this.samples + 1);
    }
    this.samples += 1;
  }

  reset(): void {
    this.last = this.min = this.max = this.avg = 0;
    this.samples = 0;
  }
}
