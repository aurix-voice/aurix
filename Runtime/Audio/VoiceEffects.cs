using System;
using System.Runtime.InteropServices;

namespace Aurix.Audio
{
    /// <summary>
    /// Ready-made voices (the same tunings as the native core and the Web SDK); see
    /// <see cref="VoiceEffectParams.Preset"/>.
    /// </summary>
    public enum VoiceEffectPreset
    {
        /// <summary>Ring-modulated, band-limited, slightly saturated.</summary>
        Robot,
        /// <summary>Pitched and formant-shifted down, growly, in a large space.</summary>
        Monster,
        /// <summary>Telephone band, crunchy, static under the voice.</summary>
        Radio,
        /// <summary>Pitched and formant-shifted up.</summary>
        Helium,
        /// <summary>Hollow, wavering, drenched in reverb.</summary>
        Ghost,
    }

    /// <summary>
    /// Every built-in voice-effect stage in one place: zero means "that stage is off", so
    /// <c>default</c> is a bypass. Stages run in field order — filters, formant, pitch, ring
    /// modulator, distortion, tremolo, static, reverb — on the local microphone only (never on
    /// injected audio or on what other participants say). Field layout mirrors the native
    /// <c>AurixVoiceEffects</c> struct.
    /// </summary>
    [Serializable]
    [StructLayout(LayoutKind.Sequential)]
    public struct VoiceEffectParams : IEquatable<VoiceEffectParams>
    {
        public const float MaxPitchSemitones = 24f;
        public const float MaxFormantSemitones = 12f;
        public const float MaxRingModHz = 2000f;
        public const float MaxDistortionDrive = 20f;
        public const float MaxTremoloHz = 20f;
        public const float MinFilterHz = 20f;
        public const float MaxFilterHz = 20000f;

        /// <summary>High-pass corner in Hz (0 = off) — thins the voice (radio, phone).</summary>
        public float HighpassHz;
        /// <summary>Low-pass corner in Hz (0 = off) — muffles.</summary>
        public float LowpassHz;
        /// <summary>Vocal-tract size in semitones (±12) without changing the pitch. Adds ~43 ms of latency while non-zero.</summary>
        public float FormantSemitones;
        /// <summary>Pitch shift in semitones (±24); formants follow.</summary>
        public float PitchSemitones;
        /// <summary>Ring-modulator ("robot") carrier in Hz (0..2000, 0 = off).</summary>
        public float RingModHz;
        /// <summary>Saturation drive (1..20, 0 = off).</summary>
        public float DistortionDrive;
        /// <summary>Tremolo rate in Hz (0..20, 0 = off).</summary>
        public float TremoloHz;
        /// <summary>Tremolo depth 0..1 (the tremolo is off at 0 whatever the rate).</summary>
        public float TremoloDepth;
        /// <summary>Static / hiss level 0..1, gated by the voice (silence stays silent).</summary>
        public float StaticLevel;
        /// <summary>Reverb wet mix 0..1 (0 = off).</summary>
        public float ReverbMix;
        /// <summary>Reverb room size 0..1.</summary>
        public float ReverbSize;
        /// <summary>Reverb high-frequency damping 0..1.</summary>
        public float ReverbDamping;

        /// <summary>Every stage off.</summary>
        public static VoiceEffectParams Bypass => default;

        /// <summary>The parameters of a built-in preset — a starting point to tweak.</summary>
        public static VoiceEffectParams Preset(VoiceEffectPreset preset)
        {
            switch (preset)
            {
                case VoiceEffectPreset.Robot:
                    return new VoiceEffectParams { HighpassHz = 200f, LowpassHz = 4000f, RingModHz = 60f, DistortionDrive = 2f };
                case VoiceEffectPreset.Monster:
                    return new VoiceEffectParams
                    {
                        FormantSemitones = -5f, PitchSemitones = -7f, DistortionDrive = 1.5f,
                        ReverbMix = 0.15f, ReverbSize = 0.6f, ReverbDamping = 0.5f,
                    };
                case VoiceEffectPreset.Radio:
                    return new VoiceEffectParams { HighpassHz = 400f, LowpassHz = 3000f, DistortionDrive = 3f, StaticLevel = 0.03f };
                case VoiceEffectPreset.Helium:
                    return new VoiceEffectParams { FormantSemitones = 6f, PitchSemitones = 6f };
                case VoiceEffectPreset.Ghost:
                    return new VoiceEffectParams
                    {
                        LowpassHz = 5000f, FormantSemitones = 2f, PitchSemitones = -3f, TremoloHz = 5f, TremoloDepth = 0.5f,
                        ReverbMix = 0.6f, ReverbSize = 0.9f, ReverbDamping = 0.3f,
                    };
                default:
                    throw new ArgumentOutOfRangeException(nameof(preset), preset, "unknown voice effect preset");
            }
        }

        /// <summary>Every field clamped into its documented range (NaN → off), like the native <c>sanitized()</c>.</summary>
        public VoiceEffectParams Sanitized()
        {
            return new VoiceEffectParams
            {
                HighpassHz = Corner(HighpassHz),
                LowpassHz = Corner(LowpassHz),
                FormantSemitones = Clamp(FormantSemitones, -MaxFormantSemitones, MaxFormantSemitones),
                PitchSemitones = Clamp(PitchSemitones, -MaxPitchSemitones, MaxPitchSemitones),
                RingModHz = Clamp(RingModHz, 0f, MaxRingModHz),
                DistortionDrive = DistortionDrive > 0f && !float.IsNaN(DistortionDrive) ? Clamp(DistortionDrive, 1f, MaxDistortionDrive) : 0f,
                TremoloHz = Clamp(TremoloHz, 0f, MaxTremoloHz),
                TremoloDepth = Clamp(TremoloDepth, 0f, 1f),
                StaticLevel = Clamp(StaticLevel, 0f, 1f),
                ReverbMix = Clamp(ReverbMix, 0f, 1f),
                ReverbSize = Clamp(ReverbSize, 0f, 1f),
                ReverbDamping = Clamp(ReverbDamping, 0f, 1f),
            };
        }

        /// <summary>True when, after sanitizing, no stage does anything.</summary>
        public bool IsBypass
        {
            get
            {
                var s = Sanitized();
                return s.HighpassHz == 0f && s.LowpassHz == 0f && s.FormantSemitones == 0f && s.PitchSemitones == 0f
                    && s.RingModHz == 0f && s.DistortionDrive == 0f && (s.TremoloHz == 0f || s.TremoloDepth == 0f)
                    && s.StaticLevel == 0f && s.ReverbMix == 0f;
            }
        }

        private static float Clamp(float v, float lo, float hi) => float.IsNaN(v) ? 0f : (v < lo ? lo : (v > hi ? hi : v));

        private static float Corner(float v) => v > 0f && !float.IsNaN(v) ? Clamp(v, MinFilterHz, MaxFilterHz) : 0f;

        public bool Equals(VoiceEffectParams o) =>
            HighpassHz == o.HighpassHz && LowpassHz == o.LowpassHz && FormantSemitones == o.FormantSemitones
            && PitchSemitones == o.PitchSemitones && RingModHz == o.RingModHz && DistortionDrive == o.DistortionDrive
            && TremoloHz == o.TremoloHz && TremoloDepth == o.TremoloDepth && StaticLevel == o.StaticLevel
            && ReverbMix == o.ReverbMix && ReverbSize == o.ReverbSize && ReverbDamping == o.ReverbDamping;

        public override bool Equals(object obj) => obj is VoiceEffectParams o && Equals(o);

        public override int GetHashCode()
        {
            unchecked
            {
                int h = HighpassHz.GetHashCode();
                h = h * 31 + LowpassHz.GetHashCode();
                h = h * 31 + FormantSemitones.GetHashCode();
                h = h * 31 + PitchSemitones.GetHashCode();
                h = h * 31 + RingModHz.GetHashCode();
                h = h * 31 + DistortionDrive.GetHashCode();
                h = h * 31 + TremoloHz.GetHashCode();
                h = h * 31 + TremoloDepth.GetHashCode();
                h = h * 31 + StaticLevel.GetHashCode();
                h = h * 31 + ReverbMix.GetHashCode();
                h = h * 31 + ReverbSize.GetHashCode();
                return h * 31 + ReverbDamping.GetHashCode();
            }
        }
    }

    /// <summary>
    /// Realtime voice-effect chain backed by the Aurix native core (<c>aurix_voice_effects_*</c> in
    /// <c>aurix_client</c>, the same binary as <see cref="NativeOpusCodec"/> and
    /// <see cref="NativeCaptureDsp"/>): the stages of <see cref="VoiceEffectParams"/> applied in
    /// place to 48 kHz mono or stereo frames without channel bleed. Check <see cref="IsAvailable"/>
    /// first; without the native library Unity has no managed fallback and the microphone goes out
    /// unprocessed. Not thread-safe: call <see cref="Process"/> and <see cref="Apply"/> from the
    /// capture thread (or hold your own lock).
    /// </summary>
    public sealed class NativeVoiceEffects : IDisposable
    {
#if UNITY_IOS && !UNITY_EDITOR
        private const string Lib = "__Internal";
#else
        private const string Lib = "aurix_client";
#endif

        [DllImport(Lib, CallingConvention = CallingConvention.Cdecl)]
        private static extern IntPtr aurix_voice_effects_create(ref VoiceEffectParams effects);
        [DllImport(Lib, CallingConvention = CallingConvention.Cdecl)]
        private static extern void aurix_voice_effects_destroy(IntPtr processor);
        [DllImport(Lib, CallingConvention = CallingConvention.Cdecl)]
        private static extern int aurix_voice_effects_set(IntPtr processor, ref VoiceEffectParams effects);
        [DllImport(Lib, CallingConvention = CallingConvention.Cdecl)]
        private static extern int aurix_voice_effects_get(IntPtr processor, out VoiceEffectParams effects);
        [DllImport(Lib, CallingConvention = CallingConvention.Cdecl)]
        private static extern int aurix_voice_effects_process_f32(IntPtr processor, ref float pcm, UIntPtr sampleCount, byte channels);
        [DllImport(Lib, CallingConvention = CallingConvention.Cdecl)]
        private static extern void aurix_voice_effects_reset(IntPtr processor);
        [DllImport(Lib, CallingConvention = CallingConvention.Cdecl)]
        private static extern IntPtr aurix_last_error();

        private static bool? _available;

        /// <summary>Whether the native library loads and exports the effect entry points on this platform (cached).</summary>
        public static bool IsAvailable
        {
            get
            {
                if (_available.HasValue) return _available.Value;
                try
                {
                    var bypass = VoiceEffectParams.Bypass;
                    var p = aurix_voice_effects_create(ref bypass);
                    if (p != IntPtr.Zero) aurix_voice_effects_destroy(p);
                    _available = p != IntPtr.Zero;
                }
                catch (DllNotFoundException) { _available = false; }
                catch (EntryPointNotFoundException) { _available = false; }
                catch (BadImageFormatException) { _available = false; }
                return _available.Value;
            }
        }

        private IntPtr _processor;
        private VoiceEffectParams _params;

        /// <exception cref="DllNotFoundException">The native library is not present for this platform.</exception>
        public NativeVoiceEffects(VoiceEffectParams effects)
        {
            _processor = aurix_voice_effects_create(ref effects);
            if (_processor == IntPtr.Zero) throw new InvalidOperationException("aurix_voice_effects_create: " + LastError());
            _params = aurix_voice_effects_get(_processor, out var back) == 0 ? back : effects.Sanitized();
        }

        public NativeVoiceEffects(VoiceEffectPreset preset) : this(VoiceEffectParams.Preset(preset)) { }

        private static string LastError()
        {
            var p = aurix_last_error();
            return p == IntPtr.Zero ? string.Empty : Marshal.PtrToStringAnsi(p) ?? string.Empty;
        }

        /// <summary>The active (clamped) parameters.</summary>
        public VoiceEffectParams Params => _params;

        /// <summary>True when every stage is off — <see cref="Process"/> is then a no-op.</summary>
        public bool IsBypass => _params.IsBypass;

        /// <summary>Replace the parameters; a changed set restarts the stages (reverb tails, pitch buffers).</summary>
        public void Apply(VoiceEffectParams effects)
        {
            ThrowIfDisposed();
            if (aurix_voice_effects_set(_processor, ref effects) != 0) throw new InvalidOperationException("aurix_voice_effects_set: " + LastError());
            _params = aurix_voice_effects_get(_processor, out var back) == 0 ? back : effects.Sanitized();
        }

        public void Apply(VoiceEffectPreset preset) => Apply(VoiceEffectParams.Preset(preset));

        /// <summary>
        /// Process <paramref name="frames"/> interleaved frames of <paramref name="channels"/> (1 or 2)
        /// 48 kHz samples in place. The chain is stateful: feed the consecutive frames of one stream.
        /// </summary>
        public void Process(float[] pcm, int frames, int channels)
        {
            if (pcm == null) throw new ArgumentNullException(nameof(pcm));
            if (channels < 1 || channels > 2) throw new ArgumentOutOfRangeException(nameof(channels));
            if (frames <= 0 || frames * channels > pcm.Length) throw new ArgumentOutOfRangeException(nameof(frames));
            ThrowIfDisposed();
            if (IsBypass) return;
            int rc = aurix_voice_effects_process_f32(_processor, ref pcm[0], (UIntPtr)(uint)(frames * channels), (byte)channels);
            if (rc != 0) throw new InvalidOperationException("aurix_voice_effects_process_f32: " + LastError());
        }

        /// <summary>Clear the stages' state (tails, phases) on a stream discontinuity such as a device switch.</summary>
        public void Reset()
        {
            if (_processor != IntPtr.Zero) aurix_voice_effects_reset(_processor);
        }

        private void ThrowIfDisposed()
        {
            if (_processor == IntPtr.Zero) throw new ObjectDisposedException(nameof(NativeVoiceEffects));
        }

        public void Dispose()
        {
            var h = _processor;
            _processor = IntPtr.Zero;
            if (h != IntPtr.Zero) aurix_voice_effects_destroy(h);
        }
    }
}
