using System;

namespace Aurix.Audio
{
    /// <summary>Noise suppression strength (dry/wet blend of the suppressor output).</summary>
    public enum NoiseSuppression
    {
        Off = 0,
        Low = 1,
        Moderate = 2,
        High = 3,
    }

    /// <summary>
    /// Capture-side processing applied to the microphone before the input gain, VAD and encoder.
    /// Mirrors <c>DspConfig</c> in the native core (<c>aurix_client::dsp</c>); out-of-range values are
    /// clamped by <see cref="Clamped"/>, never rejected.
    /// </summary>
    [Serializable]
    public struct DspSettings : IEquatable<DspSettings>
    {
        public const int MinEchoTailMs = 40;
        public const int MaxEchoTailMs = 500;
        public const int MaxStreamDelayMs = 500;

        /// <summary>80 Hz second-order high-pass (rumble, handling noise, DC).</summary>
        public bool HighPass;
        /// <summary>
        /// Acoustic echo cancellation against the audio the SDK plays (fed automatically from the
        /// remote mix, or by hand with <see cref="ICaptureProcessor.PushRender"/>). Native core only.
        /// </summary>
        public bool EchoCancellation;
        /// <summary>Echo tail modelled by the adaptive filter, ms (<see cref="MinEchoTailMs"/>..<see cref="MaxEchoTailMs"/>).</summary>
        public int EchoTailMs;
        /// <summary>Initial playback→capture delay hint, ms; the canceller refines it at runtime.</summary>
        public int StreamDelayMs;
        /// <summary>RNNoise-derived neural noise suppression. Native core only.</summary>
        public NoiseSuppression NoiseSuppression;
        /// <summary>Speech-gated automatic gain control with a soft limiter.</summary>
        public bool Agc;
        /// <summary>AGC target speech level, dBFS RMS (-30..-6).</summary>
        public float AgcTargetDbfs;
        /// <summary>Maximum AGC boost, dB (0..40).</summary>
        public float AgcMaxGainDb;

        /// <summary>Everything on: high-pass, AEC (200 ms tail), high noise suppression, AGC to -18 dBFS.</summary>
        public static DspSettings Default => new DspSettings
        {
            HighPass = true,
            EchoCancellation = true,
            EchoTailMs = 200,
            StreamDelayMs = 0,
            NoiseSuppression = NoiseSuppression.High,
            Agc = true,
            AgcTargetDbfs = -18f,
            AgcMaxGainDb = 24f,
        };

        /// <summary>Everything off: the microphone reaches the VAD/encoder untouched.</summary>
        public static DspSettings Bypass => new DspSettings
        {
            HighPass = false,
            EchoCancellation = false,
            EchoTailMs = 200,
            StreamDelayMs = 0,
            NoiseSuppression = NoiseSuppression.Off,
            Agc = false,
            AgcTargetDbfs = -18f,
            AgcMaxGainDb = 24f,
        };

        /// <summary>Same clamping as the native core (tail rounded up to 10 ms blocks).</summary>
        public DspSettings Clamped()
        {
            var s = this;
            int tail = Math.Min(Math.Max(s.EchoTailMs, MinEchoTailMs), MaxEchoTailMs);
            s.EchoTailMs = (tail + 9) / 10 * 10;
            s.StreamDelayMs = Math.Min(Math.Max(s.StreamDelayMs, 0), MaxStreamDelayMs);
            if (float.IsNaN(s.AgcTargetDbfs) || float.IsInfinity(s.AgcTargetDbfs)) s.AgcTargetDbfs = -18f;
            if (float.IsNaN(s.AgcMaxGainDb) || float.IsInfinity(s.AgcMaxGainDb)) s.AgcMaxGainDb = 24f;
            s.AgcTargetDbfs = Math.Min(Math.Max(s.AgcTargetDbfs, -30f), -6f);
            s.AgcMaxGainDb = Math.Min(Math.Max(s.AgcMaxGainDb, 0f), 40f);
            if (s.NoiseSuppression < NoiseSuppression.Off || s.NoiseSuppression > NoiseSuppression.High)
                s.NoiseSuppression = NoiseSuppression.High;
            return s;
        }

        /// <summary><c>true</c> when at least one stage is on.</summary>
        public bool AnyEnabled => HighPass || EchoCancellation || NoiseSuppression != NoiseSuppression.Off || Agc;

        public bool Equals(DspSettings o) =>
            HighPass == o.HighPass && EchoCancellation == o.EchoCancellation && EchoTailMs == o.EchoTailMs &&
            StreamDelayMs == o.StreamDelayMs && NoiseSuppression == o.NoiseSuppression && Agc == o.Agc &&
            AgcTargetDbfs.Equals(o.AgcTargetDbfs) && AgcMaxGainDb.Equals(o.AgcMaxGainDb);

        public override bool Equals(object obj) => obj is DspSettings o && Equals(o);

        public override int GetHashCode()
        {
            unchecked
            {
                int h = HighPass ? 1 : 0;
                h = h * 31 + (EchoCancellation ? 1 : 0);
                h = h * 31 + EchoTailMs;
                h = h * 31 + StreamDelayMs;
                h = h * 31 + (int)NoiseSuppression;
                h = h * 31 + (Agc ? 1 : 0);
                h = h * 31 + AgcTargetDbfs.GetHashCode();
                h = h * 31 + AgcMaxGainDb.GetHashCode();
                return h;
            }
        }

        public override string ToString() =>
            $"hp={HighPass} aec={EchoCancellation}/{EchoTailMs}ms(+{StreamDelayMs}) ns={NoiseSuppression} agc={Agc}@{AgcTargetDbfs}dBFS/{AgcMaxGainDb}dB";
    }

    /// <summary>Runtime picture of the capture DSP (mirrors the native <c>DspStats</c>).</summary>
    public struct DspStats
    {
        /// <summary>Echo return loss enhancement of the linear filter (dB) while the far end is active.</summary>
        public float ErleDb;
        /// <summary>Playback→capture delay the echo canceller is aligned to (ms).</summary>
        public int EchoDelayMs;
        /// <summary>The adaptive filter has seen enough far-end audio to have converged.</summary>
        public bool EchoConverged;
        /// <summary>Rendered audio was fed in the last second.</summary>
        public bool FarEndActive;
        /// <summary>Speech probability of the last 10 ms block (0..1).</summary>
        public float SpeechProbability;
        /// <summary>Current AGC gain (dB; 0 when AGC is off).</summary>
        public float AgcGainDb;
        /// <summary>Blocks processed without reference audio while the AEC was on and being fed.</summary>
        public ulong FarEndUnderruns;
    }

    /// <summary>
    /// Capture processing chain (high-pass → echo cancellation → noise suppression → AGC) working on
    /// mono 48 kHz PCM in 10 ms blocks. Two implementations:
    /// <list type="bullet">
    /// <item><see cref="NativeCaptureDsp"/> — the Aurix native core (<c>aurix_dsp_*</c> in <c>aurix_client</c>),
    /// every stage; needs the native binary like <see cref="NativeOpusCodec"/>.</item>
    /// <item><see cref="ManagedCaptureDsp"/> — pure C# high-pass and AGC; echo cancellation and neural
    /// noise suppression are not available and the flags are ignored (see <see cref="SupportsEchoCancellation"/>).</item>
    /// </list>
    /// <see cref="Process"/> is called from the capture thread, <see cref="PushRender"/> from the
    /// audio thread; implementations make that pair safe.
    /// </summary>
    public interface ICaptureProcessor : IDisposable
    {
        /// <summary>Samples per 10 ms block; <see cref="Process"/> takes whole multiples.</summary>
        int BlockSamples { get; }
        bool SupportsEchoCancellation { get; }
        bool SupportsNoiseSuppression { get; }
        /// <summary>Settings in effect (after clamping and stripping unsupported stages).</summary>
        DspSettings Settings { get; }
        void Apply(DspSettings settings);
        DspStats Stats { get; }
        /// <summary>Process <paramref name="frames"/> mono 48 kHz samples of <paramref name="mono"/> in place.</summary>
        void Process(float[] mono, int frames);
        /// <summary>
        /// Echo-canceller reference: 48 kHz interleaved audio the application is playing
        /// (<paramref name="count"/> total samples starting at <paramref name="offset"/>). No-op while
        /// echo cancellation is off or unsupported.
        /// </summary>
        void PushRender(float[] pcm, int offset, int count, int channels);
    }

    /// <summary>Which capture DSP implementation to use.</summary>
    public enum CaptureDspMode
    {
        /// <summary>Native core when its binary loads on this platform, otherwise the managed chain.</summary>
        Auto,
        /// <summary>Native core only (throws when the binary is missing).</summary>
        Native,
        /// <summary>Pure C# chain (high-pass + AGC).</summary>
        Managed,
        /// <summary>No processing at all.</summary>
        Off,
    }

    public static class CaptureDsp
    {
        /// <summary>Samples per 10 ms block at 48 kHz.</summary>
        public const int BlockSamples = AudioFormat.SampleRate / 100;

        /// <summary>Pick an implementation for <paramref name="mode"/>; <c>null</c> for <see cref="CaptureDspMode.Off"/>.</summary>
        /// <exception cref="DllNotFoundException"><see cref="CaptureDspMode.Native"/> and the binary is missing.</exception>
        public static ICaptureProcessor Create(CaptureDspMode mode, DspSettings settings)
        {
            switch (mode)
            {
                case CaptureDspMode.Off: return null;
                case CaptureDspMode.Managed: return new ManagedCaptureDsp(settings);
                case CaptureDspMode.Native: return new NativeCaptureDsp(settings);
                default:
                    return NativeCaptureDsp.IsAvailable ? (ICaptureProcessor)new NativeCaptureDsp(settings) : new ManagedCaptureDsp(settings);
            }
        }
    }

    /// <summary>
    /// Pure C# capture chain: 80 Hz Butterworth high-pass and a speech-gated AGC with a soft limiter
    /// (the same coefficients and time constants as the native core). Echo cancellation and neural
    /// noise suppression need the native core; here they are reported unsupported and left off.
    /// </summary>
    public sealed class ManagedCaptureDsp : ICaptureProcessor
    {
        private const float AgcMinGain = 0.1f;
        private const float LimiterKnee = 0.89f;

        private readonly object _lock = new object();
        private DspSettings _settings;
        private DspStats _stats;
        // High-pass biquad.
        private float _b0, _b1, _b2, _a1, _a2, _z1, _z2;
        // AGC.
        private float _gain = 1f;
        private float _targetRms;
        private float _maxGain;
        // Energy speech gate.
        private float _floor = 1e-6f;

        public ManagedCaptureDsp(DspSettings settings)
        {
            SetHighPass(80f);
            Apply(settings);
        }

        public int BlockSamples => CaptureDsp.BlockSamples;
        public bool SupportsEchoCancellation => false;
        public bool SupportsNoiseSuppression => false;
        public DspSettings Settings { get { lock (_lock) return _settings; } }
        public DspStats Stats { get { lock (_lock) return _stats; } }

        public void Apply(DspSettings settings)
        {
            var s = settings.Clamped();
            s.EchoCancellation = false;
            s.NoiseSuppression = NoiseSuppression.Off;
            lock (_lock)
            {
                bool agcChanged = s.Agc != _settings.Agc || !s.AgcTargetDbfs.Equals(_settings.AgcTargetDbfs) ||
                                  !s.AgcMaxGainDb.Equals(_settings.AgcMaxGainDb);
                if (s.HighPass != _settings.HighPass) { _z1 = 0f; _z2 = 0f; }
                if (agcChanged)
                {
                    _gain = 1f;
                    _targetRms = (float)Math.Pow(10.0, s.AgcTargetDbfs / 20.0);
                    _maxGain = (float)Math.Pow(10.0, s.AgcMaxGainDb / 20.0);
                    if (!s.Agc) _stats.AgcGainDb = 0f;
                }
                _settings = s;
            }
        }

        private void SetHighPass(float cutoffHz)
        {
            double w0 = 2.0 * Math.PI * cutoffHz / AudioFormat.SampleRate;
            double sin = Math.Sin(w0), cos = Math.Cos(w0);
            double alpha = sin / (2.0 * (1.0 / Math.Sqrt(2.0)));
            double a0 = 1.0 + alpha;
            _b0 = (float)((1.0 + cos) / 2.0 / a0);
            _b1 = (float)(-(1.0 + cos) / a0);
            _b2 = (float)((1.0 + cos) / 2.0 / a0);
            _a1 = (float)(-2.0 * cos / a0);
            _a2 = (float)((1.0 - alpha) / a0);
        }

        public void Process(float[] mono, int frames)
        {
            if (mono == null) throw new ArgumentNullException(nameof(mono));
            if (frames <= 0 || frames > mono.Length) throw new ArgumentOutOfRangeException(nameof(frames));
            lock (_lock)
            {
                if (!_settings.AnyEnabled) return;
                int block = BlockSamples;
                for (int off = 0; off + block <= frames; off += block)
                {
                    if (_settings.HighPass) HighPassBlock(mono, off, block);
                    float speech = SpeechProbability(mono, off, block);
                    _stats.SpeechProbability = speech;
                    if (_settings.Agc) AgcBlock(mono, off, block, speech);
                }
            }
        }

        private void HighPassBlock(float[] pcm, int off, int count)
        {
            for (int i = off; i < off + count; i++)
            {
                float x = pcm[i];
                float y = _b0 * x + _z1;
                _z1 = _b1 * x - _a1 * y + _z2;
                _z2 = _b2 * x - _a2 * y;
                pcm[i] = y;
            }
        }

        private float SpeechProbability(float[] pcm, int off, int count)
        {
            double e = 0;
            for (int i = off; i < off + count; i++) e += (double)pcm[i] * pcm[i];
            float energy = (float)(e / count);
            if (energy < _floor) _floor = Math.Max(energy, 1e-10f);
            else _floor += (energy - _floor) * 0.005f;
            return energy > _floor * 8f ? 1f : 0f;
        }

        private void AgcBlock(float[] pcm, int off, int count, float speech)
        {
            double e = 0;
            for (int i = off; i < off + count; i++) e += (double)pcm[i] * pcm[i];
            float rms = (float)Math.Sqrt(e / count);
            if (speech > 0.5f && rms > 1e-4f)
            {
                float desired = Math.Min(Math.Max(_targetRms / rms, AgcMinGain), _maxGain);
                _gain += (desired - _gain) * (desired < _gain ? 0.3f : 0.02f);
            }
            else if (rms * _gain > 0.9f)
            {
                _gain = Math.Max(_gain * 0.7f, AgcMinGain);
            }
            for (int i = off; i < off + count; i++)
            {
                float v = pcm[i] * _gain;
                float a = Math.Abs(v);
                if (a > LimiterKnee)
                {
                    float room = 1f - LimiterKnee;
                    pcm[i] = Math.Sign(v) * (LimiterKnee + room * (float)Math.Tanh((a - LimiterKnee) / room));
                }
                else pcm[i] = v;
            }
            _stats.AgcGainDb = 20f * (float)Math.Log10(_gain);
        }

        public void PushRender(float[] pcm, int offset, int count, int channels) { }

        public void Dispose() { }
    }
}
