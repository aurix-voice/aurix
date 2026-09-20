using System;
using System.Runtime.InteropServices;

namespace Aurix.Audio
{
    /// <summary>
    /// Mouth-shape buckets of <see cref="VisemeFrame"/> (index = value, same order as the native
    /// core and the Web SDK). <c>PP</c>/<c>FF</c>/<c>SS</c> follow the common lip-sync naming
    /// (bilabial closure, labiodental, sibilant); the rest are vowels.
    /// </summary>
    public enum Viseme
    {
        /// <summary>Mouth closed, no speech.</summary>
        Silence = 0,
        /// <summary>Lips together: p / b / m, and nasal humming.</summary>
        PP = 1,
        /// <summary>Soft friction: f / v / th.</summary>
        FF = 2,
        /// <summary>Sibilant: s / z / sh.</summary>
        SS = 3,
        /// <summary>Open vowel: "father".</summary>
        AA = 4,
        /// <summary>Mid front vowel: "bed".</summary>
        E = 5,
        /// <summary>Close front vowel: "see" / "sit".</summary>
        IH = 6,
        /// <summary>Mid back rounded vowel: "law" / "go".</summary>
        OH = 7,
        /// <summary>Close back rounded vowel: "boot".</summary>
        OU = 8,
    }

    /// <summary>
    /// Mouth state derived from the latest analysed 20 ms of one voice — a participant's decoded
    /// audio or the local microphone. Drive blend shapes from <see cref="Weight"/> /
    /// <see cref="MouthOpen"/> every render tick; compare <see cref="Sequence"/> between reads to
    /// see whether new audio arrived. Layout mirrors the native <c>AurixVisemeFrame</c>.
    /// </summary>
    [StructLayout(LayoutKind.Sequential)]
    public struct VisemeFrame
    {
        public const int Count = 9;

        /// <summary>Wire names used by the Web SDK / bridge, indexed by <see cref="Viseme"/>.</summary>
        public static readonly string[] Names = { "sil", "PP", "FF", "SS", "aa", "E", "ih", "oh", "ou" };

        private float _w0, _w1, _w2, _w3, _w4, _w5, _w6, _w7, _w8;
        private int _dominant;
        /// <summary>Jaw openness 0..1 (level × the dominant shape's openness, smoothed).</summary>
        public float MouthOpen;
        /// <summary>RMS level of the frame, 0..1.</summary>
        public float Energy;
        /// <summary>How clear-cut the classification is, 0..1 (margin between the top two buckets).</summary>
        public float Confidence;
        /// <summary>Frames analysed so far; unchanged between two reads means no new audio arrived.</summary>
        public ulong Sequence;

        /// <summary>The heaviest bucket.</summary>
        public Viseme Dominant
        {
            get => (Viseme)_dominant;
            set => _dominant = (int)value;
        }

        /// <summary>Smoothed weight of one bucket; the nine weights sum to ~1.</summary>
        public float Weight(Viseme viseme) => this[(int)viseme];

        public float this[int index]
        {
            get
            {
                switch (index)
                {
                    case 0: return _w0;
                    case 1: return _w1;
                    case 2: return _w2;
                    case 3: return _w3;
                    case 4: return _w4;
                    case 5: return _w5;
                    case 6: return _w6;
                    case 7: return _w7;
                    case 8: return _w8;
                    default: throw new ArgumentOutOfRangeException(nameof(index));
                }
            }
            set
            {
                switch (index)
                {
                    case 0: _w0 = value; break;
                    case 1: _w1 = value; break;
                    case 2: _w2 = value; break;
                    case 3: _w3 = value; break;
                    case 4: _w4 = value; break;
                    case 5: _w5 = value; break;
                    case 6: _w6 = value; break;
                    case 7: _w7 = value; break;
                    case 8: _w8 = value; break;
                    default: throw new ArgumentOutOfRangeException(nameof(index));
                }
            }
        }

        /// <summary>Copy the nine weights into <paramref name="into"/> (length ≥ <see cref="Count"/>).</summary>
        public void CopyWeights(float[] into)
        {
            if (into == null || into.Length < Count) throw new ArgumentException("need " + Count + " floats", nameof(into));
            for (int i = 0; i < Count; i++) into[i] = this[i];
        }

        /// <summary>Mouth closed, full confidence, the given sequence.</summary>
        public static VisemeFrame Silent(ulong sequence = 0)
        {
            var f = new VisemeFrame { Confidence = 1f, Sequence = sequence };
            f[(int)Viseme.Silence] = 1f;
            return f;
        }
    }

    /// <summary>
    /// Lip-sync analyser for one audio stream backed by the Aurix native core
    /// (<c>aurix_viseme_analyzer_*</c> in <c>aurix_client</c>): FFT + formant tracking over 20 ms
    /// frames of 48 kHz PCM, smoothed into a <see cref="VisemeFrame"/>. Purely local — nothing
    /// leaves the machine, and it works in end-to-end encrypted channels because frames are decrypted
    /// here. Check <see cref="IsAvailable"/>; without the native library Unity has no managed
    /// fallback. Not thread-safe: <see cref="Push"/> from one thread, read <see cref="Frame"/> from
    /// any (a torn read only mixes two consecutive frames).
    /// </summary>
    public sealed class NativeVisemeAnalyzer : IDisposable
    {
#if UNITY_IOS && !UNITY_EDITOR
        private const string Lib = "__Internal";
#else
        private const string Lib = "aurix_client";
#endif

        [DllImport(Lib, CallingConvention = CallingConvention.Cdecl)]
        private static extern IntPtr aurix_viseme_analyzer_create();
        [DllImport(Lib, CallingConvention = CallingConvention.Cdecl)]
        private static extern void aurix_viseme_analyzer_destroy(IntPtr analyzer);
        [DllImport(Lib, CallingConvention = CallingConvention.Cdecl)]
        private static extern int aurix_viseme_analyzer_push_f32(IntPtr analyzer, ref float pcm, UIntPtr sampleCount, byte channels);
        [DllImport(Lib, CallingConvention = CallingConvention.Cdecl)]
        private static extern int aurix_viseme_analyzer_frame(IntPtr analyzer, out VisemeFrame frame);
        [DllImport(Lib, CallingConvention = CallingConvention.Cdecl)]
        private static extern void aurix_viseme_analyzer_reset(IntPtr analyzer);

        private static bool? _available;

        /// <summary>Whether the native library loads and exports the analyser entry points on this platform (cached).</summary>
        public static bool IsAvailable
        {
            get
            {
                if (_available.HasValue) return _available.Value;
                try
                {
                    var a = aurix_viseme_analyzer_create();
                    if (a != IntPtr.Zero) aurix_viseme_analyzer_destroy(a);
                    _available = a != IntPtr.Zero;
                }
                catch (DllNotFoundException) { _available = false; }
                catch (EntryPointNotFoundException) { _available = false; }
                catch (BadImageFormatException) { _available = false; }
                return _available.Value;
            }
        }

        private IntPtr _analyzer;
        private VisemeFrame _frame = VisemeFrame.Silent();

        /// <exception cref="DllNotFoundException">The native library is not present for this platform.</exception>
        public NativeVisemeAnalyzer()
        {
            _analyzer = aurix_viseme_analyzer_create();
            if (_analyzer == IntPtr.Zero) throw new InvalidOperationException("aurix_viseme_analyzer_create failed");
        }

        /// <summary>The smoothed mouth state after the last <see cref="Push"/> (silence before the first).</summary>
        public VisemeFrame Frame => _frame;

        /// <summary>
        /// Analyse up to 20 ms (<see cref="AudioFormat.FrameSamples"/> frames) of interleaved 48 kHz PCM
        /// starting at <paramref name="offset"/>; shorter input is zero-padded, longer input is truncated.
        /// </summary>
        public void Push(float[] pcm, int offset, int frames, int channels)
        {
            if (pcm == null) throw new ArgumentNullException(nameof(pcm));
            if (channels < 1 || channels > 8) throw new ArgumentOutOfRangeException(nameof(channels));
            if (offset < 0 || frames <= 0 || offset + frames * channels > pcm.Length) throw new ArgumentOutOfRangeException(nameof(frames));
            if (_analyzer == IntPtr.Zero) throw new ObjectDisposedException(nameof(NativeVisemeAnalyzer));
            if (aurix_viseme_analyzer_push_f32(_analyzer, ref pcm[offset], (UIntPtr)(uint)(frames * channels), (byte)channels) != 0) return;
            if (aurix_viseme_analyzer_frame(_analyzer, out var f) == 0) _frame = f;
        }

        /// <summary>Analyse a whole buffer of interleaved frames.</summary>
        public void Push(float[] pcm, int frames, int channels) => Push(pcm, 0, frames, channels);

        /// <summary>
        /// One 20 ms tick with no audio to analyse (the stream is starved or paused): the mouth relaxes
        /// towards closed at the analyser's own smoothing rate instead of snapping shut.
        /// </summary>
        public void Relax()
        {
            if (_analyzer == IntPtr.Zero) throw new ObjectDisposedException(nameof(NativeVisemeAnalyzer));
            if (aurix_viseme_analyzer_push_f32(_analyzer, ref _silence[0], (UIntPtr)(uint)_silence.Length, 1) != 0) return;
            if (aurix_viseme_analyzer_frame(_analyzer, out var f) == 0) _frame = f;
        }

        private static readonly float[] _silence = new float[AudioFormat.FrameSamples];

        /// <summary>Back to silence, keeping <see cref="VisemeFrame.Sequence"/> — the stream stopped or changed speaker.</summary>
        public void Reset()
        {
            if (_analyzer == IntPtr.Zero) return;
            aurix_viseme_analyzer_reset(_analyzer);
            if (aurix_viseme_analyzer_frame(_analyzer, out var f) == 0) _frame = f;
        }

        public void Dispose()
        {
            var h = _analyzer;
            _analyzer = IntPtr.Zero;
            if (h != IntPtr.Zero) aurix_viseme_analyzer_destroy(h);
        }
    }
}
