using System;
using System.Collections.Generic;
using Aurix.Protocol;

namespace Aurix
{
    /// <summary>
    /// Network-quality model shared with the server and the other SDKs
    /// (<c>aurix_common::types::quality</c>): a simplified E-model rating <c>R</c>, a MOS estimate and
    /// 1–5 bars. Loss is always a percentage (<c>0..100</c>).
    /// </summary>
    public static class QualityModel
    {
        /// <summary>
        /// Transmission rating <c>R</c> in <c>0..100</c>. Jitter counts double because the jitter buffer
        /// turns it into delay; each percent of loss costs 2.5 points. Non-finite inputs are treated as
        /// unknown (0 for delay, 100 for loss).
        /// </summary>
        public static float RFactor(float rttMs, float jitterMs, float lossPercent)
        {
            float rtt = IsFinite(rttMs) ? Math.Max(0f, rttMs) : 0f;
            float jitter = IsFinite(jitterMs) ? Math.Max(0f, jitterMs) : 0f;
            float loss = IsFinite(lossPercent) ? Clamp(lossPercent, 0f, 100f) : 100f;
            float effectiveLatency = rtt + jitter * 2f + 10f;
            float r = effectiveLatency < 160f ? 93.2f - effectiveLatency / 40f : 93.2f - (effectiveLatency - 120f) / 10f;
            return Clamp(r - loss * 2.5f, 0f, 100f);
        }

        /// <summary>Mean opinion score <c>1.0..4.5</c> for a rating <c>R</c>.</summary>
        public static float MosFromR(float r)
        {
            float x = Clamp(r, 0f, 100f);
            return 1f + 0.035f * x + x * (x - 60f) * (100f - x) * 7e-6f;
        }

        /// <summary><c>R ≥ 80</c> → 5 bars, <c>≥ 70</c> → 4, <c>≥ 60</c> → 3, <c>≥ 50</c> → 2, else 1.</summary>
        public static int BarsFromR(float r)
        {
            if (r >= 80f) return 5;
            if (r >= 70f) return 4;
            if (r >= 60f) return 3;
            if (r >= 50f) return 2;
            return 1;
        }

        /// <summary>Loss percentage from counters; 0 when nothing was expected.</summary>
        public static float LossPercent(long lost, long received)
        {
            long expected = lost + received;
            if (expected <= 0) return 0f;
            return Clamp(lost * 100f / expected, 0f, 100f);
        }

        private static bool IsFinite(float v) => !float.IsNaN(v) && !float.IsInfinity(v);
        private static float Clamp(float v, float lo, float hi) => v < lo ? lo : v > hi ? hi : v;
    }

    /// <summary>Per-period loss from cumulative lost/received counters.</summary>
    public sealed class LossWindow
    {
        private long _prevLost, _prevReceived;

        /// <summary>Loss of the last period, <c>0..100</c>.</summary>
        public float LossPercent { get; private set; }

        /// <summary>Advance with the current cumulative counters; returns the loss of the period since the previous call.</summary>
        public float Advance(long lost, long received)
        {
            long dLost = Math.Max(0, lost - _prevLost);
            long dRecv = Math.Max(0, received - _prevReceived);
            _prevLost = lost;
            _prevReceived = received;
            LossPercent = QualityModel.LossPercent(dLost, dRecv);
            return LossPercent;
        }

        public void Reset()
        {
            _prevLost = _prevReceived = 0;
            LossPercent = 0f;
        }
    }

    /// <summary>
    /// The server's view of this connection (<c>NetworkQuality</c> control message): the downlink as
    /// reported by this client merged with what the SFU measures on the uplink; <see cref="Bars"/> is
    /// the worse of the two directions.
    /// </summary>
    public struct NetworkQuality
    {
        /// <summary>1 (unusable) … 5 (excellent).</summary>
        public int Bars;
        public float RFactor;
        public float Mos;
        /// <summary>Round trip as measured and reported by this client.</summary>
        public float RttMs;
        public float DownlinkJitterMs;
        public float DownlinkLossPercent;
        /// <summary>Inter-arrival jitter of this client's audio at the server (RFC 3550).</summary>
        public float UplinkJitterMs;
        /// <summary>Sequence gaps in this client's packets over the last report interval.</summary>
        public float UplinkLossPercent;
        public uint UplinkBitrateKbps;
        public long UplinkPacketsReceived;
        public long UplinkPacketsLost;

        /// <summary>Typed view of a <c>NetworkQuality</c> payload (<c>data.quality</c>).</summary>
        public static NetworkQuality? FromMessage(ControlMessage m)
        {
            if (m.Data == null || !m.Data.TryGetValue("quality", out var qv)) return null;
            var q = MiniJson.AsObject(qv);
            if (q == null) return null;
            return FromObject(q);
        }

        public static NetworkQuality FromObject(Dictionary<string, object> q) => new NetworkQuality
        {
            Bars = (int)MiniJson.GetNumber(q, "bars"),
            RFactor = (float)MiniJson.GetNumber(q, "r_factor"),
            Mos = (float)MiniJson.GetNumber(q, "mos"),
            RttMs = (float)MiniJson.GetNumber(q, "rtt_ms"),
            DownlinkJitterMs = (float)MiniJson.GetNumber(q, "downlink_jitter_ms"),
            DownlinkLossPercent = (float)MiniJson.GetNumber(q, "downlink_loss_percent"),
            UplinkJitterMs = (float)MiniJson.GetNumber(q, "uplink_jitter_ms"),
            UplinkLossPercent = (float)MiniJson.GetNumber(q, "uplink_loss_percent"),
            UplinkBitrateKbps = (uint)MiniJson.GetNumber(q, "uplink_bitrate_kbps"),
            UplinkPacketsReceived = (long)MiniJson.GetNumber(q, "uplink_packets_received"),
            UplinkPacketsLost = (long)MiniJson.GetNumber(q, "uplink_packets_lost"),
        };
    }

    /// <summary>
    /// One statistics snapshot of an <see cref="AurixVoiceClient"/>. Packet, byte and frame counters
    /// are cumulative for the current media transport; <see cref="LossPercent"/>, <see cref="RFactor"/>,
    /// <see cref="Mos"/> and <see cref="Bars"/> describe the last quality period.
    /// </summary>
    public struct VoiceStats
    {
        public VoiceConnectionState State;
        // Transport (native AURX over UDP).
        public long PacketsSent;
        public long BytesSent;
        public long PacketsReceived;
        public long BytesReceived;
        /// <summary>Downlink packets that failed authentication/decryption.</summary>
        public long BadAuth;
        /// <summary>Downlink packets rejected by the replay window.</summary>
        public long Replayed;
        public long HeartbeatsLost;
        /// <summary>Heartbeat RTT (media path), current and session min/avg/max.</summary>
        public float RttMs;
        public float RttMinMs;
        public float RttAvgMs;
        public float RttMaxMs;
        /// <summary>WebSocket <c>Ping</c>/<c>Pong</c> RTT.</summary>
        public float ControlRttMs;
        /// <summary>RFC 3550 inter-arrival jitter of downlink audio.</summary>
        public float JitterMs;
        /// <summary>Downlink loss over the last period, <c>0..100</c> (frames lost vs. frames received).</summary>
        public float LossPercent;
        // Playout (jitter buffers / mixer), lifetime.
        public long FramesLost;
        public long FramesLate;
        public long Underruns;
        public int ActiveStreams;
        // Derived quality.
        public float RFactor;
        public float Mos;
        /// <summary>1 (unusable) … 5 (excellent).</summary>
        public int Bars;
        /// <summary>Latest server-side quality (both directions), when received.</summary>
        public NetworkQuality? Server;
    }
}
