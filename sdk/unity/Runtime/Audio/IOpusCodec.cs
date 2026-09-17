using System;

namespace Aurix.Audio
{
    /// <summary>
    /// Opus codec abstraction. The SDK does not bundle an Opus implementation (Unity has none built
    /// in and licensing/platform choices differ per project). Plug in either:
    /// <list type="bullet">
    /// <item>Concentus (pure C#, MIT) — see <c>Samples~/Concentus/ConcentusOpusCodec.cs</c>;</item>
    /// <item>libopus via P/Invoke (fastest, needs native binaries per platform);</item>
    /// <item>UnityOpus or any other wrapper.</item>
    /// </list>
    /// The server expects 48 kHz Opus frames; mono or stereo, 20 ms (960 samples) frames recommended.
    /// </summary>
    public interface IOpusCodec : IDisposable
    {
        int SampleRate { get; }
        int Channels { get; }

        /// <summary>Encode one frame of interleaved PCM (float, -1..1). Returns the number of bytes written to <paramref name="output"/>.</summary>
        int Encode(ReadOnlySpan<float> pcm, int frameSamplesPerChannel, Span<byte> output);

        /// <summary>Decode one Opus frame into interleaved float PCM. Returns samples-per-channel decoded.</summary>
        int Decode(ReadOnlySpan<byte> opus, Span<float> pcm, int maxFrameSamplesPerChannel);

        /// <summary>Packet-loss concealment: synthesize one frame when a packet is missing.</summary>
        int DecodeLost(Span<float> pcm, int frameSamplesPerChannel);

        /// <summary>Change the target bitrate (bits per second); called when the server sends <c>BitrateCommand</c>.</summary>
        void SetBitrate(int bitsPerSecond);
    }

    /// <summary>Constants shared by the capture/playback helpers.</summary>
    public static class AudioFormat
    {
        public const int SampleRate = 48000;
        public const int FrameMs = 20;
        public const int FrameSamples = SampleRate * FrameMs / 1000; // 960
    }
}
