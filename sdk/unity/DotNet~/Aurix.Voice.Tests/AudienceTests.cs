using System;
using System.Collections.Generic;
using System.Threading;
using System.Threading.Tasks;
using Aurix.Audio;
using Aurix.Protocol;
using Aurix.Transport;
using Xunit;

namespace Aurix.Voice.Tests
{
    /// <summary>Large channels: audience roles, hidden listeners and the server-mixed downlink.</summary>
    public class AudienceTests
    {
        [Fact]
        public void JoinAckCarriesRoleParticipantCountAndRosterPolicy()
        {
            var client = new AurixVoiceClient("ws://127.0.0.1:1", "token");
            var arena = Guid.NewGuid();
            var squad = Guid.NewGuid();
            Assert.Null(client.GetChannelInfo(arena));
            Assert.False(client.CanSpeakIn(arena));

            client.HandleMessage(ControlMessage.Parse(
                "{\"type\":\"ChannelJoinAck\",\"data\":{\"channel_id\":\"" + arena +
                "\",\"participants\":[{\"user_id\":\"" + Guid.NewGuid() + "\",\"display_name\":\"Caster\",\"ssrc\":9,\"role\":\"speaker\"}]," +
                "\"role\":\"listener\",\"participant_count\":1843,\"hidden_listeners\":true,\"transcription\":true,\"safety_voice\":false}}"));
            var info = client.GetChannelInfo(arena);
            Assert.NotNull(info);
            Assert.Equal(ChannelRole.Listener, info.Value.Role);
            Assert.False(info.Value.CanSpeak);
            Assert.False(client.CanSpeakIn(arena));
            Assert.Equal(1843u, info.Value.ParticipantCount);
            Assert.True(info.Value.HiddenListeners);
            Assert.True(info.Value.Transcription);
            Assert.False(info.Value.SafetyVoice);
            // The roster is what the server chose to show (one caster), not the audience.
            Assert.Single(client.GetParticipants(arena));

            // Older servers omit the fields: a speaker, the roster size as the count, listeners visible.
            client.HandleMessage(ControlMessage.Parse(
                "{\"type\":\"ChannelJoinAck\",\"data\":{\"channel_id\":\"" + squad + "\",\"participants\":[" +
                "{\"user_id\":\"" + Guid.NewGuid() + "\",\"display_name\":\"A\",\"ssrc\":1},{\"user_id\":\"" + Guid.NewGuid() + "\",\"display_name\":\"B\",\"ssrc\":2}]}}"));
            var legacy = client.GetChannelInfo(squad);
            Assert.NotNull(legacy);
            Assert.Equal(ChannelRole.Speaker, legacy.Value.Role);
            Assert.True(client.CanSpeakIn(squad));
            Assert.Equal(2u, legacy.Value.ParticipantCount);
            Assert.False(legacy.Value.HiddenListeners);

            // A kick forgets the channel.
            client.HandleMessage(ControlMessage.Parse(
                "{\"type\":\"Kick\",\"data\":{\"channel_id\":\"" + squad + "\",\"reason\":\"bye\"}}"));
            Assert.Null(client.GetChannelInfo(squad));
            Assert.False(client.CanSpeakIn(squad));
        }

        [Fact]
        public async Task DownlinkModeIsNegotiatedOverControlAndTracked()
        {
            Assert.Equal("{\"type\":\"SetDownlinkMode\",\"data\":{\"mode\":\"mixed\"}}", ControlMessage.SetDownlinkMode(DownlinkMode.Mixed));
            Assert.Equal("{\"type\":\"SetDownlinkMode\",\"data\":{\"mode\":\"streams\"}}", ControlMessage.SetDownlinkMode(DownlinkMode.Streams));
            Assert.Equal(DownlinkMode.Mixed, ControlMessage.Parse("{\"type\":\"DownlinkModeChanged\",\"data\":{\"mode\":\"mixed\"}}").DownlinkMode());
            Assert.Equal(DownlinkMode.Streams, ControlMessage.Parse("{\"type\":\"DownlinkModeChanged\",\"data\":{\"mode\":\"streams\"}}").DownlinkMode());
            Assert.Equal(DownlinkMode.Mixed, ControlMessage.Parse("{\"type\":\"ReceiverPreferences\",\"data\":{\"downlink\":\"mixed\"}}").ReceiverPreferences().Downlink);
            Assert.Equal(DownlinkMode.Streams, ControlMessage.Parse("{\"type\":\"ReceiverPreferences\",\"data\":{}}").ReceiverPreferences().Downlink);

            var client = new AurixVoiceClient("ws://127.0.0.1:1", "token");
            var changes = new List<DownlinkMode>();
            client.OnDownlinkModeChanged += m => changes.Add(m);
            Assert.Equal(DownlinkMode.Streams, client.DownlinkMode);
            Assert.Equal(DownlinkMode.Streams, client.PreferredDownlinkMode);

            // Not connected: the preference is remembered and sent on connect / replayed after a fresh session.
            await client.SetDownlinkModeAsync(DownlinkMode.Mixed);
            Assert.Equal(DownlinkMode.Mixed, client.PreferredDownlinkMode);
            Assert.Equal(DownlinkMode.Streams, client.DownlinkMode);
            Assert.Empty(changes);

            client.HandleMessage(ControlMessage.Parse("{\"type\":\"DownlinkModeChanged\",\"data\":{\"mode\":\"mixed\"}}"));
            Assert.Equal(DownlinkMode.Mixed, client.DownlinkMode);
            Assert.Equal(new[] { DownlinkMode.Mixed }, changes);

            // A resumed session's snapshot is authoritative; an unchanged mode does not fire the event again.
            client.HandleMessage(ControlMessage.Parse("{\"type\":\"ReceiverPreferences\",\"data\":{\"blocked_users\":[],\"local_mutes\":[],\"volumes\":[],\"downlink\":\"mixed\"}}"));
            Assert.Single(changes);
            client.HandleMessage(ControlMessage.Parse("{\"type\":\"ReceiverPreferences\",\"data\":{\"blocked_users\":[],\"local_mutes\":[],\"volumes\":[]}}"));
            Assert.Equal(DownlinkMode.Streams, client.DownlinkMode);
            Assert.Equal(new[] { DownlinkMode.Mixed, DownlinkMode.Streams }, changes);
            // The preference is untouched by what the server reports.
            Assert.Equal(DownlinkMode.Mixed, client.PreferredDownlinkMode);
        }

        [Fact]
        public async Task MixedFramesAreFlaggedOnTheWireAndSurfacedToTheApp()
        {
            Assert.Equal((ushort)0x4000, (ushort)PacketFlags.Mixed);
            var pkt = AurxPacket.Audio(7, 960, 0x8000_1234u, 0xC0FFEE, new byte[] { 0xFC, 1, 2 });
            pkt.Header.Flags |= PacketFlags.Mixed;
            Assert.True(AurxPacket.TryDecode(pkt.Encode(), out var decoded, out var err), err);
            Assert.Equal(PacketFlags.Mixed, decoded.Header.Flags & PacketFlags.Mixed);
            Assert.Equal(PacketFlags.None, decoded.Header.Flags & (PacketFlags.E2ee | PacketFlags.Directional));

            var key = new byte[32];
            for (int i = 0; i < key.Length; i++) key[i] = (byte)(i + 1);
            var tunnel = new FakeTunnel(key);
            using var media = MediaTransport.OverTunnel(tunnel, Guid.NewGuid(), 0xB0B, key);
            await media.BindAsync(CancellationToken.None, 2, 1000);
            tunnel.Deliver(pkt);
            Assert.True(media.TryDequeueAudio(out var audio));
            Assert.True(audio.Mixed);
            Assert.Equal(AudioCodec.Opus, audio.Codec);
            Assert.Equal(0x8000_1234u, audio.SenderSsrc);

            tunnel.Deliver(AurxPacket.Audio(8, 960, 42, 0xC0FFEE, new byte[] { 0xFC, 1, 2 }));
            Assert.True(media.TryDequeueAudio(out var voice));
            Assert.False(voice.Mixed);
        }

        /// <summary>Stereo decoder stub: left decodes to +L, right to −L, so panning by the server is observable.</summary>
        private sealed class StereoStub : IOpusCodec
        {
            public const float Level = 0.4f;
            public int SampleRate => AudioFormat.SampleRate;
            public int Channels => 2;
            public int Encode(ReadOnlySpan<float> pcm, int frameSamplesPerChannel, Span<byte> output) => 0;
            public int Decode(ReadOnlySpan<byte> opus, Span<float> pcm, int maxFrameSamplesPerChannel)
            {
                for (int i = 0; i < AudioFormat.FrameSamples; i++) { pcm[i * 2] = Level; pcm[i * 2 + 1] = -Level; }
                return AudioFormat.FrameSamples;
            }
            public int DecodeLost(Span<float> pcm, int frameSamplesPerChannel)
            {
                pcm.Slice(0, frameSamplesPerChannel * 2).Fill(0f);
                return frameSamplesPerChannel;
            }
            public void SetBitrate(int bitsPerSecond) { }
            public void Dispose() { }
        }

        private sealed class MonoStub : IOpusCodec
        {
            public const float Level = 0.25f;
            public int SampleRate => AudioFormat.SampleRate;
            public int Channels => 1;
            public int Encode(ReadOnlySpan<float> pcm, int frameSamplesPerChannel, Span<byte> output) => 0;
            public int Decode(ReadOnlySpan<byte> opus, Span<float> pcm, int maxFrameSamplesPerChannel)
            {
                pcm.Slice(0, AudioFormat.FrameSamples).Fill(Level);
                return AudioFormat.FrameSamples;
            }
            public int DecodeLost(Span<float> pcm, int frameSamplesPerChannel)
            {
                pcm.Slice(0, frameSamplesPerChannel).Fill(0f);
                return frameSamplesPerChannel;
            }
            public void SetBitrate(int bitsPerSecond) { }
            public void Dispose() { }
        }

        [Fact]
        public void RemoteMixerDecodesMixedStreamsInStereoNextToPerSpeakerOnes()
        {
            int stereoDecoders = 0, monoDecoders = 0;
            var mixer = new RemoteMixer(() => { monoDecoders++; return new MonoStub(); }, () => { stereoDecoders++; return new StereoStub(); });
            const uint mixSsrc = 0x8000_7777u;
            for (uint seq = 0; seq < 12; seq++) mixer.Push(mixSsrc, seq, 1f, null, AudioCodec.Opus, true, new byte[] { 1 });
            for (uint seq = 0; seq < 12; seq++) mixer.Push(42, seq, 1f, null, AudioCodec.Opus, false, new byte[] { 1 });
            Assert.Equal(1, stereoDecoders);
            Assert.Equal(1, monoDecoders);

            var stereo = new float[AudioFormat.FrameSamples * 2];
            for (int i = 0; i < 4; i++) { Array.Clear(stereo, 0, stereo.Length); mixer.Mix(stereo, 2); }
            // Left = mixed L (+0.4) + mono speaker (0.25); right = mixed R (−0.4) + mono speaker (0.25).
            Assert.Equal(StereoStub.Level + MonoStub.Level, stereo[stereo.Length - 2], 3);
            Assert.Equal(-StereoStub.Level + MonoStub.Level, stereo[stereo.Length - 1], 3);

            // Mono output downmixes the stereo mix instead of dropping the right channel.
            var mono = new float[AudioFormat.FrameSamples];
            for (int i = 0; i < 2; i++) { Array.Clear(mono, 0, mono.Length); mixer.Mix(mono, 1); }
            Assert.Equal(MonoStub.Level, mono[mono.Length - 1], 3);

            // Without a stereo factory the mono factory serves mixed frames too (downmix left to the codec).
            var fallback = new RemoteMixer(() => { monoDecoders++; return new MonoStub(); });
            fallback.Push(mixSsrc, 0, 1f, null, AudioCodec.Opus, true, new byte[] { 1 });
            Assert.Equal(2, monoDecoders);

            // The IncomingAudio overload carries the flag through.
            var viaStruct = new RemoteMixer(() => new MonoStub(), () => { stereoDecoders++; return new StereoStub(); });
            viaStruct.Push(new IncomingAudio { SenderSsrc = mixSsrc, Sequence = 0, Volume = 1f, Codec = AudioCodec.Opus, Mixed = true, Payload = new byte[] { 1 } });
            Assert.Equal(2, stereoDecoders);
        }
    }
}
