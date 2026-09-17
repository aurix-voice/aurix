using System;
using Aurix.Audio;
using Concentus;
using Concentus.Enums;

namespace Aurix.Samples
{
    /// <summary>
    /// <see cref="IOpusCodec"/> backed by Concentus (pure C# Opus, MIT). Add the NuGet package
    /// <c>Concentus</c> 2.x (or drop its DLL into Plugins/) and pass <c>() => new ConcentusOpusCodec()</c>
    /// as the codec factory.
    /// </summary>
    public sealed class ConcentusOpusCodec : IOpusCodec
    {
        private readonly IOpusEncoder _encoder;
        private readonly IOpusDecoder _decoder;

        public int SampleRate { get; }
        public int Channels { get; }

        public ConcentusOpusCodec(int sampleRate = AudioFormat.SampleRate, int channels = 1, int bitrateBps = 32000)
        {
            SampleRate = sampleRate;
            Channels = channels;
            _encoder = OpusCodecFactory.CreateEncoder(sampleRate, channels, OpusApplication.OPUS_APPLICATION_VOIP);
            _encoder.Bitrate = bitrateBps;
            _encoder.UseInbandFEC = true;
            _decoder = OpusCodecFactory.CreateDecoder(sampleRate, channels);
        }

        public int Encode(ReadOnlySpan<float> pcm, int frameSamplesPerChannel, Span<byte> output) =>
            _encoder.Encode(pcm, frameSamplesPerChannel, output, output.Length);

        public int Decode(ReadOnlySpan<byte> opus, Span<float> pcm, int maxFrameSamplesPerChannel) =>
            _decoder.Decode(opus, pcm, maxFrameSamplesPerChannel, false);

        public int DecodeLost(Span<float> pcm, int frameSamplesPerChannel) =>
            _decoder.Decode(ReadOnlySpan<byte>.Empty, pcm, frameSamplesPerChannel, false);

        public void SetBitrate(int bitsPerSecond) => _encoder.Bitrate = Math.Clamp(bitsPerSecond, 6000, 510000);

        public void Dispose() { }
    }
}
