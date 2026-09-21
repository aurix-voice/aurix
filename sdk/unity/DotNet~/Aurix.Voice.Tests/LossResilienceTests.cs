using System;
using System.Collections.Generic;
using Aurix.Audio;
using Xunit;

namespace Aurix.Voice.Tests
{
    public class LossResilienceTests
    {
        private static readonly DateTime T0 = new DateTime(2026, 1, 1, 0, 0, 0, DateTimeKind.Utc);
        private static DateTime At(double secs) => T0.AddSeconds(secs);

        [Fact]
        public void EscalatesImmediatelyAndRelaxesWithHysteresisAndDwell()
        {
            var c = new LossController();
            Assert.Equal(LossProfile.Low, c.Profile);
            Assert.Null(c.Pinned);
            Assert.False(c.Observe(2.9f, At(0)));
            Assert.True(c.Observe(3f, At(2)));
            Assert.Equal(LossProfile.Moderate, c.Profile);
            Assert.False(c.Observe(2f, At(4)));   // below entry, above exit: stays
            Assert.False(c.Observe(0.5f, At(6))); // below exit but within the 6 s dwell
            Assert.True(c.Observe(0.5f, At(8)));
            Assert.Equal(LossProfile.Low, c.Profile);
            Assert.True(c.Observe(25f, At(10)));  // straight to High on a burst
            Assert.Equal(LossProfile.High, c.Profile);
            Assert.False(c.Observe(6f, At(20)));
            Assert.True(c.Observe(4f, At(22)));
            Assert.Equal(LossProfile.Moderate, c.Profile);
            Assert.True(c.Observe(12f, At(23)));
            Assert.Equal(LossProfile.High, c.Profile);
            Assert.Equal(12f, c.UplinkLossPercent);

            // NaN / out-of-range reports are sanitised.
            var d = new LossController();
            Assert.False(d.Observe(float.NaN, At(0)));
            Assert.Equal(0f, d.UplinkLossPercent);
            Assert.True(d.Observe(250f, At(1)));
            Assert.Equal(100f, d.UplinkLossPercent);
        }

        [Fact]
        public void PinnedProfileIgnoresReportsAndAutoResumesFromTheLastReport()
        {
            var c = new LossController();
            Assert.True(c.SetPinned(LossProfile.High, At(0)));
            Assert.False(c.Observe(0f, At(1)));
            Assert.Equal(LossProfile.High, c.Profile);
            Assert.True(c.SetPinned(null, At(2)));
            Assert.Equal(LossProfile.Low, c.Profile);
            c.Observe(15f, At(3));
            Assert.True(c.SetPinned(LossProfile.Low, At(4)));
            Assert.True(c.SetPinned(null, At(5))); // re-evaluates the last report at once
            Assert.Equal(LossProfile.High, c.Profile);
            c.Reset();
            Assert.Equal(LossProfile.Low, c.Profile);
            Assert.Equal(0f, c.UplinkLossPercent);

            var pinned = new LossController(LossProfile.Moderate, LossProfilePolicy.Default);
            pinned.Reset();
            Assert.Equal(LossProfile.Moderate, pinned.Profile);
        }

        [Fact]
        public void ProfilesShapeFecExpectedLossDredAndBitrateFloor()
        {
            var baseline = OpusEncoderSettings.Default;
            baseline.BitrateBps = 16000;
            baseline.Fec = false;
            baseline.ExpectedLossPercent = 2;
            baseline.DredDurationMs = 0;

            var low = LossProfilePolicy.Shape(LossProfile.Low, baseline, 30f, 48000);
            Assert.Equal(baseline, low);

            var moderate = LossProfilePolicy.Shape(LossProfile.Moderate, baseline, 4f, 48000);
            Assert.True(moderate.Fec);
            Assert.Equal(LossProfilePolicy.ModerateExpectedLoss, moderate.ExpectedLossPercent);
            Assert.Equal(16000, moderate.BitrateBps);
            Assert.Equal(0, moderate.DredDurationMs);
            Assert.Equal(14, LossProfilePolicy.Shape(LossProfile.Moderate, baseline, 14.2f, null).ExpectedLossPercent);

            var high = LossProfilePolicy.Shape(LossProfile.High, baseline, 12f, 48000);
            Assert.True(high.Fec);
            Assert.Equal(LossProfilePolicy.HighExpectedLoss, high.ExpectedLossPercent);
            Assert.Equal(LossProfilePolicy.HighDredDurationMs, high.DredDurationMs);
            Assert.Equal(LossProfilePolicy.DredMinBitrateBps, high.BitrateBps);
            // The floor never exceeds the channel's target …
            Assert.Equal(20000, LossProfilePolicy.Shape(LossProfile.High, baseline, 12f, 20000).BitrateBps);
            // … and a baseline already above it (or with a longer DRED) is left alone.
            var rich = baseline;
            rich.BitrateBps = 40000;
            rich.DredDurationMs = 1000;
            var shaped = LossProfilePolicy.Shape(LossProfile.High, rich, 12f, null);
            Assert.Equal(40000, shaped.BitrateBps);
            Assert.Equal(1000, shaped.DredDurationMs);

            // DRED duration is clamped to 10 ms steps within libopus' cap; it is part of equality.
            var odd = new OpusEncoderSettings { DredDurationMs = 5000, BitrateBps = 32000 }.Clamped();
            Assert.Equal(OpusEncoderSettings.MaxDredDurationMs, odd.DredDurationMs);
            Assert.Equal(120, new OpusEncoderSettings { DredDurationMs = 125, BitrateBps = 32000 }.Clamped().DredDurationMs);
            Assert.NotEqual(baseline, high);
            Assert.Contains("dred(400ms)", high.ToString());
            Assert.Equal(5, OpusDecoderSettings.Default.Complexity);
            Assert.Equal(10, new OpusDecoderSettings { Complexity = 42 }.Clamped().Complexity);
        }

        /// <summary>
        /// Stub with FEC and DRED at distinct levels so the mixer's recovery order (FEC → DRED → PLC) is observable.
        /// DRED reaches <see cref="Reach"/> frames back.
        /// </summary>
        private sealed class RedundantCodec : IOpusCodec, IOpusFecDecoder, IOpusDredDecoder, IOpusDecoderControls
        {
            public const float Level = 0.5f, FecLevel = 0.25f, DredLevel = 0.125f;
            public const int Reach = 3;
            public readonly List<int> DredAsks = new List<int>();
            public OpusDecoderSettings DecoderSettings { get; private set; } = OpusDecoderSettings.Default;
            public int SampleRate => AudioFormat.SampleRate;
            public int Channels => 1;
            public int Encode(ReadOnlySpan<float> pcm, int frameSamplesPerChannel, Span<byte> output) => 0;
            public int Decode(ReadOnlySpan<byte> opus, Span<float> pcm, int maxFrameSamplesPerChannel) { pcm.Slice(0, AudioFormat.FrameSamples).Fill(Level); return AudioFormat.FrameSamples; }
            public int DecodeLost(Span<float> pcm, int frameSamplesPerChannel) { pcm.Slice(0, frameSamplesPerChannel).Fill(0f); return frameSamplesPerChannel; }
            public int DecodeFec(ReadOnlySpan<byte> nextPacket, Span<float> pcm, int frameSamplesPerChannel)
            {
                if (nextPacket[0] == 0xFF) return 0;
                pcm.Slice(0, frameSamplesPerChannel).Fill(FecLevel);
                return frameSamplesPerChannel;
            }
            public int DecodeDred(ReadOnlySpan<byte> laterPacket, int framesBefore, Span<float> pcm, int frameSamplesPerChannel)
            {
                DredAsks.Add(framesBefore);
                if (laterPacket[0] == 0xFF || framesBefore > Reach) return 0;
                pcm.Slice(0, frameSamplesPerChannel).Fill(DredLevel);
                return frameSamplesPerChannel;
            }
            public void ApplyDecoder(OpusDecoderSettings settings) => DecoderSettings = settings.Clamped();
            public void SetBitrate(int bitsPerSecond) { }
            public void Dispose() { }
        }

        [Fact]
        public void RemoteMixerRebuildsBurstsFromDredBeforePlcAndAppliesDecoderSettings()
        {
            var codecs = new List<RedundantCodec>();
            var mixer = new RemoteMixer(() => { var c = new RedundantCodec(); codecs.Add(c); return c; });
            mixer.DecoderSettings = new OpusDecoderSettings { Complexity = 7, OsceBwe = true };
            var mono = new float[AudioFormat.FrameSamples];
            // 0, 1, (2 3 4 5 6 lost), 7, 8: 6 comes from FEC, 5..4 from DRED (reach 3), 3..2 PLC.
            foreach (uint seq in new uint[] { 0, 1, 7, 8 }) mixer.Push(9, seq, 1f, new byte[] { 1 });
            var levels = new List<float>();
            for (int i = 0; i < 9; i++) { Array.Clear(mono, 0, mono.Length); mixer.Mix(mono, 1); levels.Add(mono[0]); }
            Assert.Equal(new[]
            {
                RedundantCodec.Level, RedundantCodec.Level,
                0f, 0f, RedundantCodec.DredLevel, RedundantCodec.DredLevel, RedundantCodec.FecLevel,
                RedundantCodec.Level, RedundantCodec.Level,
            }, levels.ToArray());
            var t = mixer.Totals;
            Assert.Equal(5, t.Lost);
            Assert.Equal(1, t.FecRecovered);
            Assert.Equal(2, t.DredRecovered);
            Assert.Equal(new[] { 5, 4, 3, 2 }, codecs[0].DredAsks.ToArray()); // oldest first, 1 frame back went to FEC
            Assert.Equal(7, codecs[0].DecoderSettings.Complexity);
            Assert.True(codecs[0].DecoderSettings.OsceBwe);

            // A later change reaches running decoders; a new stream starts with it.
            mixer.DecoderSettings = new OpusDecoderSettings { Complexity = 3 };
            Assert.Equal(3, codecs[0].DecoderSettings.Complexity);
            mixer.Push(10, 0, 1f, new byte[] { 1 });
            Assert.Equal(3, codecs[1].DecoderSettings.Complexity);

            // A packet without redundancy: FEC and DRED both decline, PLC fills in.
            var plain = new RemoteMixer(() => new RedundantCodec());
            plain.Push(9, 0, 1f, new byte[] { 1 });
            plain.Push(9, 3, 1f, new byte[] { 0xFF });
            for (int i = 0; i < 4; i++) { Array.Clear(mono, 0, mono.Length); plain.Mix(mono, 1); levels.Add(mono[0]); }
            Assert.Equal(new[] { RedundantCodec.Level, 0f, 0f, RedundantCodec.Level }, levels.GetRange(9, 4).ToArray());
            Assert.Equal(2, plain.Totals.Lost);
            Assert.Equal(0, plain.Totals.FecRecovered + plain.Totals.DredRecovered);
        }

        [Fact]
        public void JitterBufferReportsHowFarTheLaterPacketIs()
        {
            var jb = new JitterBuffer(targetDepthFrames: 1);
            jb.Push(0, new byte[] { 0 });
            jb.Push(4, new byte[] { 4 });
            Assert.True(jb.Pop(out var p, out var later, out int back));
            Assert.Equal(0, p[0]);
            Assert.Null(later);
            Assert.Equal(0, back);
            Assert.True(jb.Pop(out p, out later, out back)); // slot 1 lost, packet 4 is 3 frames later
            Assert.Null(p);
            Assert.Equal(4, later[0]);
            Assert.Equal(3, back);
            Assert.True(jb.Pop(out p, out later, out back));
            Assert.Equal(2, back);
            Assert.True(jb.Pop(out p, out later, out var fec));
            Assert.Equal(1, fec);
            Assert.Equal(1, jb.Recoverable);
            Assert.Equal(3, jb.Lost);
            Assert.True(jb.Pop(out p, out byte[] fecFrom));
            Assert.Equal(4, p[0]);
            Assert.Null(fecFrom);
        }

        /// <summary>
        /// Speech-like test signal (same shape as <c>aurix_opus::testing::speech_like_f32</c>): SILK keeps classifying
        /// it as speech, so LBRR and DRED are produced in steady state where a tone would be adapted away.
        /// </summary>
        private static float[] SpeechLike(int frames)
        {
            var pcm = new float[frames * AudioFormat.FrameSamples];
            uint seed = 0x12345678;
            for (int i = 0; i < pcm.Length; i++)
            {
                float t = i / 48000f;
                float syllable = MathF.Floor(t / 0.18f);
                float phase = (t % 0.18f) / 0.18f;
                bool voiced = phase < 0.7f;
                float f0 = 110f + 25f * MathF.Sin(syllable * 0.9f) + 15f * MathF.Sin(t * 4f);
                float formant = 500f + 400f * (MathF.Sin(syllable * 1.7f) + 1f);
                float env = voiced ? MathF.Min(phase / 0.1f, 1f) * MathF.Min((0.7f - phase) / 0.1f, 1f) : 0f;
                float s = 0f;
                for (int h = 1; h <= 12; h++)
                {
                    float f = f0 * h;
                    float w = 1f / (1f + MathF.Pow((f - formant) / 300f, 2));
                    s += MathF.Sin(2f * MathF.PI * f * t) * w;
                }
                seed = unchecked(seed * 1664525u + 1013904223u);
                float noise = ((seed >> 9) / (float)(1u << 23) - 1f) * 0.02f;
                pcm[i] = (s * 0.4f * env + noise) * 0.27f;
            }
            return pcm;
        }

        private static float Rms(ReadOnlySpan<float> pcm)
        {
            double acc = 0;
            foreach (var v in pcm) acc += v * v;
            return (float)Math.Sqrt(acc / Math.Max(1, pcm.Length));
        }

        [Fact]
        public void NativeCodecRebuildsABurstFromDredAndTunesTheDecoder()
        {
            if (!NativeLib.TryLoad() || !NativeOpusCodec.IsAvailable)
            {
                Assert.NotEqual("1", Environment.GetEnvironmentVariable("AURIX_REQUIRE_NATIVE"));
                return;
            }
            Assert.True(NativeOpusCodec.DredSupported); // the bundled libopus 1.6 has it
            var settings = OpusEncoderSettings.Default;
            settings.BitrateBps = 40000;
            settings.Complexity = 10;
            settings.Fec = true;
            settings.ExpectedLossPercent = 30;
            settings.Dtx = false;
            settings.DredDurationMs = 400;
            using var codec = new NativeOpusCodec(AudioFormat.SampleRate, 1, settings);
            Assert.Equal(400, codec.Settings.DredDurationMs);
            codec.ApplyDecoder(new OpusDecoderSettings { Complexity = 6, OsceBwe = false });
            Assert.Equal(6, codec.DecoderSettings.Complexity);

            const int n = 50;
            var speech = SpeechLike(n);
            var packets = new byte[n][];
            var buf = new byte[NativeOpusCodec.MaxPacketBytes];
            for (int i = 0; i < n; i++)
            {
                int len = codec.Encode(new ReadOnlySpan<float>(speech, i * AudioFormat.FrameSamples, AudioFormat.FrameSamples), AudioFormat.FrameSamples, buf);
                packets[i] = buf.AsSpan(0, len).ToArray();
            }
            var pcm = new float[AudioFormat.FrameSamples];
            for (int i = 0; i < 30; i++) codec.Decode(packets[i], pcm, AudioFormat.FrameSamples);

            // Frames 30..33 lost: packet 34 rebuilds 30..32 from DRED (4, 3, 2 frames back) and 33 from FEC.
            var rebuilt = new List<float>();
            for (int back = 4; back >= 2; back--)
            {
                Assert.Equal(AudioFormat.FrameSamples, codec.DecodeDred(packets[34], back, pcm, AudioFormat.FrameSamples));
                rebuilt.AddRange(pcm);
            }
            float reference = Rms(new ReadOnlySpan<float>(speech, 30 * AudioFormat.FrameSamples, 3 * AudioFormat.FrameSamples));
            float got = Rms(rebuilt.ToArray());
            Assert.InRange(got, reference * 0.3f, reference * 3f);
            Assert.Equal(AudioFormat.FrameSamples, codec.DecodeFec(packets[34], pcm, AudioFormat.FrameSamples));
            Assert.Equal(AudioFormat.FrameSamples, codec.Decode(packets[34], pcm, AudioFormat.FrameSamples));
            // Beyond what the packet carries → 0; a DRED-less packet → 0 for any distance.
            Assert.Equal(0, codec.DecodeDred(packets[34], 60, pcm, AudioFormat.FrameSamples));
            var plain = settings;
            plain.DredDurationMs = 0;
            using var noDred = new NativeOpusCodec(AudioFormat.SampleRate, 1, plain);
            Assert.Equal(0, noDred.Settings.DredDurationMs);
            int len2 = noDred.Encode(new ReadOnlySpan<float>(speech, 0, AudioFormat.FrameSamples), AudioFormat.FrameSamples, buf);
            Assert.Equal(0, noDred.DecodeDred(buf.AsSpan(0, len2).ToArray(), 2, pcm, AudioFormat.FrameSamples));
            Assert.Equal(0, noDred.DecodeDred(ReadOnlySpan<byte>.Empty, 2, pcm, AudioFormat.FrameSamples));

            // The mixer path end to end: a burst through a real decoder is attributed to FEC/DRED, not PLC only.
            var mixer = new RemoteMixer(() => new NativeOpusCodec(AudioFormat.SampleRate, 1));
            var lost = new HashSet<uint> { 30, 31, 32, 33 };
            var outFrame = new float[AudioFormat.FrameSamples];
            for (uint seq = 0; seq < n; seq++)
            {
                if (!lost.Contains(seq)) mixer.Push(7, seq, 1f, packets[seq]);
                Array.Clear(outFrame, 0, outFrame.Length);
                mixer.Mix(outFrame, 1);
            }
            for (int i = 0; i < 4; i++) mixer.Mix(outFrame, 1);
            var t = mixer.Totals;
            Assert.Equal(4, t.Lost);
            Assert.Equal(1, t.FecRecovered);
            Assert.Equal(3, t.DredRecovered);
            mixer.Dispose();
        }
    }
}
