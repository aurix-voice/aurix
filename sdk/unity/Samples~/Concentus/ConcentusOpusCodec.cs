using System;
using Aurix.Audio;
using Concentus;
using OpusApplication = Concentus.Enums.OpusApplication;

namespace Aurix.Samples
{
    /// <summary>
    /// <see cref="IOpusCodec"/> backed by Concentus (pure C# Opus, MIT). Add the NuGet package
    /// <c>Concentus</c> 2.x (or drop its DLL into Plugins/) and pass <c>() => new ConcentusOpusCodec()</c>
    /// as the codec factory. Honours the full <see cref="OpusEncoderSettings"/> set and rebuilds lost
    /// frames from in-band FEC (<see cref="IOpusFecDecoder"/>). Roughly 5–10× the CPU of libopus per
    /// stream; keep <see cref="OpusEncoderSettings.Complexity"/> ≤ 5 on mobile.
    /// </summary>
    public sealed class ConcentusOpusCodec : IOpusCodec, IOpusEncoderControls, IOpusFecDecoder
    {
        private readonly IOpusEncoder _encoder;
        private readonly IOpusDecoder _decoder;
        private readonly object _encLock = new object();
        private OpusEncoderSettings _settings;

        public int SampleRate { get; }
        public int Channels { get; }

        public ConcentusOpusCodec(int sampleRate = AudioFormat.SampleRate, int channels = 1, int bitrateBps = 32000)
            : this(sampleRate, channels, WithBitrate(OpusEncoderSettings.Default, bitrateBps)) { }

        public ConcentusOpusCodec(int sampleRate, int channels, OpusEncoderSettings settings)
        {
            SampleRate = sampleRate;
            Channels = channels;
            _encoder = OpusCodecFactory.CreateEncoder(sampleRate, channels, Application(settings.Signal));
            _decoder = OpusCodecFactory.CreateDecoder(sampleRate, channels);
            Apply(settings);
        }

        private static OpusEncoderSettings WithBitrate(OpusEncoderSettings s, int bitrateBps)
        {
            s.BitrateBps = bitrateBps;
            return s;
        }

        /// <summary>Settings the encoder is running with (clamped to what libopus accepts).</summary>
        public OpusEncoderSettings Settings { get { lock (_encLock) return _settings; } }

        public void Apply(OpusEncoderSettings settings)
        {
            var s = settings.Clamped();
            lock (_encLock)
            {
                _encoder.Application = Application(s.Signal);
                _encoder.SignalType = Signal(s.Signal);
                _encoder.MaxBandwidth = Bandwidth(s.MaxBandwidth);
                _encoder.Complexity = s.Complexity;
                _encoder.UseVBR = s.Vbr;
                _encoder.UseConstrainedVBR = s.ConstrainedVbr;
                _encoder.UseInbandFEC = s.Fec;
                _encoder.PacketLossPercent = s.ExpectedLossPercent;
                _encoder.UseDTX = s.Dtx;
                _encoder.Bitrate = s.BitrateBps;
                _settings = s;
            }
        }

        public void SetBitrate(int bitsPerSecond)
        {
            var s = Settings;
            s.BitrateBps = bitsPerSecond;
            Apply(s);
        }

        public int Encode(ReadOnlySpan<float> pcm, int frameSamplesPerChannel, Span<byte> output)
        {
            lock (_encLock) return _encoder.Encode(pcm, frameSamplesPerChannel, output, output.Length);
        }

        public int Decode(ReadOnlySpan<byte> opus, Span<float> pcm, int maxFrameSamplesPerChannel) =>
            _decoder.Decode(opus, pcm, maxFrameSamplesPerChannel, false);

        public int DecodeLost(Span<float> pcm, int frameSamplesPerChannel) =>
            _decoder.Decode(ReadOnlySpan<byte>.Empty, pcm, frameSamplesPerChannel, false);

        public int DecodeFec(ReadOnlySpan<byte> nextPacket, Span<float> pcm, int frameSamplesPerChannel) =>
            _decoder.Decode(nextPacket, pcm, frameSamplesPerChannel, true);

        private static OpusApplication Application(OpusSignal s) =>
            s == OpusSignal.Music ? OpusApplication.OPUS_APPLICATION_AUDIO : OpusApplication.OPUS_APPLICATION_VOIP;

        private static Concentus.Enums.OpusSignal Signal(OpusSignal s)
        {
            switch (s)
            {
                case OpusSignal.Voice: return Concentus.Enums.OpusSignal.OPUS_SIGNAL_VOICE;
                case OpusSignal.Music: return Concentus.Enums.OpusSignal.OPUS_SIGNAL_MUSIC;
                default: return Concentus.Enums.OpusSignal.OPUS_SIGNAL_AUTO;
            }
        }

        private static Concentus.Enums.OpusBandwidth Bandwidth(OpusBandwidth b)
        {
            switch (b)
            {
                case OpusBandwidth.Narrowband: return Concentus.Enums.OpusBandwidth.OPUS_BANDWIDTH_NARROWBAND;
                case OpusBandwidth.Mediumband: return Concentus.Enums.OpusBandwidth.OPUS_BANDWIDTH_MEDIUMBAND;
                case OpusBandwidth.Wideband: return Concentus.Enums.OpusBandwidth.OPUS_BANDWIDTH_WIDEBAND;
                case OpusBandwidth.Superwideband: return Concentus.Enums.OpusBandwidth.OPUS_BANDWIDTH_SUPERWIDEBAND;
                default: return Concentus.Enums.OpusBandwidth.OPUS_BANDWIDTH_FULLBAND;
            }
        }

        public void Dispose() { }
    }
}
