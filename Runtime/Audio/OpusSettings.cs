using System;
using System.Collections.Generic;
using Aurix.Protocol;

namespace Aurix.Audio
{
    /// <summary>Encoder bandwidth ceiling (<c>OPUS_SET_MAX_BANDWIDTH</c>). Wire names are snake_case.</summary>
    public enum OpusBandwidth
    {
        Narrowband = 0,
        Mediumband = 1,
        Wideband = 2,
        Superwideband = 3,
        Fullband = 4,
    }

    /// <summary>Content hint for the encoder (<c>OPUS_SET_SIGNAL</c>).</summary>
    public enum OpusSignal
    {
        Auto = 0,
        Voice = 1,
        Music = 2,
    }

    public static class OpusEnums
    {
        public static OpusBandwidth ParseBandwidth(string wire, OpusBandwidth fallback = OpusBandwidth.Fullband)
        {
            switch (wire)
            {
                case "narrowband": return OpusBandwidth.Narrowband;
                case "mediumband": return OpusBandwidth.Mediumband;
                case "wideband": return OpusBandwidth.Wideband;
                case "superwideband": return OpusBandwidth.Superwideband;
                case "fullband": return OpusBandwidth.Fullband;
                default: return fallback;
            }
        }

        public static string ToWire(this OpusBandwidth b)
        {
            switch (b)
            {
                case OpusBandwidth.Narrowband: return "narrowband";
                case OpusBandwidth.Mediumband: return "mediumband";
                case OpusBandwidth.Wideband: return "wideband";
                case OpusBandwidth.Superwideband: return "superwideband";
                default: return "fullband";
            }
        }

        /// <summary>Audio bandwidth in Hz the ceiling corresponds to (Fullband = 20 kHz).</summary>
        public static int PlaybackRateHz(this OpusBandwidth b)
        {
            switch (b)
            {
                case OpusBandwidth.Narrowband: return 8000;
                case OpusBandwidth.Mediumband: return 12000;
                case OpusBandwidth.Wideband: return 16000;
                case OpusBandwidth.Superwideband: return 24000;
                default: return 48000;
            }
        }

        public static OpusSignal ParseSignal(string wire, OpusSignal fallback = OpusSignal.Auto)
        {
            switch (wire)
            {
                case "auto": return OpusSignal.Auto;
                case "voice": return OpusSignal.Voice;
                case "music": return OpusSignal.Music;
                default: return fallback;
            }
        }

        public static string ToWire(this OpusSignal s)
        {
            switch (s)
            {
                case OpusSignal.Voice: return "voice";
                case OpusSignal.Music: return "music";
                default: return "auto";
            }
        }
    }

    /// <summary>
    /// Everything the SDK can ask of an Opus encoder. Mirrors the native core's <c>EncoderSettings</c>;
    /// which fields a codec honours depends on the implementation (see <see cref="IOpusEncoderControls"/>).
    /// </summary>
    public struct OpusEncoderSettings : IEquatable<OpusEncoderSettings>
    {
        public const int MinBitrate = 6000;
        /// <summary>libopus clamps a mono encoder here regardless of what is requested.</summary>
        public const int MaxBitrate = 300000;
        public const int MaxComplexity = 10;

        /// <summary>Target bitrate, bits per second.</summary>
        public int BitrateBps;
        /// <summary>0 (cheapest) .. 10 (best quality).</summary>
        public int Complexity;
        public OpusBandwidth MaxBandwidth;
        public OpusSignal Signal;
        /// <summary>Variable bitrate; false = hard CBR.</summary>
        public bool Vbr;
        /// <summary>Constrained VBR: keeps the short-term rate near the target (ignored under CBR).</summary>
        public bool ConstrainedVbr;
        /// <summary>In-band FEC for the previous frame (costs bitrate, pays off above ~2 % loss).</summary>
        public bool Fec;
        /// <summary>Loss the encoder should plan for, 0..100 (drives how much FEC it emits).</summary>
        public int ExpectedLossPercent;
        /// <summary>Discontinuous transmission during silence.</summary>
        public bool Dtx;

        public static OpusEncoderSettings Default => new OpusEncoderSettings
        {
            BitrateBps = 32000,
            Complexity = 9,
            MaxBandwidth = OpusBandwidth.Fullband,
            Signal = OpusSignal.Voice,
            Vbr = true,
            ConstrainedVbr = true,
            Fec = true,
            ExpectedLossPercent = 5,
            Dtx = false,
        };

        /// <summary>Copy with every numeric field pulled into the range libopus accepts.</summary>
        public OpusEncoderSettings Clamped()
        {
            var s = this;
            s.BitrateBps = Math.Clamp(s.BitrateBps, MinBitrate, MaxBitrate);
            s.Complexity = Math.Clamp(s.Complexity, 0, MaxComplexity);
            s.ExpectedLossPercent = Math.Clamp(s.ExpectedLossPercent, 0, 100);
            return s;
        }

        /// <summary>
        /// These settings under a channel policy: bitrate, FEC, DTX, bandwidth and signal come from the
        /// policy; complexity from <paramref name="localComplexity"/> (a local pin), else the policy's
        /// hint, else unchanged. VBR/constrained VBR/expected loss stay local.
        /// </summary>
        public OpusEncoderSettings WithPolicy(AudioPolicy policy, int? localComplexity)
        {
            var s = this;
            s.BitrateBps = policy.BitrateBps;
            s.Fec = policy.Fec;
            s.Dtx = policy.Dtx;
            s.MaxBandwidth = policy.MaxBandwidth;
            s.Signal = policy.Signal;
            var c = localComplexity ?? policy.Complexity;
            if (c.HasValue) s.Complexity = c.Value;
            return s.Clamped();
        }

        public bool Equals(OpusEncoderSettings o) =>
            BitrateBps == o.BitrateBps && Complexity == o.Complexity && MaxBandwidth == o.MaxBandwidth &&
            Signal == o.Signal && Vbr == o.Vbr && ConstrainedVbr == o.ConstrainedVbr && Fec == o.Fec &&
            ExpectedLossPercent == o.ExpectedLossPercent && Dtx == o.Dtx;

        public override bool Equals(object obj) => obj is OpusEncoderSettings o && Equals(o);

        public override int GetHashCode() =>
            HashCode.Combine(BitrateBps, Complexity, (int)MaxBandwidth, (int)Signal,
                (Vbr ? 1 : 0) | (ConstrainedVbr ? 2 : 0) | (Fec ? 4 : 0) | (Dtx ? 8 : 0), ExpectedLossPercent);

        public override string ToString() =>
            $"{BitrateBps} bps c{Complexity} {MaxBandwidth} {Signal}" +
            $"{(Vbr ? (ConstrainedVbr ? " cvbr" : " vbr") : " cbr")}{(Fec ? $" fec({ExpectedLossPercent}%)" : "")}{(Dtx ? " dtx" : "")}";
    }

    /// <summary>
    /// Encoder policy a channel requires of everyone sending into it (<c>ChannelJoinAck.audio</c>,
    /// <c>ChannelAudioPolicy</c>). A client in several channels applies <see cref="Merge"/> over all of them.
    /// </summary>
    public struct AudioPolicy : IEquatable<AudioPolicy>
    {
        public int BitrateBps;
        /// <summary>Floor the server's adaptive <c>BitrateCommand</c> never goes below.</summary>
        public int MinBitrateBps;
        public bool Fec;
        public bool Dtx;
        public OpusBandwidth MaxBandwidth;
        /// <summary>Complexity hint, or null when the channel leaves it to the client.</summary>
        public int? Complexity;
        public OpusSignal Signal;

        /// <summary>The server's default channel policy (48 kbit/s, FEC+DTX, fullband, voice, no complexity hint).</summary>
        public static AudioPolicy Default => new AudioPolicy
        {
            BitrateBps = 48000,
            MinBitrateBps = 12000,
            Fec = true,
            Dtx = true,
            MaxBandwidth = OpusBandwidth.Fullband,
            Complexity = null,
            Signal = OpusSignal.Voice,
        };

        /// <summary>
        /// Combined policy for one encoder feeding several channels: the widest bitrate and bandwidth so
        /// no channel is starved, FEC if any wants it, DTX only if every channel allows it, the highest
        /// complexity hint, and Music if any channel is music.
        /// </summary>
        public AudioPolicy Merge(AudioPolicy o) => new AudioPolicy
        {
            BitrateBps = Math.Max(BitrateBps, o.BitrateBps),
            MinBitrateBps = Math.Max(MinBitrateBps, o.MinBitrateBps),
            Fec = Fec || o.Fec,
            Dtx = Dtx && o.Dtx,
            MaxBandwidth = (OpusBandwidth)Math.Max((int)MaxBandwidth, (int)o.MaxBandwidth),
            Complexity = Complexity.HasValue && o.Complexity.HasValue
                ? Math.Max(Complexity.Value, o.Complexity.Value)
                : Complexity ?? o.Complexity,
            Signal = Signal == OpusSignal.Music || o.Signal == OpusSignal.Music ? OpusSignal.Music
                : Signal == OpusSignal.Voice || o.Signal == OpusSignal.Voice ? OpusSignal.Voice
                : OpusSignal.Auto,
        };

        /// <summary>Merge of all policies; <see cref="Default"/> when there are none.</summary>
        public static AudioPolicy MergeAll(IEnumerable<AudioPolicy> policies)
        {
            AudioPolicy? acc = null;
            foreach (var p in policies) acc = acc.HasValue ? acc.Value.Merge(p) : p;
            return acc ?? Default;
        }

        /// <summary>Parse the <c>audio</c> object of a control message; missing keys take the server defaults.</summary>
        public static AudioPolicy FromObject(Dictionary<string, object> o)
        {
            var d = Default;
            if (o == null) return d;
            var p = new AudioPolicy
            {
                BitrateBps = (int)MiniJson.GetNumber(o, "bitrate_bps", d.BitrateBps),
                MinBitrateBps = (int)MiniJson.GetNumber(o, "min_bitrate_bps", d.MinBitrateBps),
                Fec = MiniJson.GetBool(o, "fec", d.Fec),
                Dtx = MiniJson.GetBool(o, "dtx", d.Dtx),
                MaxBandwidth = OpusEnums.ParseBandwidth(MiniJson.GetString(o, "max_bandwidth"), d.MaxBandwidth),
                Signal = OpusEnums.ParseSignal(MiniJson.GetString(o, "signal"), d.Signal),
            };
            if (o.TryGetValue("complexity", out var c) && c is double cd) p.Complexity = (int)cd;
            return p;
        }

        /// <summary>The <c>audio</c> object carried by <c>ChannelJoinAck</c>/<c>ChannelAudioPolicy</c>, or null.</summary>
        public static AudioPolicy? FromMessage(ControlMessage m)
        {
            if (m.Data == null || !m.Data.TryGetValue("audio", out var v)) return null;
            var o = MiniJson.AsObject(v);
            return o == null ? (AudioPolicy?)null : FromObject(o);
        }

        public bool Equals(AudioPolicy o) =>
            BitrateBps == o.BitrateBps && MinBitrateBps == o.MinBitrateBps && Fec == o.Fec && Dtx == o.Dtx &&
            MaxBandwidth == o.MaxBandwidth && Complexity == o.Complexity && Signal == o.Signal;

        public override bool Equals(object obj) => obj is AudioPolicy o && Equals(o);

        public override int GetHashCode() =>
            HashCode.Combine(BitrateBps, MinBitrateBps, Fec, Dtx, (int)MaxBandwidth, Complexity ?? -1, (int)Signal);

        public override string ToString() =>
            $"{BitrateBps} bps (min {MinBitrateBps}) {MaxBandwidth} {Signal}" +
            $"{(Fec ? " fec" : "")}{(Dtx ? " dtx" : "")}{(Complexity.HasValue ? $" c{Complexity.Value}" : "")}";
    }
}
