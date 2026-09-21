using System;
using System.Threading.Tasks;
using Aurix.Audio;
using Aurix.Protocol;
using Xunit;

namespace Aurix.Voice.Tests
{
    /// <summary>Lip-sync: the viseme frame model, the native analyser wrapper and its lifecycle in the downlink mixer.</summary>
    public class VisemeTests
    {
        private const int N = AudioFormat.FrameSamples;

        /// <summary>A vowel-like signal (150 Hz voice with two formants), <paramref name="channels"/>-interleaved.</summary>
        private static float[] Voiced(int frames, int channels, float amp = 0.2f)
        {
            var v = new float[frames * channels];
            for (int i = 0; i < frames; i++)
            {
                float t = i / (float)AudioFormat.SampleRate;
                float s = amp * ((float)Math.Sin(t * 150f * 2 * Math.PI) + (float)Math.Sin(t * 750f * 2 * Math.PI) + 0.5f * (float)Math.Sin(t * 1200f * 2 * Math.PI));
                for (int c = 0; c < channels; c++) v[i * channels + c] = s;
            }
            return v;
        }

        /// <summary>Decoder whose output is the voiced signal when the payload byte is non-zero, silence otherwise.</summary>
        private sealed class VoicedCodec : IOpusCodec
        {
            public int SampleRate => AudioFormat.SampleRate;
            public int Channels => 1;
            public int Encode(ReadOnlySpan<float> pcm, int frameSamplesPerChannel, Span<byte> output) => 0;
            public int Decode(ReadOnlySpan<byte> opus, Span<float> pcm, int maxFrameSamplesPerChannel)
            {
                if (opus[0] == 0) pcm.Slice(0, N).Fill(0f);
                else Voiced(N, 1).AsSpan().CopyTo(pcm);
                return N;
            }
            public int DecodeLost(Span<float> pcm, int frameSamplesPerChannel) { pcm.Slice(0, frameSamplesPerChannel).Fill(0f); return frameSamplesPerChannel; }
            public void SetBitrate(int bitsPerSecond) { }
            public void Dispose() { }
        }

        [Fact]
        public void FrameModelMatchesTheNativeEnumOrder()
        {
            Assert.Equal(9, VisemeFrame.Count);
            Assert.Equal(new[] { "sil", "PP", "FF", "SS", "aa", "E", "ih", "oh", "ou" }, VisemeFrame.Names);
            Assert.Equal(0, (int)Viseme.Silence);
            Assert.Equal(8, (int)Viseme.OU);
            Assert.Equal(VisemeFrame.Count, Enum.GetValues(typeof(Viseme)).Length);

            var f = VisemeFrame.Silent(42);
            Assert.Equal(42UL, f.Sequence);
            Assert.Equal(Viseme.Silence, f.Dominant);
            Assert.Equal(1f, f.Weight(Viseme.Silence));
            Assert.Equal(1f, f.Confidence);
            Assert.Equal(0f, f.MouthOpen);
            for (int i = 1; i < VisemeFrame.Count; i++) Assert.Equal(0f, f[i]);

            f[(int)Viseme.AA] = 0.7f;
            f.Dominant = Viseme.AA;
            Assert.Equal(0.7f, f.Weight(Viseme.AA));
            var weights = new float[VisemeFrame.Count];
            f.CopyWeights(weights);
            Assert.Equal(0.7f, weights[4]);
            Assert.Equal(1f, weights[0]);
            Assert.Throws<ArgumentException>(() => f.CopyWeights(new float[3]));
            Assert.Throws<ArgumentOutOfRangeException>(() => f[9]);
            Assert.Throws<ArgumentOutOfRangeException>(() => f[-1] = 1f);
        }

        [Fact]
        public void NativeAnalyserTracksVoiceRelaxesAndResets()
        {
            if (!NativeLib.TryLoad() || !NativeVisemeAnalyzer.IsAvailable)
            {
                Assert.Throws<DllNotFoundException>(() => new NativeVisemeAnalyzer());
                var mixer = new RemoteMixer(() => new VoicedCodec());
                Assert.Throws<PlatformNotSupportedException>(() => mixer.VisemesEnabled = true);
                Assert.False(mixer.VisemesEnabled);
                return;
            }

            using var an = new NativeVisemeAnalyzer();
            Assert.Equal(Viseme.Silence, an.Frame.Dominant);
            Assert.Equal(0UL, an.Frame.Sequence);

            an.Push(new float[N], N, 1);
            Assert.Equal(1UL, an.Frame.Sequence);
            Assert.Equal(Viseme.Silence, an.Frame.Dominant);

            // Stereo input is downmixed; the vowel opens the mouth and leaves silence.
            var voiced = Voiced(N, 2);
            for (int i = 0; i < 10; i++) an.Push(voiced, N, 2);
            var talking = an.Frame;
            Assert.Equal(11UL, talking.Sequence);
            Assert.NotEqual(Viseme.Silence, talking.Dominant);
            Assert.True(talking.MouthOpen > 0f && talking.Energy > 0f, $"mouth {talking.MouthOpen} energy {talking.Energy}");
            float sum = 0f;
            for (int i = 0; i < VisemeFrame.Count; i++) sum += talking[i];
            Assert.InRange(sum, 0.9f, 1.1f);

            // A sub-frame chunk (offset into a longer buffer) is accepted and zero-padded.
            var longer = Voiced(N * 2, 1);
            an.Push(longer, N, N / 2, 1);
            Assert.Equal(12UL, an.Frame.Sequence);

            // Relax: no audio → the mouth closes gradually, not at once, and keeps counting.
            an.Relax();
            Assert.Equal(13UL, an.Frame.Sequence);
            float afterOne = an.Frame.MouthOpen;
            for (int i = 0; i < 100; i++) an.Relax();
            Assert.True(an.Frame.MouthOpen <= afterOne, "relaxing reopened the mouth");
            Assert.True(an.Frame.MouthOpen < 0.05f, $"mouth still open after a second of silence: {an.Frame.MouthOpen}");
            Assert.Equal(Viseme.Silence, an.Frame.Dominant);

            // Reset: straight to silence, sequence preserved.
            for (int i = 0; i < 10; i++) an.Push(voiced, N, 2);
            Assert.NotEqual(Viseme.Silence, an.Frame.Dominant);
            ulong before = an.Frame.Sequence;
            an.Reset();
            Assert.Equal(Viseme.Silence, an.Frame.Dominant);
            Assert.Equal(0f, an.Frame.MouthOpen);
            Assert.Equal(before, an.Frame.Sequence);

            Assert.Throws<ArgumentOutOfRangeException>(() => an.Push(voiced, N, 9));
            Assert.Throws<ArgumentOutOfRangeException>(() => an.Push(voiced, N + 1, 2));
            Assert.Throws<ArgumentNullException>(() => an.Push(null, N, 1));
            an.Dispose();
            an.Dispose();
            an.Reset();
            Assert.Throws<ObjectDisposedException>(() => an.Push(voiced, N, 2));
            Assert.Throws<ObjectDisposedException>(() => an.Relax());
        }

        [Fact]
        public void MixerAnalysesDecodedAudioPerStreamAndCleansUp()
        {
            if (!NativeLib.TryLoad() || !NativeVisemeAnalyzer.IsAvailable) return;

            var mixer = new RemoteMixer(() => new VoicedCodec());
            Assert.False(mixer.VisemesEnabled);
            for (uint seq = 0; seq < 6; seq++) mixer.Push(7, seq, 1f, new byte[] { 1 });
            Assert.False(mixer.TryGetVisemes(7, out _));

            // Enabling mid-stream attaches an analyser to the existing stream; frames decoded from now on feed it.
            mixer.VisemesEnabled = true;
            Assert.True(mixer.TryGetVisemes(7, out var idle));
            Assert.Equal(0UL, idle.Sequence);
            Assert.Equal(Viseme.Silence, idle.Dominant);
            Assert.False(mixer.TryGetVisemes(8, out _));

            var output = new float[N];
            for (int i = 0; i < 6; i++) { Array.Clear(output, 0, N); mixer.Mix(output, 1); }
            Assert.True(mixer.TryGetVisemes(7, out var talking));
            Assert.True(talking.Sequence >= 4, $"only {talking.Sequence} frames analysed");
            Assert.NotEqual(Viseme.Silence, talking.Dominant);
            Assert.True(talking.MouthOpen > 0f);
            // Analysis happens before the receiver's volume: a muted-by-volume stream still animates.
            for (uint seq = 6; seq < 12; seq++) mixer.Push(7, seq, 0f, new byte[] { 1 });
            for (int i = 0; i < 6; i++) { Array.Clear(output, 0, N); mixer.Mix(output, 1); }
            Assert.All(output, v => Assert.Equal(0f, v));
            Assert.True(mixer.TryGetVisemes(7, out var quiet));
            Assert.True(quiet.Sequence > talking.Sequence);
            Assert.True(quiet.MouthOpen > 0f, "volume 0 must not stop lip-sync");

            // Starvation: the jitter buffer runs dry; the mouth relaxes (one tick per rendered 20 ms) instead of freezing open.
            ulong seqBefore = quiet.Sequence;
            for (int i = 0; i < 60; i++) { Array.Clear(output, 0, N); mixer.Mix(output, 1); System.Threading.Thread.Sleep(AudioFormat.FrameMs); }
            Assert.True(mixer.TryGetVisemes(7, out var relaxed));
            Assert.True(relaxed.Sequence > seqBefore, "no relax ticks while starved");
            Assert.Equal(Viseme.Silence, relaxed.Dominant);
            Assert.True(relaxed.MouthOpen < 0.05f, $"mouth still open: {relaxed.MouthOpen}");

            // The participant lookup prefers whichever of mic / TTS stream spoke last.
            uint tts = 7 | AurxPacket.SynthSsrcFlag;
            for (uint seq = 0; seq < 4; seq++) mixer.Push(tts, seq, 1f, new byte[] { 1 });
            for (int i = 0; i < 4; i++) { Array.Clear(output, 0, N); mixer.Mix(output, 1); }
            Assert.True(mixer.TryGetParticipantVisemes(7, out var viaTts));
            Assert.NotEqual(Viseme.Silence, viaTts.Dominant);
            Assert.True(mixer.TryGetParticipantVisemes(tts, out var viaFlag));
            Assert.Equal(viaTts.Sequence, viaFlag.Sequence);

            // Retiring the stream disposes its analyser; a new stream on the same SSRC starts fresh.
            mixer.Remove(7);
            Assert.False(mixer.TryGetVisemes(7, out _));
            mixer.Push(7, 100, 1f, new byte[] { 1 });
            Assert.True(mixer.TryGetVisemes(7, out var fresh));
            Assert.Equal(0UL, fresh.Sequence);

            // Switching off frees every analyser; switching on again re-creates them.
            mixer.VisemesEnabled = false;
            Assert.False(mixer.TryGetVisemes(7, out _));
            Assert.False(mixer.TryGetParticipantVisemes(7, out _));
            mixer.VisemesEnabled = true;
            Assert.True(mixer.TryGetVisemes(tts, out _));
            mixer.Dispose();
            Assert.False(mixer.TryGetVisemes(tts, out _));
        }

        [Fact]
        public async Task ClientResolvesParticipantsToStreamsAndKeepsThePreferenceAcrossMixers()
        {
            var client = new AurixVoiceClient("ws://127.0.0.1:1", "token");
            Assert.False(client.VisemesEnabled);
            Assert.Null(client.GetLocalVisemes());
            Assert.Null(client.GetParticipantVisemes(Guid.NewGuid()));
            await client.SetVisemesAsync(false);

            if (!NativeLib.TryLoad() || !NativeVisemeAnalyzer.IsAvailable)
            {
                Assert.False(client.SupportsVisemes);
                await Assert.ThrowsAsync<PlatformNotSupportedException>(() => client.SetVisemesAsync(true));
                Assert.False(client.VisemesEnabled);
                return;
            }
            Assert.True(client.SupportsVisemes);

            var channel = Guid.NewGuid();
            var alice = Guid.NewGuid();
            client.HandleMessage(ControlMessage.Parse(
                "{\"type\":\"ChannelJoinAck\",\"data\":{\"channel_id\":\"" + channel + "\",\"participants\":[{\"user_id\":\"" + alice +
                "\",\"display_name\":\"Alice\",\"ssrc\":7}]}}"));

            var mixer = new RemoteMixer(() => new VoicedCodec());
            client.Mixer = mixer;
            await client.SetVisemesAsync(true);
            Assert.True(client.VisemesEnabled);
            Assert.True(mixer.VisemesEnabled);

            // Local microphone frames handed in by the behaviour animate the local mouth.
            Assert.Equal(Viseme.Silence, client.GetLocalVisemes().Value.Dominant);
            var voiced = Voiced(N, 1);
            for (int i = 0; i < 10; i++) client.AnalyzeLocalVoice(voiced, N, 1);
            Assert.NotEqual(Viseme.Silence, client.GetLocalVisemes().Value.Dominant);
            client.ResetLocalVoice();
            Assert.Equal(Viseme.Silence, client.GetLocalVisemes().Value.Dominant);
            Assert.Equal(10UL, client.GetLocalVisemes().Value.Sequence);

            // Alice: unknown until her stream carries audio, then looked up by SSRC through the roster.
            Assert.Null(client.GetParticipantVisemes(alice));
            for (uint seq = 0; seq < 6; seq++) mixer.Push(7, seq, 1f, new byte[] { 1 });
            var output = new float[N];
            for (int i = 0; i < 6; i++) { Array.Clear(output, 0, N); mixer.Mix(output, 1); }
            var frame = client.GetParticipantVisemes(alice);
            Assert.NotNull(frame);
            Assert.NotEqual(Viseme.Silence, frame.Value.Dominant);
            Assert.Null(client.GetParticipantVisemes(Guid.NewGuid()));

            // A replacement mixer (reconnect) inherits the preference; the old streams are gone.
            var again = new RemoteMixer(() => new VoicedCodec());
            client.Mixer = again;
            Assert.True(again.VisemesEnabled);
            Assert.Null(client.GetParticipantVisemes(alice));

            await client.SetVisemesAsync(false);
            Assert.False(again.VisemesEnabled);
            Assert.Null(client.GetLocalVisemes());
            client.AnalyzeLocalVoice(voiced, N, 1); // no-op when off
            client.Dispose();
        }
    }
}
