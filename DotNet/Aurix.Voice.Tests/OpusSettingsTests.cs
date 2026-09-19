using System;
using System.Collections.Generic;
using System.IO;
using System.Reflection;
using System.Runtime.InteropServices;
using Aurix.Audio;
using Aurix.Protocol;
using Aurix.Samples;
using Xunit;

namespace Aurix.Voice.Tests
{
    public class OpusSettingsTests
    {
        private static readonly Guid Team = Guid.Parse("11111111-2222-3333-4444-555555555555");
        private static readonly Guid Party = Guid.Parse("11111111-2222-3333-4444-555555555556");

        // Pinned in aurix-common's `audio_policy_wire_shape` test.
        private const string PolicyJson =
            "{\"bitrate_bps\":24000,\"min_bitrate_bps\":8000,\"fec\":true,\"dtx\":false,\"max_bandwidth\":\"wideband\",\"complexity\":5,\"signal\":\"voice\"}";

        [Fact]
        public void EncoderSettingsClampToLibopusRanges()
        {
            var s = new OpusEncoderSettings { BitrateBps = 999_999, Complexity = 42, ExpectedLossPercent = 300 }.Clamped();
            Assert.Equal(OpusEncoderSettings.MaxBitrate, s.BitrateBps);
            Assert.Equal(10, s.Complexity);
            Assert.Equal(100, s.ExpectedLossPercent);
            s = new OpusEncoderSettings { BitrateBps = 1, Complexity = -3, ExpectedLossPercent = -1 }.Clamped();
            Assert.Equal(OpusEncoderSettings.MinBitrate, s.BitrateBps);
            Assert.Equal(0, s.Complexity);
            Assert.Equal(0, s.ExpectedLossPercent);

            var d = OpusEncoderSettings.Default;
            Assert.Equal(d, d.Clamped());
            Assert.True(d.Vbr && d.ConstrainedVbr && d.Fec && !d.Dtx);
            Assert.Equal(OpusSignal.Voice, d.Signal);
            Assert.Equal(OpusBandwidth.Fullband, d.MaxBandwidth);
        }

        [Fact]
        public void PolicyParsesServerWireAndFillsDefaults()
        {
            var m = ControlMessage.Parse("{\"type\":\"ChannelAudioPolicy\",\"data\":{\"channel_id\":\"" + Team + "\",\"audio\":" + PolicyJson + "}}");
            var p = AudioPolicy.FromMessage(m).Value;
            Assert.Equal(24000, p.BitrateBps);
            Assert.Equal(8000, p.MinBitrateBps);
            Assert.True(p.Fec);
            Assert.False(p.Dtx);
            Assert.Equal(OpusBandwidth.Wideband, p.MaxBandwidth);
            Assert.Equal(5, p.Complexity);
            Assert.Equal(OpusSignal.Voice, p.Signal);

            var sparse = AudioPolicy.FromMessage(ControlMessage.Parse(
                "{\"type\":\"ChannelJoinAck\",\"data\":{\"channel_id\":\"" + Team + "\",\"participants\":[],\"audio\":{\"bitrate_bps\":16000,\"signal\":\"music\",\"complexity\":null}}}")).Value;
            Assert.Equal(16000, sparse.BitrateBps);
            Assert.Equal(OpusSignal.Music, sparse.Signal);
            Assert.Equal(AudioPolicy.Default.MinBitrateBps, sparse.MinBitrateBps);
            Assert.Equal(OpusBandwidth.Fullband, sparse.MaxBandwidth);
            Assert.Null(sparse.Complexity);
            Assert.True(sparse.Dtx);

            // Older servers send no `audio` block at all.
            Assert.Null(AudioPolicy.FromMessage(ControlMessage.Parse("{\"type\":\"ChannelJoinAck\",\"data\":{\"channel_id\":\"" + Team + "\",\"participants\":[]}}")));
            Assert.Equal(OpusBandwidth.Narrowband, OpusEnums.ParseBandwidth("narrowband", OpusBandwidth.Fullband));
            Assert.Equal(OpusBandwidth.Fullband, OpusEnums.ParseBandwidth("bogus", OpusBandwidth.Fullband));
            Assert.Equal("superwideband", OpusBandwidth.Superwideband.ToWire());
            Assert.Equal(16000, OpusBandwidth.Wideband.PlaybackRateHz());
        }

        [Fact]
        public void PolicyMergeMatchesNativeSemantics()
        {
            var quiet = new AudioPolicy { BitrateBps = 16000, MinBitrateBps = 8000, Fec = false, Dtx = true, MaxBandwidth = OpusBandwidth.Wideband, Complexity = 3, Signal = OpusSignal.Voice };
            var music = new AudioPolicy { BitrateBps = 96000, MinBitrateBps = 32000, Fec = true, Dtx = false, MaxBandwidth = OpusBandwidth.Fullband, Complexity = null, Signal = OpusSignal.Music };
            var m = quiet.Merge(music);
            Assert.Equal(96000, m.BitrateBps);
            Assert.Equal(32000, m.MinBitrateBps);
            Assert.True(m.Fec);          // any channel wanting FEC gets it
            Assert.False(m.Dtx);         // DTX only if every channel allows it
            Assert.Equal(OpusBandwidth.Fullband, m.MaxBandwidth);
            Assert.Equal(3, m.Complexity); // the only hint survives
            Assert.Equal(OpusSignal.Music, m.Signal);
            Assert.Equal(m, music.Merge(quiet));
            Assert.Equal(AudioPolicy.Default, AudioPolicy.MergeAll(new AudioPolicy[0]));
            Assert.Equal(quiet, AudioPolicy.MergeAll(new[] { quiet }));

            // Applying a policy: bitrate/FEC/DTX/bandwidth/signal from the policy, complexity pin wins over the hint.
            var baseline = OpusEncoderSettings.Default;
            baseline.BitrateBps = 32000;
            baseline.Complexity = 9;
            var applied = baseline.WithPolicy(quiet, null);
            Assert.Equal(16000, applied.BitrateBps);
            Assert.False(applied.Fec);
            Assert.True(applied.Dtx);
            Assert.Equal(OpusBandwidth.Wideband, applied.MaxBandwidth);
            Assert.Equal(3, applied.Complexity);
            Assert.Equal(7, baseline.WithPolicy(quiet, 7).Complexity);
            Assert.Equal(9, baseline.WithPolicy(music, null).Complexity); // no hint → keep ours
            Assert.True(applied.Vbr && applied.ConstrainedVbr); // local-only knobs untouched
        }

        private sealed class RecordingCodec : IOpusCodec, IOpusEncoderControls
        {
            public readonly List<OpusEncoderSettings> Applied = new List<OpusEncoderSettings>();
            public OpusEncoderSettings Settings { get; private set; } = OpusEncoderSettings.Default;
            public int SampleRate => AudioFormat.SampleRate;
            public int Channels => 1;
            public void Apply(OpusEncoderSettings settings) { Settings = settings; Applied.Add(settings); }
            public void SetBitrate(int bitsPerSecond) => throw new InvalidOperationException("Apply should be preferred");
            public int Encode(ReadOnlySpan<float> pcm, int frameSamplesPerChannel, Span<byte> output) => 0;
            public int Decode(ReadOnlySpan<byte> opus, Span<float> pcm, int maxFrameSamplesPerChannel) => 0;
            public int DecodeLost(Span<float> pcm, int frameSamplesPerChannel) => 0;
            public void Dispose() { }
        }

        private sealed class BitrateOnlyCodec : IOpusCodec
        {
            public int Bitrate;
            public int SampleRate => AudioFormat.SampleRate;
            public int Channels => 1;
            public void SetBitrate(int bitsPerSecond) => Bitrate = bitsPerSecond;
            public int Encode(ReadOnlySpan<float> pcm, int frameSamplesPerChannel, Span<byte> output) => 0;
            public int Decode(ReadOnlySpan<byte> opus, Span<float> pcm, int maxFrameSamplesPerChannel) => 0;
            public int DecodeLost(Span<float> pcm, int frameSamplesPerChannel) => 0;
            public void Dispose() { }
        }

        private static ControlMessage JoinAck(Guid channel, string audio) =>
            ControlMessage.Parse("{\"type\":\"ChannelJoinAck\",\"data\":{\"channel_id\":\"" + channel + "\",\"participants\":[],\"audio\":" + audio + "}}");

        [Fact]
        public void ClientLayersPolicyPinAndBitrateCommandOverTheBaseline()
        {
            var client = new AurixVoiceClient("ws://127.0.0.1:1", "token");
            var codec = new RecordingCodec();
            var policies = new List<AudioPolicy>();
            var settings = new List<OpusEncoderSettings>();
            var commands = new List<BitrateCommand>();
            client.OnAudioPolicyChanged += policies.Add;
            client.OnEncoderSettingsChanged += settings.Add;
            client.OnBitrateCommand += commands.Add;

            var baseline = OpusEncoderSettings.Default;
            baseline.BitrateBps = 32000;
            baseline.Complexity = 9;
            client.SetEncoderSettings(baseline);
            client.Encoder = codec;
            Assert.Single(codec.Applied);
            Assert.Equal(baseline, codec.Settings);
            Assert.Null(client.AudioPolicy);

            // Joining a wideband/16k/no-DTX channel retunes the encoder; the hint (5) is taken since nothing is pinned.
            client.HandleMessage(JoinAck(Team, PolicyJson));
            Assert.Single(policies);
            Assert.Equal(24000, codec.Settings.BitrateBps);
            Assert.Equal(OpusBandwidth.Wideband, codec.Settings.MaxBandwidth);
            Assert.False(codec.Settings.Dtx);
            Assert.Equal(5, codec.Settings.Complexity);
            Assert.Equal(codec.Settings, client.EffectiveEncoderSettings);
            Assert.Equal(baseline, client.EncoderSettings); // baseline untouched

            // A second (music, 96k, fullband) channel widens the merged policy.
            client.HandleMessage(JoinAck(Party, "{\"bitrate_bps\":96000,\"dtx\":true,\"max_bandwidth\":\"fullband\",\"signal\":\"music\"}"));
            Assert.Equal(2, policies.Count);
            Assert.Equal(96000, codec.Settings.BitrateBps);
            Assert.Equal(OpusSignal.Music, codec.Settings.Signal);
            Assert.Equal(OpusBandwidth.Fullband, codec.Settings.MaxBandwidth);
            Assert.False(codec.Settings.Dtx); // Team forbids DTX

            // The same policy again is a no-op; an operator edit on Team is not.
            int before = settings.Count;
            client.HandleMessage(ControlMessage.Parse("{\"type\":\"ChannelAudioPolicy\",\"data\":{\"channel_id\":\"" + Team + "\",\"audio\":" + PolicyJson + "}}"));
            Assert.Equal(before, settings.Count);
            Assert.Equal(2, policies.Count);
            client.HandleMessage(ControlMessage.Parse("{\"type\":\"ChannelAudioPolicy\",\"data\":{\"channel_id\":\"" + Team + "\",\"audio\":{\"bitrate_bps\":24000,\"dtx\":true,\"complexity\":8}}}"));
            Assert.Equal(3, policies.Count);
            Assert.True(codec.Settings.Dtx);
            Assert.Equal(8, codec.Settings.Complexity);
            // A policy for a channel we are not in is ignored.
            client.HandleMessage(ControlMessage.Parse("{\"type\":\"ChannelAudioPolicy\",\"data\":{\"channel_id\":\"" + Guid.NewGuid() + "\",\"audio\":{\"bitrate_bps\":6000}}}"));
            Assert.Equal(3, policies.Count);

            // Pinning complexity beats the hint; the server's bitrate command is transient and raises the FEC loss estimate.
            client.SetComplexity(4);
            Assert.Equal(4, codec.Settings.Complexity);
            client.HandleMessage(ControlMessage.Parse("{\"type\":\"BitrateCommand\",\"data\":{\"target_bitrate_kbps\":16,\"reason\":\"loss\",\"expected_loss_percent\":30}}"));
            Assert.Single(commands);
            Assert.Equal(16u, commands[0].TargetBitrateKbps);
            Assert.Equal(30, commands[0].ExpectedLossPercent);
            Assert.Equal(16000, codec.Settings.BitrateBps);
            Assert.Equal(30, codec.Settings.ExpectedLossPercent);
            Assert.Equal(OpusSignal.Music, codec.Settings.Signal); // everything else kept
            // Replacing the baseline forgets the command; the policy stays layered.
            client.SetEncoderSettings(baseline);
            Assert.Equal(96000, codec.Settings.BitrateBps);
            Assert.Equal(baseline.ExpectedLossPercent, codec.Settings.ExpectedLossPercent);
            Assert.Equal(4, codec.Settings.Complexity);

            // Not following the policy: baseline + pin only. Following again re-applies.
            client.FollowChannelPolicy = false;
            Assert.Equal(32000, codec.Settings.BitrateBps);
            Assert.Equal(OpusSignal.Voice, codec.Settings.Signal);
            Assert.Equal(4, codec.Settings.Complexity);
            client.FollowChannelPolicy = true;
            Assert.Equal(96000, codec.Settings.BitrateBps);

            // Kick from Party narrows back to Team's policy; leaving the last channel keeps the previous one.
            client.HandleMessage(ControlMessage.Parse("{\"type\":\"Kick\",\"data\":{\"channel_id\":\"" + Party + "\",\"reason\":\"bye\"}}"));
            Assert.Equal(4, policies.Count);
            Assert.Equal(24000, codec.Settings.BitrateBps);
            Assert.Equal(OpusSignal.Voice, codec.Settings.Signal);
            client.HandleMessage(ControlMessage.Parse("{\"type\":\"Kick\",\"data\":{\"channel_id\":\"" + Team + "\",\"reason\":\"bye\"}}"));
            Assert.Equal(4, policies.Count);
            Assert.Equal(24000, client.AudioPolicy.Value.BitrateBps);

            // A codec without full controls only gets the bitrate.
            var simple = new BitrateOnlyCodec();
            client.Encoder = simple;
            Assert.Equal(24000, simple.Bitrate);
            client.HandleMessage(ControlMessage.Parse("{\"type\":\"BitrateCommand\",\"data\":{\"target_bitrate_kbps\":12,\"reason\":\"jitter\",\"expected_loss_percent\":0}}"));
            Assert.Equal(12000, simple.Bitrate);
            client.Dispose();
        }

        private static float[] Tone(int frames, int channels = 1, float hz = 440f)
        {
            var pcm = new float[frames * channels];
            for (int i = 0; i < frames; i++)
            {
                float v = 0.5f * MathF.Sin(2f * MathF.PI * hz * i / AudioFormat.SampleRate);
                for (int c = 0; c < channels; c++) pcm[i * channels + c] = v;
            }
            return pcm;
        }

        private static float Rms(ReadOnlySpan<float> pcm)
        {
            double acc = 0;
            foreach (var v in pcm) acc += v * v;
            return (float)Math.Sqrt(acc / Math.Max(1, pcm.Length));
        }

        /// <summary>Common contract for every codec implementation: controls round-trip, audio survives, FEC beats PLC.</summary>
        private static void ExerciseCodec<T>(Func<OpusEncoderSettings, T> make) where T : IOpusCodec, IOpusEncoderControls, IOpusFecDecoder
        {
            var settings = OpusEncoderSettings.Default;
            settings.BitrateBps = 24000;
            settings.MaxBandwidth = OpusBandwidth.Wideband;
            settings.Complexity = 5;
            settings.Fec = true;
            settings.ExpectedLossPercent = 20;
            using var codec = make(settings);
            Assert.Equal(settings, codec.Settings);

            // Out-of-range values are clamped, not rejected.
            var wild = settings;
            wild.BitrateBps = 1_000_000;
            wild.Complexity = 99;
            codec.Apply(wild);
            Assert.Equal(OpusEncoderSettings.MaxBitrate, codec.Settings.BitrateBps);
            Assert.Equal(10, codec.Settings.Complexity);
            codec.SetBitrate(24000);
            Assert.Equal(24000, codec.Settings.BitrateBps);
            Assert.Equal(OpusBandwidth.Wideband, codec.Settings.MaxBandwidth); // SetBitrate keeps the rest
            codec.Apply(settings);

            // Encode a run of frames; drop one and rebuild it from the next packet's FEC.
            const int n = 8;
            var tone = Tone(AudioFormat.FrameSamples * n);
            var packets = new byte[n][];
            var buf = new byte[NativeOpusCodec.MaxPacketBytes];
            for (int i = 0; i < n; i++)
            {
                int len = codec.Encode(new ReadOnlySpan<float>(tone, i * AudioFormat.FrameSamples, AudioFormat.FrameSamples), AudioFormat.FrameSamples, buf);
                Assert.InRange(len, 1, buf.Length);
                packets[i] = buf.AsSpan(0, len).ToArray();
            }
            var pcm = new float[AudioFormat.FrameSamples * 3];
            for (int i = 0; i < 4; i++) Assert.Equal(AudioFormat.FrameSamples, codec.Decode(packets[i], pcm, pcm.Length));
            Assert.InRange(Rms(pcm.AsSpan(0, AudioFormat.FrameSamples)), 0.25f, 0.45f); // 0.5 * 1/sqrt(2) ≈ 0.354

            // Frame 4 lost: FEC from packet 5 yields a full frame of real signal …
            Assert.Equal(AudioFormat.FrameSamples, codec.DecodeFec(packets[5], pcm, AudioFormat.FrameSamples));
            Assert.InRange(Rms(pcm.AsSpan(0, AudioFormat.FrameSamples)), 0.15f, 0.5f);
            Assert.Equal(AudioFormat.FrameSamples, codec.Decode(packets[5], pcm, pcm.Length));
            // … and plain PLC is still available.
            Assert.Equal(AudioFormat.FrameSamples, codec.DecodeLost(pcm, AudioFormat.FrameSamples));
            Assert.Equal(AudioFormat.FrameSamples, codec.Decode(packets[7], pcm, pcm.Length));

            // Without FEC in the stream, FEC decoding still returns a concealed frame rather than failing.
            var noFec = settings;
            noFec.Fec = false;
            codec.Apply(noFec);
            int len2 = codec.Encode(new ReadOnlySpan<float>(tone, 0, AudioFormat.FrameSamples), AudioFormat.FrameSamples, buf);
            Assert.Equal(AudioFormat.FrameSamples, codec.DecodeFec(buf.AsSpan(0, len2).ToArray(), pcm, AudioFormat.FrameSamples));

            // DTX + CBR + narrowband all apply without error and silence encodes to a tiny packet.
            var dtx = settings;
            dtx.Dtx = true; dtx.Vbr = false; dtx.MaxBandwidth = OpusBandwidth.Narrowband; dtx.Signal = OpusSignal.Music;
            codec.Apply(dtx);
            Assert.Equal(dtx, codec.Settings);
            var silence = new float[AudioFormat.FrameSamples];
            int last = 0;
            for (int i = 0; i < 20; i++) last = codec.Encode(silence, AudioFormat.FrameSamples, buf);
            Assert.InRange(last, 0, 2); // DTX: 1–2 byte "no update" packets (or nothing)
        }

        [Fact]
        public void ConcentusCodecHonoursAllControlsAndFec()
        {
            ExerciseCodec(s => new ConcentusOpusCodec(AudioFormat.SampleRate, 1, s));
            // Legacy constructor keeps working (bitrate only, FEC on by default).
            using var legacy = new ConcentusOpusCodec(bitrateBps: 20000);
            Assert.Equal(20000, legacy.Settings.BitrateBps);
            Assert.True(legacy.Settings.Fec);
        }

        [Fact]
        public void NativeCodecHonoursAllControlsAndFec()
        {
            if (!NativeLib.TryLoad() || !NativeOpusCodec.IsAvailable)
            {
                // No native build around (e.g. SDK-only CI job): the pure C# path above is the contract.
                Assert.NotEqual("1", Environment.GetEnvironmentVariable("AURIX_REQUIRE_NATIVE"));
                Assert.False(NativeOpusCodec.IsAvailable);
                return;
            }
            ExerciseCodec(s => new NativeOpusCodec(AudioFormat.SampleRate, 1, s));
            Assert.Throws<ArgumentOutOfRangeException>(() => new NativeOpusCodec(AudioFormat.SampleRate, 3));
            Assert.Throws<InvalidOperationException>(() => new NativeOpusCodec(44100, 1));
            using var stereo = new NativeOpusCodec(AudioFormat.SampleRate, 2);
            var buf = new byte[NativeOpusCodec.MaxPacketBytes];
            int len = stereo.Encode(Tone(AudioFormat.FrameSamples, 2), AudioFormat.FrameSamples, buf);
            Assert.InRange(len, 1, buf.Length);
            var pcm = new float[AudioFormat.FrameSamples * 2];
            Assert.Equal(AudioFormat.FrameSamples, stereo.Decode(buf.AsSpan(0, len).ToArray(), pcm, AudioFormat.FrameSamples));
            Assert.Throws<ArgumentException>(() => stereo.Encode(new float[10], AudioFormat.FrameSamples, buf));
        }

        /// <summary>Stub with FEC: a lost frame is "recovered" at a distinct level so the mixer path is observable.</summary>
        private sealed class FecCodec : IOpusCodec, IOpusFecDecoder
        {
            public const float Level = 0.5f, FecLevel = 0.25f;
            public int SampleRate => AudioFormat.SampleRate;
            public int Channels => 1;
            public int Encode(ReadOnlySpan<float> pcm, int frameSamplesPerChannel, Span<byte> output) => 0;
            public int Decode(ReadOnlySpan<byte> opus, Span<float> pcm, int maxFrameSamplesPerChannel) { pcm.Slice(0, AudioFormat.FrameSamples).Fill(Level); return AudioFormat.FrameSamples; }
            public int DecodeLost(Span<float> pcm, int frameSamplesPerChannel) { pcm.Slice(0, frameSamplesPerChannel).Fill(0f); return frameSamplesPerChannel; }
            public int DecodeFec(ReadOnlySpan<byte> nextPacket, Span<float> pcm, int frameSamplesPerChannel)
            {
                if (nextPacket[0] == 0xFF) return 0; // "no FEC data in this packet"
                pcm.Slice(0, frameSamplesPerChannel).Fill(FecLevel);
                return frameSamplesPerChannel;
            }
            public void SetBitrate(int bitsPerSecond) { }
            public void Dispose() { }
        }

        [Fact]
        public void RemoteMixerRecoversLostFramesFromFecWhenTheNextPacketIsAlreadyBuffered()
        {
            var mixer = new RemoteMixer(() => new FecCodec());
            var mono = new float[AudioFormat.FrameSamples];
            // 0, 1, (2 lost), 3, (4 lost), 5 without FEC payload, 6, then nothing.
            foreach (uint seq in new uint[] { 0, 1, 3 }) mixer.Push(9, seq, 1f, new byte[] { 1 });
            mixer.Push(9, 5, 1f, new byte[] { 0xFF });
            mixer.Push(9, 6, 1f, new byte[] { 1 });
            var levels = new List<float>();
            for (int i = 0; i < 7; i++) { Array.Clear(mono, 0, mono.Length); mixer.Mix(mono, 1); levels.Add(mono[0]); }
            Assert.Equal(new[] { FecCodec.Level, FecCodec.Level, FecCodec.FecLevel, FecCodec.Level, 0f, FecCodec.Level, FecCodec.Level }, levels.ToArray());
            var t = mixer.Totals;
            Assert.Equal(2, t.Lost);
            Assert.Equal(1, t.FecRecovered);

            // A codec without FEC support just conceals.
            var plain = new RemoteMixer(() => new FecCodecless());
            foreach (uint seq in new uint[] { 0, 1, 3 }) plain.Push(9, seq, 1f, new byte[] { 1 });
            for (int i = 0; i < 3; i++) { Array.Clear(mono, 0, mono.Length); plain.Mix(mono, 1); }
            Assert.Equal(0f, mono[0]);
            Assert.Equal(1, plain.Totals.Lost);
            Assert.Equal(0, plain.Totals.FecRecovered);
        }

        private sealed class FecCodecless : IOpusCodec
        {
            public int SampleRate => AudioFormat.SampleRate;
            public int Channels => 1;
            public int Encode(ReadOnlySpan<float> pcm, int frameSamplesPerChannel, Span<byte> output) => 0;
            public int Decode(ReadOnlySpan<byte> opus, Span<float> pcm, int maxFrameSamplesPerChannel) { pcm.Slice(0, AudioFormat.FrameSamples).Fill(0.5f); return AudioFormat.FrameSamples; }
            public int DecodeLost(Span<float> pcm, int frameSamplesPerChannel) { pcm.Slice(0, frameSamplesPerChannel).Fill(0f); return frameSamplesPerChannel; }
            public void SetBitrate(int bitsPerSecond) { }
            public void Dispose() { }
        }
    }
}
