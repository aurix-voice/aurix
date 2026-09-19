using System;

namespace Aurix.Audio
{
    /// <summary>
    /// Opus codec abstraction. The SDK does not bundle an Opus implementation (Unity has none built
    /// in and licensing/platform choices differ per project). Plug in either:
    /// <list type="bullet">
    /// <item><see cref="NativeOpusCodec"/> — libopus statically linked into the Aurix native core
    /// (<c>aurix_client</c> shared library, one binary per platform; every encoder control);</item>
    /// <item>Concentus (pure C#, MIT) — see <c>Samples~/Concentus/ConcentusOpusCodec.cs</c>;</item>
    /// <item>UnityOpus or any other wrapper.</item>
    /// </list>
    /// The server expects 48 kHz Opus frames; mono or stereo, 20 ms (960 samples) frames recommended.
    /// Implement <see cref="IOpusEncoderControls"/> as well to receive the full channel policy
    /// (complexity, bandwidth, FEC, DTX, …) instead of just the bitrate, and
    /// <see cref="IOpusFecDecoder"/> to let the jitter buffer recover lost frames from in-band FEC.
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

    /// <summary>
    /// Optional: a codec that accepts the whole <see cref="OpusEncoderSettings"/> set. The SDK calls
    /// <see cref="Apply"/> instead of <see cref="IOpusCodec.SetBitrate"/> when the codec implements it.
    /// </summary>
    public interface IOpusEncoderControls
    {
        /// <summary>Settings the encoder is running with (as clamped by the implementation).</summary>
        OpusEncoderSettings Settings { get; }

        /// <summary>Apply new settings; takes effect from the next frame. Unsupported fields are ignored.</summary>
        void Apply(OpusEncoderSettings settings);
    }

    /// <summary>
    /// Optional: a decoder that can rebuild a lost frame from the in-band FEC data of the packet that
    /// follows it. Falls back to <see cref="IOpusCodec.DecodeLost"/> (PLC) when absent.
    /// </summary>
    public interface IOpusFecDecoder
    {
        /// <summary>
        /// Decode the FEC copy of the *previous* frame carried by <paramref name="nextPacket"/> into
        /// <paramref name="pcm"/>. Returns samples-per-channel written (the sender's frame size, so
        /// <paramref name="frameSamplesPerChannel"/> must match the stream's frame duration).
        /// </summary>
        int DecodeFec(ReadOnlySpan<byte> nextPacket, Span<float> pcm, int frameSamplesPerChannel);
    }

    /// <summary>Constants shared by the capture/playback helpers.</summary>
    public static class AudioFormat
    {
        public const int SampleRate = 48000;
        public const int FrameMs = 20;
        public const int FrameSamples = SampleRate * FrameMs / 1000; // 960
    }
}
