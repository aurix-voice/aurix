using System;
using Aurix.Audio;
using Aurix.Protocol;
using Xunit;

namespace Aurix.Voice.Tests
{
    public class PcmuTests
    {
        [Fact]
        public void G711MatchesTheServerReferenceVectors()
        {
            Assert.Equal(0xFF, G711.UlawEncode(0));
            Assert.Equal(0x7F, G711.UlawEncode(-1));
            Assert.Equal(0x80, G711.UlawEncode(32767));
            Assert.Equal(0x00, G711.UlawEncode(-32768));
            Assert.Equal(0xFE, G711.UlawEncode(8));
            Assert.Equal(0xCE, G711.UlawEncode(1000));
            Assert.Equal(0, G711.UlawDecode(0xFF));
            Assert.Equal(32124, G711.UlawDecode(0x80));
            Assert.Equal(-32124, G711.UlawDecode(0x00));
            Assert.Equal(988, G711.UlawDecode(0xCE));

            // Every code point survives decode → encode (0x7F is "negative zero", which re-encodes as 0xFF).
            for (int b = 0; b < 256; b++)
            {
                if (b == 0x7F) continue;
                Assert.Equal((byte)b, G711.UlawEncode(G711.UlawDecode((byte)b)));
            }
            Assert.Equal(0, G711.UlawDecode(0x7F));

            var pcm = new float[] { 0f, 0.5f, -0.5f, 1f, -1f, 2f };
            var ulaw = new byte[pcm.Length];
            var back = new float[pcm.Length];
            G711.Encode(pcm, ulaw);
            G711.Decode(ulaw, back);
            Assert.Equal(0xFF, ulaw[0]);
            Assert.Equal(0x80, ulaw[3]);
            Assert.Equal(0x80, ulaw[5]); // clipped
            for (int i = 1; i < 5; i++) Assert.InRange(back[i] / Math.Clamp(pcm[i], -1f, 1f), 0.93, 1.07);
            Assert.True(G711.IsValidFrame(160));
            Assert.False(G711.IsValidFrame(100));
        }

        [Fact]
        public void PcmuCodecDecimatesTo8kHzAndRestoresTheTone()
        {
            var codec = new PcmuCodec();
            var frame = new float[AudioFormat.FrameSamples];
            var ulaw = new byte[G711.FrameSamples];
            var back = new float[AudioFormat.FrameSamples];
            double energyIn = 0, energyOut = 0;
            int t = 0;
            for (int f = 0; f < 10; f++)
            {
                for (int i = 0; i < frame.Length; i++, t++) frame[i] = 0.3f * MathF.Sin(2f * MathF.PI * 440f * t / AudioFormat.SampleRate);
                Assert.Equal(G711.FrameSamples, codec.Encode(frame, AudioFormat.FrameSamples, ulaw));
                Assert.Equal(AudioFormat.FrameSamples, codec.Decode(ulaw, back, AudioFormat.FrameSamples));
                if (f < 2) continue; // FIR warm-up
                foreach (var s in frame) energyIn += s * s;
                foreach (var s in back) energyOut += s * s;
            }
            double rmsIn = Math.Sqrt(energyIn / (8 * frame.Length)), rmsOut = Math.Sqrt(energyOut / (8 * frame.Length));
            Assert.InRange(rmsIn, 0.21, 0.22);
            Assert.InRange(rmsOut, rmsIn * 0.9, rmsIn * 1.1);

            // A 10 kHz tone is above the telephone band: the anti-aliasing filter removes it.
            for (int i = 0; i < frame.Length; i++) frame[i] = 0.5f * MathF.Sin(2f * MathF.PI * 10000f * i / AudioFormat.SampleRate);
            codec.Reset();
            for (int f = 0; f < 3; f++) codec.Encode(frame, AudioFormat.FrameSamples, ulaw);
            codec.Decode(ulaw, back, AudioFormat.FrameSamples);
            Assert.True(AudioLevel.Rms(back, back.Length) < 0.02f);

            // Only 10/20/40/60 ms frames decode; concealment fades the last sample to silence.
            Assert.Equal(0, codec.Decode(new byte[100], back, AudioFormat.FrameSamples));
            Assert.Equal(480, codec.Decode(new byte[80], back, AudioFormat.FrameSamples));
            Assert.Equal(AudioFormat.FrameSamples, codec.DecodeLost(back, AudioFormat.FrameSamples));
            Assert.InRange(Math.Abs(back[back.Length - 1]), 0f, 0.002f);
            Assert.InRange(Math.Abs(back[0]), 0.9f, 1f);
        }

        /// <summary>Decoder stub standing in for Opus: every frame decodes to a constant level.</summary>
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

        [Fact]
        public void RemoteMixerDecodesPcmuStreamsNextToOpusOnes()
        {
            var mixer = new RemoteMixer(() => new ConstantCodec());
            // μ-law DC at +0.25 (code point for 8192) — the FIR passes DC with unity gain.
            var dc = new byte[G711.FrameSamples];
            Array.Fill(dc, G711.UlawEncode(8192));
            for (uint seq = 0; seq < 12; seq++) mixer.Push(0xA11CE, seq, 1f, null, AudioCodec.Opus, new byte[] { 1 });
            for (uint seq = 0; seq < 12; seq++) mixer.Push(0xB0B, seq, 0.5f, null, AudioCodec.Pcmu, dc);

            var mono = new float[AudioFormat.FrameSamples];
            for (int i = 0; i < 4; i++) { Array.Clear(mono, 0, mono.Length); mixer.Mix(mono, 1); }
            // Opus stub 0.5 + PCMU 0.25 × volume 0.5 (per-participant volume applies to both codecs).
            Assert.InRange(mono[mono.Length - 1], ConstantCodec.Level + 0.25f * 0.5f - 0.01f, ConstantCodec.Level + 0.25f * 0.5f + 0.01f);

            // The same SSRC switching codecs (session re-negotiated) swaps the decoder instead of feeding μ-law to Opus.
            for (uint seq = 12; seq < 20; seq++) mixer.Push(0xB0B, seq, 1f, null, AudioCodec.Opus, new byte[] { 1 });
            for (int i = 0; i < 6; i++) { Array.Clear(mono, 0, mono.Length); mixer.Mix(mono, 1); }
            Assert.Equal(ConstantCodec.Level * 2f, mono[mono.Length - 1], 2);

            // Legacy Opus-only overloads still work.
            var legacy = new RemoteMixer(() => new ConstantCodec());
            for (uint seq = 0; seq < 4; seq++) legacy.Push(1, seq, 1f, new byte[] { 1 });
            Array.Clear(mono, 0, mono.Length);
            legacy.Mix(mono, 1);
            Assert.Equal(ConstantCodec.Level, mono[0], 4);
        }

        [Fact]
        public void PcmuFramesAreFlaggedOnTheWireAndNegotiatedOverControl()
        {
            var pkt = AurxPacket.Audio(1, 960, 7, 0xC0FFEE, new byte[G711.FrameSamples]);
            pkt.Header.Flags |= PacketFlags.Pcmu;
            Assert.True(AurxPacket.TryDecode(pkt.Encode(), out var decoded, out var err), err);
            Assert.Equal(PacketFlags.Pcmu, decoded.Header.Flags & PacketFlags.Pcmu);
            Assert.Equal((ushort)0x2000, (ushort)PacketFlags.Pcmu);

            Assert.Equal("{\"type\":\"SetAudioCodec\",\"data\":{\"codec\":\"pcmu\"}}", ControlMessage.SetAudioCodec(AudioCodec.Pcmu));
            Assert.Equal("{\"type\":\"SetAudioCodec\",\"data\":{\"codec\":\"opus\"}}", ControlMessage.SetAudioCodec(AudioCodec.Opus));
            Assert.Equal(AudioCodec.Pcmu, ControlMessage.Parse("{\"type\":\"AudioCodecChanged\",\"data\":{\"codec\":\"pcmu\"}}").AudioCodec());
            Assert.Equal(AudioCodec.Opus, ControlMessage.Parse("{\"type\":\"AudioCodecChanged\",\"data\":{\"codec\":\"opus\"}}").AudioCodec());
            Assert.Equal(AudioCodec.Pcmu, ControlMessage.Parse("{\"type\":\"ReceiverPreferences\",\"data\":{\"codec\":\"pcmu\"}}").ReceiverPreferences().Codec);
            // Older servers omit the field: Opus.
            Assert.Equal(AudioCodec.Opus, ControlMessage.Parse("{\"type\":\"ReceiverPreferences\",\"data\":{}}").ReceiverPreferences().Codec);
        }
    }
}
