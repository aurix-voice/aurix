using System;
using Aurix.Audio;
using Xunit;

namespace Aurix.Voice.Tests
{
    /// <summary>Cross-node failover: endpoint rotation and playout re-synchronisation after a takeover.</summary>
    public class FailoverTests
    {
        [Fact]
        public void ReconnectsTryTheSessionNodeFirstThenRotateThroughFailoverNodes()
        {
            var failover = new[] { "wss://b/ws", "wss://c/ws" };
            var order = new string[7];
            for (int a = 1; a <= 7; a++) order[a - 1] = AurixVoiceClient.ReconnectEndpoint("wss://a/ws", failover, a);
            Assert.Equal(new[] { "wss://a/ws", "wss://b/ws", "wss://c/ws", "wss://a/ws", "wss://b/ws", "wss://c/ws", "wss://a/ws" }, order);
            Assert.Equal("wss://a/ws", AurixVoiceClient.ReconnectEndpoint("wss://a/ws", failover, 0));
            for (int a = 1; a <= 4; a++)
            {
                Assert.Equal("wss://a/ws", AurixVoiceClient.ReconnectEndpoint("wss://a/ws", Array.Empty<string>(), a));
                Assert.Equal("wss://a/ws", AurixVoiceClient.ReconnectEndpoint("wss://a/ws", null, a));
            }
        }

        [Fact]
        public void NewClientStartsOnTheConfiguredEndpointWithoutFailoverNodes()
        {
            var client = new AurixVoiceClient("ws://127.0.0.1:1/ws", "token");
            Assert.Equal("ws://127.0.0.1:1/ws", client.Endpoint);
            Assert.Empty(client.FailoverEndpoints);
        }

        [Fact]
        public void JitterBufferResynchronisesOnARestartedSequence()
        {
            var jb = new JitterBuffer(targetDepthFrames: 2);
            for (uint seq = 1000; seq < 1004; seq++) jb.Push(seq, new byte[] { (byte)(seq - 1000) });
            Assert.True(jb.Pop(out var f)); Assert.Equal(0, f[0]);
            Assert.True(jb.Pop(out f)); Assert.Equal(1, f[0]);

            // A real straggler from the current run is still late …
            jb.Push(990, new byte[] { 99 });
            Assert.Equal(1, jb.Late);

            // … but a new node numbering from scratch is a restart, not 1000 late frames.
            jb.Push(7, new byte[] { 7 });
            jb.Push(8, new byte[] { 8 });
            Assert.True(jb.Pop(out f)); Assert.Equal(7, f[0]);
            Assert.True(jb.Pop(out f)); Assert.Equal(8, f[0]);
            Assert.Equal(1, jb.Late);
            Assert.Equal(0, jb.Lost);

            // Forward jumps re-synchronise too (no 900k "lost" slots).
            jb.Push(900_000, new byte[] { 1 });
            jb.Push(900_001, new byte[] { 2 });
            Assert.True(jb.Pop(out f)); Assert.Equal(1, f[0]);
            Assert.Equal(0, jb.Lost);
        }

        [Fact]
        public void MixerResyncDropsBufferedAudioButKeepsStreamsAndVolumes()
        {
            var mixer = new RemoteMixer(() => new ConstantCodec());
            for (uint seq = 500; seq < 512; seq++) mixer.Push(0xA11CE, seq, 0.5f, new byte[] { 1 });
            var frame = new float[AudioFormat.FrameSamples];
            mixer.Mix(frame, 1);
            Assert.Equal(ConstantCodec.Level * 0.5f, frame[0], 4);
            Assert.Equal(1, mixer.ActiveStreams);

            mixer.Resync();
            Assert.Equal(1, mixer.ActiveStreams);
            Array.Clear(frame, 0, frame.Length);
            mixer.Mix(frame, 1);
            Assert.All(frame, v => Assert.Equal(0f, v)); // the old node's frames were flushed

            // The new node numbers from scratch; playout resumes at once at the same volume.
            for (uint seq = 0; seq < 4; seq++) mixer.Push(0xA11CE, seq, 0.5f, new byte[] { 1 });
            Array.Clear(frame, 0, frame.Length);
            mixer.Mix(frame, 1);
            Assert.Equal(ConstantCodec.Level * 0.5f, frame[0], 4);
            Assert.Equal(0, mixer.Totals.Lost);
        }

        private sealed class ConstantCodec : IOpusCodec
        {
            public const float Level = 0.5f;
            public int SampleRate => AudioFormat.SampleRate;
            public int Channels => 1;
            public int Encode(ReadOnlySpan<float> pcm, int frameSamplesPerChannel, Span<byte> output) => 0;
            public int Decode(ReadOnlySpan<byte> opus, Span<float> pcm, int maxFrameSamplesPerChannel)
            {
                pcm.Slice(0, AudioFormat.FrameSamples).Fill(Level);
                return AudioFormat.FrameSamples;
            }
            public int DecodeLost(Span<float> pcm, int frameSamplesPerChannel)
            {
                pcm.Slice(0, frameSamplesPerChannel).Fill(0f);
                return frameSamplesPerChannel;
            }
            public void SetBitrate(int bitsPerSecond) { }
            public void Dispose() { }
        }
    }
}
