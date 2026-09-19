using System;
using System.Runtime.InteropServices;
using System.Threading;

namespace Aurix.Audio
{
    /// <summary>
    /// <see cref="ICaptureProcessor"/> backed by the Aurix native core (<c>aurix_dsp_*</c> in
    /// <c>aurix_client</c>, the same binary as <see cref="NativeOpusCodec"/>): 80 Hz high-pass,
    /// frequency-domain acoustic echo cancellation with delay estimation, RNNoise-derived neural
    /// noise suppression and a speech-gated AGC. Check <see cref="IsAvailable"/> to choose between
    /// this and <see cref="ManagedCaptureDsp"/> at runtime (or let <see cref="CaptureDsp.Create"/> do it).
    /// </summary>
    public sealed class NativeCaptureDsp : ICaptureProcessor
    {
#if UNITY_IOS && !UNITY_EDITOR
        private const string Lib = "__Internal";
#else
        private const string Lib = "aurix_client";
#endif

        /// <summary>Blittable mirror of the C <c>AurixDspConfig</c> (bools are one byte in C).</summary>
        [StructLayout(LayoutKind.Sequential)]
        private struct NativeConfig
        {
            public byte HighPass;
            public byte EchoCancellation;
            public uint EchoTailMs;
            public uint StreamDelayMs;
            public int NoiseSuppression;
            public byte Agc;
            public float AgcTargetDbfs;
            public float AgcMaxGainDb;

            public static NativeConfig From(DspSettings s)
            {
                s = s.Clamped();
                return new NativeConfig
                {
                    HighPass = (byte)(s.HighPass ? 1 : 0),
                    EchoCancellation = (byte)(s.EchoCancellation ? 1 : 0),
                    EchoTailMs = (uint)s.EchoTailMs,
                    StreamDelayMs = (uint)s.StreamDelayMs,
                    NoiseSuppression = (int)s.NoiseSuppression,
                    Agc = (byte)(s.Agc ? 1 : 0),
                    AgcTargetDbfs = s.AgcTargetDbfs,
                    AgcMaxGainDb = s.AgcMaxGainDb,
                };
            }

            public DspSettings ToManaged() => new DspSettings
            {
                HighPass = HighPass != 0,
                EchoCancellation = EchoCancellation != 0,
                EchoTailMs = (int)EchoTailMs,
                StreamDelayMs = (int)StreamDelayMs,
                NoiseSuppression = (NoiseSuppression)NoiseSuppression,
                Agc = Agc != 0,
                AgcTargetDbfs = AgcTargetDbfs,
                AgcMaxGainDb = AgcMaxGainDb,
            };
        }

        /// <summary>Blittable mirror of the C <c>AurixDspStats</c>.</summary>
        [StructLayout(LayoutKind.Sequential)]
        private struct NativeStats
        {
            public float ErleDb;
            public uint EchoDelayMs;
            public byte EchoConverged;
            public byte FarEndActive;
            public float SpeechProbability;
            public float AgcGainDb;
            public ulong FarEndUnderruns;

            public DspStats ToManaged() => new DspStats
            {
                ErleDb = ErleDb,
                EchoDelayMs = (int)EchoDelayMs,
                EchoConverged = EchoConverged != 0,
                FarEndActive = FarEndActive != 0,
                SpeechProbability = SpeechProbability,
                AgcGainDb = AgcGainDb,
                FarEndUnderruns = FarEndUnderruns,
            };
        }

        [DllImport(Lib, CallingConvention = CallingConvention.Cdecl)]
        private static extern IntPtr aurix_dsp_create(ref NativeConfig config);
        [DllImport(Lib, CallingConvention = CallingConvention.Cdecl)]
        private static extern void aurix_dsp_destroy(IntPtr dsp);
        [DllImport(Lib, CallingConvention = CallingConvention.Cdecl)]
        private static extern int aurix_dsp_set_config(IntPtr dsp, ref NativeConfig config);
        [DllImport(Lib, CallingConvention = CallingConvention.Cdecl)]
        private static extern int aurix_dsp_config(IntPtr dsp, out NativeConfig config);
        [DllImport(Lib, CallingConvention = CallingConvention.Cdecl)]
        private static extern int aurix_dsp_stats(IntPtr dsp, out NativeStats stats);
        [DllImport(Lib, CallingConvention = CallingConvention.Cdecl)]
        private static extern int aurix_dsp_process_f32(IntPtr dsp, ref float pcm, UIntPtr sampleCount);
        [DllImport(Lib, CallingConvention = CallingConvention.Cdecl)]
        private static extern void aurix_dsp_push_render_f32(IntPtr dsp, ref float pcm, UIntPtr sampleCount, byte channels);
        [DllImport(Lib, CallingConvention = CallingConvention.Cdecl)]
        private static extern IntPtr aurix_last_error();

        private static bool? _available;

        /// <summary>Whether the native library loads and exports the DSP entry points on this platform (cached).</summary>
        public static bool IsAvailable
        {
            get
            {
                if (_available.HasValue) return _available.Value;
                try
                {
                    var cfg = NativeConfig.From(DspSettings.Bypass);
                    var d = aurix_dsp_create(ref cfg);
                    if (d != IntPtr.Zero) aurix_dsp_destroy(d);
                    _available = d != IntPtr.Zero;
                }
                catch (DllNotFoundException) { _available = false; }
                catch (EntryPointNotFoundException) { _available = false; }
                catch (BadImageFormatException) { _available = false; }
                return _available.Value;
            }
        }

        private IntPtr _dsp;
        private readonly object _lock = new object();
        private DspSettings _settings;

        /// <exception cref="DllNotFoundException">The native library is not present for this platform.</exception>
        public NativeCaptureDsp(DspSettings settings)
        {
            var cfg = NativeConfig.From(settings);
            _dsp = aurix_dsp_create(ref cfg);
            if (_dsp == IntPtr.Zero) throw new InvalidOperationException("aurix_dsp_create: " + LastError());
            _settings = ReadSettings();
        }

        private static string LastError()
        {
            var p = aurix_last_error();
            return p == IntPtr.Zero ? string.Empty : Marshal.PtrToStringAnsi(p) ?? string.Empty;
        }

        private DspSettings ReadSettings() =>
            aurix_dsp_config(_dsp, out var c) == 0 ? c.ToManaged() : _settings;

        public int BlockSamples => CaptureDsp.BlockSamples;
        public bool SupportsEchoCancellation => true;
        public bool SupportsNoiseSuppression => true;

        public DspSettings Settings { get { lock (_lock) return _settings; } }

        public void Apply(DspSettings settings)
        {
            lock (_lock)
            {
                ThrowIfDisposed();
                var cfg = NativeConfig.From(settings);
                if (aurix_dsp_set_config(_dsp, ref cfg) != 0) throw new InvalidOperationException("aurix_dsp_set_config: " + LastError());
                _settings = ReadSettings();
            }
        }

        public DspStats Stats
        {
            get
            {
                lock (_lock)
                {
                    if (_dsp == IntPtr.Zero) return default;
                    return aurix_dsp_stats(_dsp, out var s) == 0 ? s.ToManaged() : default;
                }
            }
        }

        public void Process(float[] mono, int frames)
        {
            if (mono == null) throw new ArgumentNullException(nameof(mono));
            if (frames <= 0 || frames > mono.Length) throw new ArgumentOutOfRangeException(nameof(frames));
            int whole = frames - frames % BlockSamples;
            if (whole == 0) return;
            lock (_lock)
            {
                ThrowIfDisposed();
                int rc = aurix_dsp_process_f32(_dsp, ref mono[0], (UIntPtr)(uint)whole);
                if (rc != 0) throw new InvalidOperationException("aurix_dsp_process_f32: " + LastError());
            }
        }

        // Called from the audio thread. The native far-end queue is thread-safe; the lock only
        // fences the handle against Dispose, so a short wait bounds the audio-thread stall.
        public void PushRender(float[] pcm, int offset, int count, int channels)
        {
            if (pcm == null || count <= 0 || offset < 0 || offset + count > pcm.Length || channels < 1 || channels > 255) return;
            if (!Monitor.TryEnter(_lock, 2)) return;
            try
            {
                if (_dsp == IntPtr.Zero) return;
                aurix_dsp_push_render_f32(_dsp, ref pcm[offset], (UIntPtr)(uint)count, (byte)channels);
            }
            finally { Monitor.Exit(_lock); }
        }

        private void ThrowIfDisposed()
        {
            if (_dsp == IntPtr.Zero) throw new ObjectDisposedException(nameof(NativeCaptureDsp));
        }

        public void Dispose()
        {
            lock (_lock)
            {
                var h = _dsp;
                _dsp = IntPtr.Zero;
                if (h != IntPtr.Zero) aurix_dsp_destroy(h);
            }
        }
    }
}
