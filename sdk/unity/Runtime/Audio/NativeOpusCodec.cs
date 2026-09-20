using System;
using System.Runtime.InteropServices;

namespace Aurix.Audio
{
    /// <summary>
    /// <see cref="IOpusCodec"/> backed by libopus, statically linked into the Aurix native core
    /// (<c>aurix_client</c>: <c>aurix_client.dll</c> / <c>libaurix_client.so</c> / <c>libaurix_client.dylib</c>,
    /// built by <c>sdk/unreal/AurixVoice/build_native.*</c> or <c>cargo build -p aurix-client --release</c>).
    /// Drop the binary for each target into <c>Plugins/&lt;platform&gt;/</c>; on iOS link the static library
    /// (<c>libaurix_client.a</c>) and the symbols resolve through <c>__Internal</c>.
    /// <para>
    /// Supports every control in <see cref="OpusEncoderSettings"/> and FEC recovery of lost frames
    /// (<see cref="IOpusFecDecoder"/>). Nothing here is variadic, so the P/Invoke signatures are
    /// valid on every ABI (a direct <c>opus_encoder_ctl</c> import is not, e.g. on Apple arm64).
    /// Projects that do not ship native binaries keep using Concentus; check
    /// <see cref="IsAvailable"/> to pick at runtime.
    /// </para>
    /// </summary>
    public sealed class NativeOpusCodec : IOpusCodec, IOpusEncoderControls, IOpusFecDecoder
    {
#if UNITY_IOS && !UNITY_EDITOR
        private const string Lib = "__Internal";
#else
        private const string Lib = "aurix_client";
#endif
        /// <summary>Largest packet libopus can produce for one frame.</summary>
        public const int MaxPacketBytes = 1275;

        /// <summary>Blittable mirror of the C <c>AurixEncoderSettings</c> (bools are one byte in C).</summary>
        [StructLayout(LayoutKind.Sequential)]
        private struct NativeSettings
        {
            public uint BitrateBps;
            public byte Complexity;
            public int MaxBandwidth;
            public int Signal;
            public byte Vbr;
            public byte ConstrainedVbr;
            public byte Fec;
            public byte ExpectedLossPercent;
            public byte Dtx;
            public byte Channels;

            public static NativeSettings From(OpusEncoderSettings s)
            {
                s = s.Clamped();
                return new NativeSettings
                {
                    BitrateBps = (uint)s.BitrateBps,
                    Complexity = (byte)s.Complexity,
                    MaxBandwidth = (int)s.MaxBandwidth,
                    Signal = (int)s.Signal,
                    Vbr = (byte)(s.Vbr ? 1 : 0),
                    ConstrainedVbr = (byte)(s.ConstrainedVbr ? 1 : 0),
                    Fec = (byte)(s.Fec ? 1 : 0),
                    ExpectedLossPercent = (byte)s.ExpectedLossPercent,
                    Dtx = (byte)(s.Dtx ? 1 : 0),
                    Channels = (byte)s.Channels,
                };
            }

            public OpusEncoderSettings ToManaged() => new OpusEncoderSettings
            {
                BitrateBps = (int)BitrateBps,
                Complexity = Complexity,
                MaxBandwidth = (OpusBandwidth)MaxBandwidth,
                Signal = (OpusSignal)Signal,
                Vbr = Vbr != 0,
                ConstrainedVbr = ConstrainedVbr != 0,
                Fec = Fec != 0,
                ExpectedLossPercent = ExpectedLossPercent,
                Dtx = Dtx != 0,
                Channels = Channels,
            };
        }

        [DllImport(Lib, CallingConvention = CallingConvention.Cdecl)]
        private static extern IntPtr aurix_opus_encoder_create(uint sampleRateHz, byte channels, ref NativeSettings settings);
        [DllImport(Lib, CallingConvention = CallingConvention.Cdecl)]
        private static extern void aurix_opus_encoder_destroy(IntPtr encoder);
        [DllImport(Lib, CallingConvention = CallingConvention.Cdecl)]
        private static extern int aurix_opus_encoder_apply(IntPtr encoder, ref NativeSettings settings);
        [DllImport(Lib, CallingConvention = CallingConvention.Cdecl)]
        [return: MarshalAs(UnmanagedType.U1)]
        private static extern bool aurix_opus_encoder_settings(IntPtr encoder, out NativeSettings settings);
        [DllImport(Lib, CallingConvention = CallingConvention.Cdecl)]
        private static extern int aurix_opus_encoder_encode_f32(IntPtr encoder, ref float pcm, UIntPtr frameSamplesPerChannel, ref byte output, UIntPtr outputLen);
        [DllImport(Lib, CallingConvention = CallingConvention.Cdecl)]
        private static extern IntPtr aurix_opus_decoder_create(uint sampleRateHz, byte channels);
        [DllImport(Lib, CallingConvention = CallingConvention.Cdecl)]
        private static extern void aurix_opus_decoder_destroy(IntPtr decoder);
        [DllImport(Lib, CallingConvention = CallingConvention.Cdecl)]
        private static extern int aurix_opus_decoder_decode_f32(IntPtr decoder, ref byte packet, UIntPtr packetLen, ref float pcm, UIntPtr maxFrameSamplesPerChannel, [MarshalAs(UnmanagedType.U1)] bool fec);
        [DllImport(Lib, CallingConvention = CallingConvention.Cdecl)]
        private static extern IntPtr aurix_last_error();

        private static bool? _available;

        /// <summary>
        /// Whether the native library loads and exports the codec entry points on this platform. Cached;
        /// safe to call from the main thread at startup to choose between this and Concentus.
        /// </summary>
        public static bool IsAvailable
        {
            get
            {
                if (_available.HasValue) return _available.Value;
                try
                {
                    var d = Probe();
                    _available = d;
                }
                catch (DllNotFoundException) { _available = false; }
                catch (EntryPointNotFoundException) { _available = false; }
                catch (BadImageFormatException) { _available = false; }
                return _available.Value;
            }
        }

        private static bool Probe()
        {
            var d = aurix_opus_decoder_create(AudioFormat.SampleRate, 1);
            if (d == IntPtr.Zero) return false;
            aurix_opus_decoder_destroy(d);
            return true;
        }

        private IntPtr _encoder;
        private IntPtr _decoder;
        private readonly object _encLock = new object();
        private readonly object _decLock = new object();
        private OpusEncoderSettings _settings;

        public int SampleRate { get; }
        public int Channels { get; }

        /// <exception cref="DllNotFoundException">The native library is not present for this platform.</exception>
        /// <exception cref="InvalidOperationException">libopus rejected the parameters (see the message).</exception>
        public NativeOpusCodec(int sampleRate = AudioFormat.SampleRate, int channels = 1, OpusEncoderSettings? settings = null)
        {
            if (channels != 1 && channels != 2) throw new ArgumentOutOfRangeException(nameof(channels), "Opus supports 1 or 2 channels");
            SampleRate = sampleRate;
            Channels = channels;
            var s = NativeSettings.From(settings ?? OpusEncoderSettings.Default);
            _encoder = aurix_opus_encoder_create((uint)sampleRate, (byte)channels, ref s);
            if (_encoder == IntPtr.Zero) throw new InvalidOperationException("aurix_opus_encoder_create: " + LastError());
            _decoder = aurix_opus_decoder_create((uint)sampleRate, (byte)channels);
            if (_decoder == IntPtr.Zero)
            {
                var err = LastError();
                aurix_opus_encoder_destroy(_encoder);
                _encoder = IntPtr.Zero;
                throw new InvalidOperationException("aurix_opus_decoder_create: " + err);
            }
            _settings = ReadSettings();
        }

        private static string LastError()
        {
            var p = aurix_last_error();
            return p == IntPtr.Zero ? string.Empty : Marshal.PtrToStringAnsi(p) ?? string.Empty;
        }

        private OpusEncoderSettings ReadSettings()
        {
            return aurix_opus_encoder_settings(_encoder, out var s) ? s.ToManaged() : _settings;
        }

        /// <summary>Settings libopus is running with (after clamping).</summary>
        public OpusEncoderSettings Settings { get { lock (_encLock) return _settings; } }

        public void Apply(OpusEncoderSettings settings)
        {
            lock (_encLock)
            {
                ThrowIfDisposed();
                var s = NativeSettings.From(settings);
                int rc = aurix_opus_encoder_apply(_encoder, ref s);
                if (rc != 0) throw new InvalidOperationException("aurix_opus_encoder_apply: " + LastError());
                _settings = ReadSettings();
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
            if (frameSamplesPerChannel <= 0 || pcm.Length < frameSamplesPerChannel * Channels)
                throw new ArgumentException("pcm shorter than one frame", nameof(pcm));
            if (output.IsEmpty) throw new ArgumentException("empty output buffer", nameof(output));
            lock (_encLock)
            {
                ThrowIfDisposed();
                int n = aurix_opus_encoder_encode_f32(_encoder, ref MemoryMarshal.GetReference(pcm),
                    (UIntPtr)(uint)frameSamplesPerChannel, ref MemoryMarshal.GetReference(output), (UIntPtr)(uint)output.Length);
                if (n < 0) throw new InvalidOperationException("opus encode: " + LastError());
                return n;
            }
        }

        public int Decode(ReadOnlySpan<byte> opus, Span<float> pcm, int maxFrameSamplesPerChannel) =>
            DecodeInternal(opus, pcm, maxFrameSamplesPerChannel, false);

        public int DecodeLost(Span<float> pcm, int frameSamplesPerChannel) =>
            DecodeInternal(ReadOnlySpan<byte>.Empty, pcm, frameSamplesPerChannel, false);

        public int DecodeFec(ReadOnlySpan<byte> nextPacket, Span<float> pcm, int frameSamplesPerChannel) =>
            nextPacket.IsEmpty ? DecodeLost(pcm, frameSamplesPerChannel) : DecodeInternal(nextPacket, pcm, frameSamplesPerChannel, true);

        private static byte _noPacket;

        private int DecodeInternal(ReadOnlySpan<byte> packet, Span<float> pcm, int frameSamplesPerChannel, bool fec)
        {
            if (frameSamplesPerChannel <= 0 || pcm.Length < frameSamplesPerChannel * Channels)
                throw new ArgumentException("pcm buffer too small", nameof(pcm));
            lock (_decLock)
            {
                ThrowIfDisposed();
                ref float o = ref MemoryMarshal.GetReference(pcm);
                var frames = (UIntPtr)(uint)frameSamplesPerChannel;
                int n = packet.IsEmpty
                    ? aurix_opus_decoder_decode_f32(_decoder, ref _noPacket, UIntPtr.Zero, ref o, frames, false)
                    : aurix_opus_decoder_decode_f32(_decoder, ref MemoryMarshal.GetReference(packet), (UIntPtr)(uint)packet.Length, ref o, frames, fec);
                // A corrupt packet is not fatal for a live stream: conceal it and keep going.
                if (n < 0) n = aurix_opus_decoder_decode_f32(_decoder, ref _noPacket, UIntPtr.Zero, ref o, frames, false);
                return Math.Max(0, n);
            }
        }

        private void ThrowIfDisposed()
        {
            if (_encoder == IntPtr.Zero || _decoder == IntPtr.Zero) throw new ObjectDisposedException(nameof(NativeOpusCodec));
        }

        public void Dispose()
        {
            lock (_encLock)
            lock (_decLock)
            {
                if (_encoder != IntPtr.Zero) { aurix_opus_encoder_destroy(_encoder); _encoder = IntPtr.Zero; }
                if (_decoder != IntPtr.Zero) { aurix_opus_decoder_destroy(_decoder); _decoder = IntPtr.Zero; }
            }
        }
    }
}
