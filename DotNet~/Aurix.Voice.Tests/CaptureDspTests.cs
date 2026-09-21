using System;
using System.Collections.Generic;
using System.IO;
using System.Runtime.InteropServices;
using Aurix.Audio;
using Xunit;

namespace Aurix.Voice.Tests
{
    public class CaptureDspTests
    {
        private const int Block = CaptureDsp.BlockSamples;
        private const int Frame = AudioFormat.FrameSamples;

        private static float Rms(float[] pcm, int count)
        {
            double e = 0;
            for (int i = 0; i < count; i++) e += (double)pcm[i] * pcm[i];
            return (float)Math.Sqrt(e / count);
        }

        private static float[] Tone(int len, float hz, float amp, ref double phase)
        {
            var v = new float[len];
            for (int i = 0; i < len; i++)
            {
                v[i] = amp * (float)Math.Sin(phase);
                phase += 2.0 * Math.PI * hz / AudioFormat.SampleRate;
            }
            return v;
        }

        [Fact]
        public void SettingsClampLikeTheNativeCore()
        {
            var s = new DspSettings { EchoTailMs = 5000, StreamDelayMs = 9999, AgcTargetDbfs = 3f, AgcMaxGainDb = 99f, NoiseSuppression = (NoiseSuppression)42 }.Clamped();
            Assert.Equal(DspSettings.MaxEchoTailMs, s.EchoTailMs);
            Assert.Equal(DspSettings.MaxStreamDelayMs, s.StreamDelayMs);
            Assert.Equal(-6f, s.AgcTargetDbfs);
            Assert.Equal(40f, s.AgcMaxGainDb);
            Assert.Equal(NoiseSuppression.High, s.NoiseSuppression);
            s = new DspSettings { EchoTailMs = 1, AgcTargetDbfs = float.NaN, AgcMaxGainDb = float.PositiveInfinity }.Clamped();
            Assert.Equal(DspSettings.MinEchoTailMs, s.EchoTailMs);
            Assert.Equal(-18f, s.AgcTargetDbfs);
            Assert.Equal(24f, s.AgcMaxGainDb);
            Assert.Equal(130, new DspSettings { EchoTailMs = 123 }.Clamped().EchoTailMs);
            Assert.True(DspSettings.Default.AnyEnabled);
            Assert.False(DspSettings.Bypass.AnyEnabled);
            Assert.Equal(DspSettings.Default, DspSettings.Default.Clamped());
        }

        [Fact]
        public void ManagedBypassIsIdentity()
        {
            using var dsp = new ManagedCaptureDsp(DspSettings.Bypass);
            double phase = 0;
            var frame = Tone(Frame, 440f, 0.5f, ref phase);
            var copy = (float[])frame.Clone();
            dsp.Process(frame, Frame);
            Assert.Equal(copy, frame);
            Assert.False(dsp.SupportsEchoCancellation);
            Assert.False(dsp.Settings.EchoCancellation);
        }

        [Fact]
        public void ManagedHighPassRemovesDcAndKeepsSpeechBand()
        {
            var s = DspSettings.Bypass;
            s.HighPass = true;
            using var dsp = new ManagedCaptureDsp(s);
            double phase = 0;
            float[] frame = null;
            for (int i = 0; i < 25; i++)
            {
                frame = Tone(Frame, 1000f, 0.3f, ref phase);
                for (int j = 0; j < frame.Length; j++) frame[j] += 0.4f;
                dsp.Process(frame, Frame);
            }
            double mean = 0;
            foreach (var v in frame) mean += v;
            mean /= frame.Length;
            Assert.True(Math.Abs(mean) < 0.01, $"DC left: {mean}");
            float rms = Rms(frame, Frame);
            Assert.InRange(rms, 0.3f * 0.7071f * 0.95f, 0.3f * 0.7071f * 1.05f);
        }

        [Fact]
        public void ManagedAgcNormalisesQuietSpeechAndLimitsLoudInput()
        {
            var s = DspSettings.Bypass;
            s.Agc = true;
            s.AgcTargetDbfs = -18f;
            s.AgcMaxGainDb = 24f;
            using var dsp = new ManagedCaptureDsp(s);
            var rng = new Random(7);
            // Speech-like: noise bursts at -36 dBFS with silent gaps so the energy gate has a floor.
            float[] frame = new float[Frame];
            float last = 0f;
            for (int i = 0; i < 300; i++)
            {
                bool voiced = (i / 10) % 3 != 2;
                for (int j = 0; j < Frame; j++)
                    frame[j] = voiced ? 0.016f * (float)(rng.NextDouble() * 2 - 1) * 1.7f : 0.0005f * (float)(rng.NextDouble() * 2 - 1);
                dsp.Process(frame, Frame);
                if (voiced) last = Rms(frame, Frame);
            }
            float target = (float)Math.Pow(10, -18 / 20.0);
            Assert.InRange(last, target * 0.6f, target * 1.4f);
            Assert.InRange(dsp.Stats.AgcGainDb, 6f, 24f);

            // A full-scale slam never leaves the limiter above 1.
            for (int j = 0; j < Frame; j++) frame[j] = 0.99f * (j % 2 == 0 ? 1f : -1f);
            dsp.Process(frame, Frame);
            foreach (var v in frame) Assert.True(Math.Abs(v) <= 1f);
        }

        [Fact]
        public void ManagedRejectsBadArgumentsAndIgnoresPartialBlocks()
        {
            using var dsp = new ManagedCaptureDsp(DspSettings.Default);
            Assert.Throws<ArgumentNullException>(() => dsp.Process(null, 1));
            Assert.Throws<ArgumentOutOfRangeException>(() => dsp.Process(new float[10], 11));
            var tail = new float[Block + 7];
            for (int i = 0; i < tail.Length; i++) tail[i] = 0.25f;
            dsp.Process(tail, tail.Length);
            for (int i = Block; i < tail.Length; i++) Assert.Equal(0.25f, tail[i]);
            dsp.PushRender(tail, 0, tail.Length, 1); // no-op, must not throw
        }

        [Fact]
        public void FactoryHonoursMode()
        {
            Assert.Null(CaptureDsp.Create(CaptureDspMode.Off, DspSettings.Default));
            using var managed = CaptureDsp.Create(CaptureDspMode.Managed, DspSettings.Default);
            Assert.IsType<ManagedCaptureDsp>(managed);
            using var auto = CaptureDsp.Create(CaptureDspMode.Auto, DspSettings.Default);
            Assert.NotNull(auto);
        }

        [Fact]
        public void NativeDspCancelsEchoAndSuppressesNoise()
        {
            if (!NativeLib.TryLoad() || !NativeCaptureDsp.IsAvailable)
            {
                Assert.Throws<DllNotFoundException>(() => new NativeCaptureDsp(DspSettings.Default));
                return;
            }

            var s = DspSettings.Default;
            s.EchoTailMs = 123;
            using var dsp = new NativeCaptureDsp(s);
            Assert.True(dsp.SupportsEchoCancellation);
            Assert.Equal(130, dsp.Settings.EchoTailMs);
            Assert.Equal(NoiseSuppression.High, dsp.Settings.NoiseSuppression);

            // Stationary noise at -30 dBFS: the neural suppressor must take most of it out.
            var ns = DspSettings.Bypass;
            ns.NoiseSuppression = NoiseSuppression.High;
            dsp.Apply(ns);
            var rng = new Random(3);
            var frame = new float[Frame];
            float inRms = 0f, outRms = 0f;
            for (int i = 0; i < 400; i++)
            {
                for (int j = 0; j < Frame; j++) frame[j] = 0.055f * (float)(rng.NextDouble() * 2 - 1);
                float before = Rms(frame, Frame);
                dsp.Process(frame, Frame);
                if (i >= 300) { inRms += before; outRms += Rms(frame, Frame); }
            }
            Assert.True(outRms < inRms * 0.15f, $"noise not suppressed: in {inRms / 100} out {outRms / 100}");

            // Echo: the render signal comes back with a 30 ms delay and -6 dB; the AEC removes it.
            var aec = DspSettings.Bypass;
            aec.EchoCancellation = true;
            aec.EchoTailMs = 100;
            dsp.Apply(aec);
            var history = new Queue<float>();
            for (int i = 0; i < 1440; i++) history.Enqueue(0f); // 30 ms of silence before the echo starts
            float echoIn = 0f, echoOut = 0f;
            for (int i = 0; i < 500; i++)
            {
                var render = new float[Frame];
                for (int j = 0; j < Frame; j++) render[j] = 0.4f * (float)(rng.NextDouble() * 2 - 1);
                dsp.PushRender(render, 0, Frame, 1);
                for (int j = 0; j < Frame; j++)
                {
                    history.Enqueue(render[j]);
                    frame[j] = 0.5f * history.Dequeue();
                }
                float before = Rms(frame, Frame);
                dsp.Process(frame, Frame);
                if (i >= 400) { echoIn += before; echoOut += Rms(frame, Frame); }
            }
            var stats = dsp.Stats;
            Assert.True(stats.FarEndActive);
            Assert.True(echoOut < echoIn * 0.2f, $"echo not cancelled: in {echoIn / 100} out {echoOut / 100} (erle {stats.ErleDb} dB, delay {stats.EchoDelayMs} ms)");

            // Partial blocks are ignored, bad arguments rejected, dispose is idempotent.
            var tail = new float[Block + 5];
            for (int i = 0; i < tail.Length; i++) tail[i] = 0.2f;
            dsp.Process(tail, tail.Length);
            for (int i = Block; i < tail.Length; i++) Assert.Equal(0.2f, tail[i]);
            Assert.Throws<ArgumentOutOfRangeException>(() => dsp.Process(tail, tail.Length + 1));
            dsp.Dispose();
            dsp.Dispose();
            Assert.Throws<ObjectDisposedException>(() => dsp.Process(tail, Block));
        }
    }
}
