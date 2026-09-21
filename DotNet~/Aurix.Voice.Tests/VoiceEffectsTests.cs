using System;
using System.Threading.Tasks;
using Aurix.Audio;
using Xunit;

namespace Aurix.Voice.Tests
{
    /// <summary>The microphone voice-effects library (native chain behind a safe C# wrapper).</summary>
    public class VoiceEffectsTests
    {
        private const int N = AudioFormat.FrameSamples;

        private static float Rms(float[] pcm, int start, int count, int stride = 1)
        {
            double e = 0;
            int n = 0;
            for (int i = start; n < count; i += stride, n++) e += (double)pcm[i] * pcm[i];
            return (float)Math.Sqrt(e / Math.Max(1, n));
        }

        private static float[] Tone(int frames, int channels, float hz, float amp, ref double phase, int onlyChannel = -1)
        {
            var v = new float[frames * channels];
            for (int i = 0; i < frames; i++)
            {
                float s = amp * (float)Math.Sin(phase);
                phase += 2.0 * Math.PI * hz / AudioFormat.SampleRate;
                for (int c = 0; c < channels; c++)
                    v[i * channels + c] = onlyChannel < 0 || onlyChannel == c ? s : 0f;
            }
            return v;
        }

        [Fact]
        public void PresetsAreNonBypassAndDistinctAndSanitizedLikeTheNativeCore()
        {
            var seen = new System.Collections.Generic.HashSet<VoiceEffectParams>();
            foreach (VoiceEffectPreset p in Enum.GetValues(typeof(VoiceEffectPreset)))
            {
                var e = VoiceEffectParams.Preset(p);
                Assert.False(e.IsBypass, p.ToString());
                Assert.Equal(e, e.Sanitized());
                Assert.True(seen.Add(e), p + " duplicates another preset");
            }
            Assert.True(VoiceEffectParams.Bypass.IsBypass);
            Assert.Throws<ArgumentOutOfRangeException>(() => VoiceEffectParams.Preset((VoiceEffectPreset)99));

            var wild = new VoiceEffectParams
            {
                HighpassHz = 5f, LowpassHz = 99_000f, FormantSemitones = -40f, PitchSemitones = 100f, RingModHz = 9_000f,
                DistortionDrive = 0.5f, TremoloHz = 50f, TremoloDepth = 2f, StaticLevel = -1f, ReverbMix = float.NaN,
                ReverbSize = 1.5f, ReverbDamping = -0.2f,
            }.Sanitized();
            Assert.Equal(VoiceEffectParams.MinFilterHz, wild.HighpassHz);
            Assert.Equal(VoiceEffectParams.MaxFilterHz, wild.LowpassHz);
            Assert.Equal(-VoiceEffectParams.MaxFormantSemitones, wild.FormantSemitones);
            Assert.Equal(VoiceEffectParams.MaxPitchSemitones, wild.PitchSemitones);
            Assert.Equal(VoiceEffectParams.MaxRingModHz, wild.RingModHz);
            Assert.Equal(1f, wild.DistortionDrive); // a drive below unity is meaningless; 0 stays off
            Assert.Equal(VoiceEffectParams.MaxTremoloHz, wild.TremoloHz);
            Assert.Equal(1f, wild.TremoloDepth);
            Assert.Equal(0f, wild.StaticLevel);
            Assert.Equal(0f, wild.ReverbMix);
            Assert.Equal(1f, wild.ReverbSize);
            Assert.Equal(0f, wild.ReverbDamping);

            // NaN / negative corners switch a filter off rather than clamping to 20 Hz.
            Assert.Equal(0f, new VoiceEffectParams { HighpassHz = float.NaN }.Sanitized().HighpassHz);
            Assert.Equal(0f, new VoiceEffectParams { LowpassHz = -300f }.Sanitized().LowpassHz);
            Assert.Equal(0f, new VoiceEffectParams { DistortionDrive = 0f }.Sanitized().DistortionDrive);
            // Tremolo needs both a rate and a depth to do anything.
            Assert.True(new VoiceEffectParams { TremoloHz = 5f }.IsBypass);
            Assert.True(new VoiceEffectParams { TremoloDepth = 0.5f }.IsBypass);
            Assert.False(new VoiceEffectParams { TremoloHz = 5f, TremoloDepth = 0.5f }.IsBypass);
        }

        [Fact]
        public async Task ClientBypassNeedsNoNativeLibraryAndProcessingIsANoOp()
        {
            var client = new AurixVoiceClient("ws://127.0.0.1:1", "token");
            Assert.Equal(VoiceEffectParams.Bypass, client.VoiceEffects);
            await client.SetVoiceEffectsAsync(VoiceEffectParams.Bypass);
            await client.SetVoiceEffectsAsync(new VoiceEffectParams { TremoloHz = 3f }); // depth 0 → still bypass
            Assert.True(client.VoiceEffects.IsBypass);
            var frame = new float[N];
            for (int i = 0; i < N; i++) frame[i] = 0.3f;
            client.ApplyVoiceEffects(frame, N, 1);
            client.AnalyzeLocalVoice(frame, N, 1);
            Assert.All(frame, v => Assert.Equal(0.3f, v));
            Assert.Null(client.GetLocalVisemes());
            if (!NativeLib.TryLoad() || !NativeVoiceEffects.IsAvailable)
            {
                Assert.False(client.SupportsVoiceEffects);
                await Assert.ThrowsAsync<PlatformNotSupportedException>(() => client.SetVoiceEffectsAsync(VoiceEffectPreset.Robot));
                return;
            }
            Assert.True(client.SupportsVoiceEffects);
            await client.SetVoiceEffectsAsync(VoiceEffectPreset.Helium);
            Assert.Equal(VoiceEffectParams.Preset(VoiceEffectPreset.Helium), client.VoiceEffects);
            double phase = 0;
            var voiced = Tone(N, 1, 200f, 0.4f, ref phase);
            var copy = (float[])voiced.Clone();
            client.ApplyVoiceEffects(voiced, N, 1);
            Assert.NotEqual(copy, voiced);
            await client.SetVoiceEffectsAsync(VoiceEffectParams.Bypass);
            Assert.Equal(VoiceEffectParams.Bypass, client.VoiceEffects);
            var untouched = (float[])copy.Clone();
            client.ApplyVoiceEffects(copy, N, 1);
            Assert.Equal(untouched, copy);
            client.Dispose();
        }

        [Fact]
        public void NativeChainProcessesMonoAndStereoWithoutChannelBleed()
        {
            if (!NativeLib.TryLoad() || !NativeVoiceEffects.IsAvailable)
            {
                Assert.Throws<DllNotFoundException>(() => new NativeVoiceEffects(VoiceEffectPreset.Robot));
                return;
            }

            // The native side clamps the same way the managed Sanitized() does.
            using (var clamped = new NativeVoiceEffects(new VoiceEffectParams { PitchSemitones = 100f, HighpassHz = 5f }))
            {
                Assert.Equal(VoiceEffectParams.MaxPitchSemitones, clamped.Params.PitchSemitones);
                Assert.Equal(VoiceEffectParams.MinFilterHz, clamped.Params.HighpassHz);
                Assert.False(clamped.IsBypass);
            }

            using var fx = new NativeVoiceEffects(VoiceEffectPreset.Robot);
            Assert.Equal(VoiceEffectParams.Preset(VoiceEffectPreset.Robot), fx.Params);

            // Mono: a 200 Hz tone is well below the Robot high-pass (200 Hz) + ring modulation (60 Hz)
            // reshapes it; the output differs from the input yet stays bounded and non-silent.
            double phase = 0;
            float[] mono = null, before = null;
            for (int i = 0; i < 10; i++)
            {
                mono = Tone(N, 1, 200f, 0.4f, ref phase);
                before = (float[])mono.Clone();
                fx.Process(mono, N, 1);
            }
            Assert.NotEqual(before, mono);
            Assert.True(Rms(mono, 0, N) > 0.01f, "effect silenced the voice");
            Assert.All(mono, v => Assert.InRange(v, -1f, 1f));

            // Stereo: the chain runs per channel — a signal on the left only never leaks into the right.
            fx.Reset();
            float[] stereo = null;
            for (int i = 0; i < 10; i++)
            {
                stereo = Tone(N, 2, 200f, 0.4f, ref phase, onlyChannel: 0);
                fx.Process(stereo, N, 2);
            }
            Assert.True(Rms(stereo, 0, N, 2) > 0.01f, "left channel silenced");
            Assert.Equal(0f, Rms(stereo, 1, N, 2), 4);

            // Bypass is an exact identity, stateful stages restart on Apply/Reset, arguments are validated.
            fx.Apply(VoiceEffectParams.Bypass);
            Assert.True(fx.IsBypass);
            var plain = Tone(N, 1, 440f, 0.3f, ref phase);
            var copy = (float[])plain.Clone();
            fx.Process(plain, N, 1);
            Assert.Equal(copy, plain);
            Assert.Throws<ArgumentOutOfRangeException>(() => fx.Process(plain, N, 3));
            Assert.Throws<ArgumentOutOfRangeException>(() => fx.Process(plain, N + 1, 1));
            Assert.Throws<ArgumentNullException>(() => fx.Process(null, N, 1));
            fx.Dispose();
            fx.Dispose();
            Assert.Throws<ObjectDisposedException>(() => fx.Process(plain, N, 1));
        }

        [Fact]
        public void GhostReverbCarriesATailAcrossFramesAndResetClearsIt()
        {
            if (!NativeLib.TryLoad() || !NativeVoiceEffects.IsAvailable) return;
            using var fx = new NativeVoiceEffects(VoiceEffectPreset.Ghost);
            double phase = 0;
            for (int i = 0; i < 25; i++) fx.Process(Tone(N, 1, 220f, 0.5f, ref phase), N, 1);
            var silence = new float[N];
            fx.Process(silence, N, 1);
            Assert.True(Rms(silence, 0, N) > 0.001f, "no reverb tail after the voice stopped");
            fx.Reset();
            Array.Clear(silence, 0, N);
            fx.Process(silence, N, 1);
            Assert.True(Rms(silence, 0, N) < 0.0005f, "reset did not clear the tail");
        }
    }
}
