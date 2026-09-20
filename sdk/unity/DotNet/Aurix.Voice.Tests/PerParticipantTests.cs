using System;
using System.Collections.Generic;
using Aurix.Audio;
using Aurix.Protocol;
using Xunit;

namespace Aurix.Voice.Tests
{
    /// <summary>Per-participant PCM pull for engine spatialization next to the aggregate mix.</summary>
    public class PerParticipantTests
    {
        private const int N = AudioFormat.FrameSamples;

        /// <summary>Mono decoder whose level is the payload byte / 100 (levels keep the Opus stereo TOC bit 0x04 clear).</summary>
        private sealed class LevelCodec : IOpusCodec
        {
            public int SampleRate => AudioFormat.SampleRate;
            public int Channels => 1;
            public int Encode(ReadOnlySpan<float> pcm, int frameSamplesPerChannel, Span<byte> output) => 0;
            public int Decode(ReadOnlySpan<byte> opus, Span<float> pcm, int maxFrameSamplesPerChannel)
            {
                pcm.Slice(0, N).Fill(opus[0] / 100f);
                return N;
            }
            public int DecodeLost(Span<float> pcm, int frameSamplesPerChannel) { pcm.Slice(0, frameSamplesPerChannel).Fill(0f); return frameSamplesPerChannel; }
            public void SetBitrate(int bitsPerSecond) { }
            public void Dispose() { }
        }

        /// <summary>Stereo decoder (payload = stereo TOC, level): L = +level, R = −level.</summary>
        private sealed class StereoLevelCodec : IOpusCodec
        {
            public int SampleRate => AudioFormat.SampleRate;
            public int Channels => 2;
            public int Encode(ReadOnlySpan<float> pcm, int frameSamplesPerChannel, Span<byte> output) => 0;
            public int Decode(ReadOnlySpan<byte> opus, Span<float> pcm, int maxFrameSamplesPerChannel)
            {
                float level = opus[1] / 100f;
                for (int i = 0; i < N; i++) { pcm[i * 2] = level; pcm[i * 2 + 1] = -level; }
                return N;
            }
            public int DecodeLost(Span<float> pcm, int frameSamplesPerChannel) { pcm.Slice(0, frameSamplesPerChannel * 2).Fill(0f); return frameSamplesPerChannel; }
            public void SetBitrate(int bitsPerSecond) { }
            public void Dispose() { }
        }

        private static void Feed(RemoteMixer mixer, uint ssrc, byte level, Direction? direction = null, int frames = 12, float volume = 1f)
        {
            for (uint seq = 0; seq < frames; seq++) mixer.Push(ssrc, seq, volume, direction, AudioCodec.Opus, false, new byte[] { level });
        }

        [Fact]
        public void PullReturnsOnlyTheSelectedParticipantUnpanned()
        {
            var mixer = new RemoteMixer(() => new LevelCodec());
            // Alice hard left (server direction), Bob centred, Carol's TTS voice on her synthesized SSRC.
            Feed(mixer, 1, 16, new Direction(-MathF.PI / 2, 0f));
            Feed(mixer, 2, 40);
            Feed(mixer, 3 | AurxPacket.SynthSsrcFlag, 8);

            var stereo = new float[N * 2];
            int got = mixer.PullParticipant(1, stereo, 0, N, 2);
            Assert.Equal(N, got);
            // Engine spatialization: no local panning, both channels carry the plain signal.
            Assert.Equal(0.16f, stereo[0], 3);
            Assert.Equal(0.16f, stereo[1], 3);
            Assert.Equal(0.16f, stereo[stereo.Length - 1], 3);
            // …whereas the aggregate mix would have panned her hard left.
            // The TTS voice is reachable through the participant's base SSRC.
            var mono = new float[N];
            Assert.Equal(N, mixer.PullParticipant(3, mono, 0, N, 1));
            Assert.Equal(0.08f, mono[N - 1], 3);

            // Unknown participant: silence, 0 frames.
            Array.Fill(mono, 0.5f);
            Assert.Equal(0, mixer.PullParticipant(99, mono, 0, N, 1));
            Assert.All(mono, v => Assert.Equal(0f, v));

            // Drain Alice through pulls; the aggregate mix afterwards has Bob (never pulled) and Carol's
            // remainder but none of Alice: pulling consumes, nothing is heard twice.
            for (int i = 1; i < 12; i++) Assert.Equal(N, mixer.PullParticipant(1, stereo, 0, N, 2));
            Assert.Equal(0, mixer.PullParticipant(1, stereo, 0, N, 2));
            var mix = new float[N * 2];
            mixer.Mix(mix, 2);
            Assert.Equal(0.48f, mix[mix.Length - 2], 2);
            Assert.Equal(0.48f, mix[mix.Length - 1], 2);
        }

        [Fact]
        public void MixWithExcludedStreamsLeavesThemForTheirOwnPull()
        {
            var mixer = new RemoteMixer(() => new LevelCodec());
            Feed(mixer, 1, 16);
            Feed(mixer, 1 | AurxPacket.SynthSsrcFlag, 8);
            Feed(mixer, 2, 40);

            var claimed = new HashSet<uint> { 1, 1 | AurxPacket.SynthSsrcFlag };
            var mix = new float[N];
            mixer.Mix(mix, 0, N, 1, claimed);
            Assert.Equal(0.40f, mix[N - 1], 3); // Bob only

            var alice = new float[N];
            Assert.Equal(N, mixer.PullParticipant(1, alice, 0, N, 1));
            Assert.Equal(0.24f, alice[N - 1], 3); // microphone + TTS voice, nothing lost to the mix

            // PerParticipant fallback: an unbound talker is still heard through the aggregate.
            claimed.Clear();
            Array.Clear(mix, 0, mix.Length);
            mixer.Mix(mix, 0, N, 1, claimed);
            Assert.Equal(0.64f, mix[N - 1], 2);
        }

        [Fact]
        public void PullKeepsStereoImageAndDownmixesForMono()
        {
            var mixer = new RemoteMixer(() => new LevelCodec(), () => new StereoLevelCodec());
            const uint music = 7;
            for (uint seq = 0; seq < 12; seq++)
                mixer.Push(new Transport.IncomingAudio { SenderSsrc = music, Sequence = seq, Volume = 1f, Codec = AudioCodec.Opus, Mixed = false, Payload = new byte[] { 0x04, 30 } }); // stereo TOC
            var infos = new List<StreamInfo>();
            mixer.GetStreams(infos);
            Assert.Single(infos);
            Assert.Equal(music, infos[0].Ssrc);
            Assert.False(infos[0].Synthesized);
            Assert.True(infos[0].Stereo);
            Assert.False(infos[0].Mixed);

            // Stereo output keeps the image (no downmix, no panning)…
            var stereo = new float[N * 2];
            Assert.Equal(N, mixer.PullParticipant(music, stereo, 0, N, 2));
            Assert.Equal(0.30f, stereo[0], 3);
            Assert.Equal(-0.30f, stereo[1], 3);

            // …mono output gets (L + R) / 2.
            var mono = new float[N];
            Assert.Equal(N, mixer.PullParticipant(music, mono, 0, N, 1));
            Assert.Equal(0f, mono[N - 1], 3);
        }

        [Fact]
        public void VolumeMuteAndRemovalApplyToPulledStreams()
        {
            var mixer = new RemoteMixer(() => new LevelCodec());
            Feed(mixer, 1, 40, volume: 0.5f); // server-side gain (per-participant volume / attenuation)
            mixer.OutputVolume = 0.5f;

            var mono = new float[N];
            Assert.Equal(N, mixer.PullParticipant(1, mono, 0, N, 1));
            Assert.Equal(0.10f, mono[N - 1], 3);

            mixer.OutputMuted = true;
            Assert.Equal(N, mixer.PullParticipant(1, mono, 0, N, 1)); // still consumed, keeps the jitter buffer flowing
            Assert.Equal(0f, mono[N - 1], 3);
            mixer.OutputMuted = false;

            mixer.Remove(1);
            Assert.Equal(0, mixer.PullParticipant(1, mono, 0, N, 1));
            var infos = new List<StreamInfo>();
            mixer.GetStreams(infos);
            Assert.Empty(infos);
        }

        [Fact]
        public void RenderRateConverterTracksAToneAcrossBlocks()
        {
            // 44.1 kHz device blocks → 48 kHz reference: a 1 kHz tone must stay continuous at the block edges.
            var conv = new RenderRateConverter();
            const int inRate = 44100;
            var output = new List<float>();
            var scratch = new float[RenderRateConverter.MaxOutputSamples(441, 1, inRate)];
            int n = 0;
            for (int block = 0; block < 20; block++)
            {
                int len = block % 2 == 0 ? 441 : 300;
                var input = new float[len];
                for (int i = 0; i < len; i++, n++) input[i] = MathF.Sin(2 * MathF.PI * 1000f * n / inRate);
                int produced = conv.Convert(input, 1, inRate, scratch);
                for (int i = 0; i < produced; i++) output.Add(scratch[i]);
            }
            double expectedFrames = n * 48000.0 / inRate;
            Assert.InRange(output.Count, expectedFrames - 3, expectedFrames + 1);
            // Compare against the ideal tone at 48 kHz; linear interpolation of a 1 kHz tone is accurate to ~1 %.
            float maxErr = 0f;
            for (int i = 0; i < output.Count; i++)
            {
                float ideal = MathF.Sin(2 * MathF.PI * 1000f * i / 48000f);
                maxErr = MathF.Max(maxErr, MathF.Abs(output[i] - ideal));
            }
            Assert.True(maxErr < 0.02f, $"max error {maxErr}");

            // 48 kHz passes through untouched.
            var same = new float[96];
            for (int i = 0; i < same.Length; i++) same[i] = i;
            var copy = new float[96];
            Assert.Equal(96, new RenderRateConverter().Convert(same, 2, 48000, copy));
            Assert.Equal(same, copy);
        }
    }
}
