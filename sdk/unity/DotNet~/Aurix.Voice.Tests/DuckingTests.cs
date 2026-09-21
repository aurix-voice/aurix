using System;
using System.Collections.Generic;
using System.Text;
using Aurix.Audio;
using Aurix.Protocol;
using Xunit;

namespace Aurix.Voice.Tests
{
    /// <summary>Priority speakers → receiver-local game-audio ducking: client state machine and the envelope.</summary>
    public class DuckingTests
    {
        private static string TokenFor(Guid userId)
        {
            static string B64(string s) => Convert.ToBase64String(Encoding.UTF8.GetBytes(s)).TrimEnd('=').Replace('+', '-').Replace('/', '_');
            return B64("{\"alg\":\"HS256\"}") + "." + B64("{\"user_id\":\"" + userId + "\",\"app_id\":\"x\"}") + ".sig";
        }

        private static void Msg(AurixVoiceClient c, string json) => c.HandleMessage(ControlMessage.Parse(json));

        private static void Speaking(AurixVoiceClient c, Guid channel, Guid user, bool on) =>
            Msg(c, "{\"type\":\"SpeakingStateChanged\",\"data\":{\"channel_id\":\"" + channel + "\",\"user_id\":\"" + user + "\",\"speaking\":" + (on ? "true" : "false") + "}}");

        private static void Priority(AurixVoiceClient c, Guid channel, Guid user, bool on) =>
            Msg(c, "{\"type\":\"PriorityChanged\",\"data\":{\"channel_id\":\"" + channel + "\",\"user_id\":\"" + user + "\",\"priority\":" + (on ? "true" : "false") + "}}");

        private static string Member(Guid user, uint ssrc, string role = "speaker", bool priority = false, bool speaking = false) =>
            "{\"user_id\":\"" + user + "\",\"display_name\":\"" + ssrc + "\",\"ssrc\":" + ssrc + ",\"role\":\"" + role + "\",\"is_priority\":" +
            (priority ? "true" : "false") + ",\"is_speaking\":" + (speaking ? "true" : "false") + "}";

        [Fact]
        public void DuckingConfigParsesAndClamps()
        {
            var m = ControlMessage.Parse("{\"type\":\"ChannelJoinAck\",\"data\":{\"channel_id\":\"" + Guid.NewGuid() +
                "\",\"participants\":[],\"ducking\":{\"gain\":1.7,\"attack_ms\":10,\"release_ms\":900,\"hold_ms\":100,\"moderators\":true},\"priority\":true}}");
            var d = DuckingConfig.FromMessage(m);
            Assert.NotNull(d);
            Assert.Equal(1f, d.Value.Gain);
            Assert.Equal(10, d.Value.AttackMs);
            Assert.Equal(900, d.Value.ReleaseMs);
            Assert.Equal(100, d.Value.HoldMs);
            Assert.True(d.Value.Moderators);
            Assert.Null(DuckingConfig.FromMessage(ControlMessage.Parse("{\"type\":\"ChannelAudioPolicy\",\"data\":{\"channel_id\":\"" + Guid.NewGuid() + "\",\"ducking\":null}}")));
            Assert.Equal(DuckingConfig.Default, DuckingConfig.Default);
            Assert.Equal(0.25f, DuckingConfig.Default.Gain);
            Assert.False(DuckingConfig.Default.Moderators);
        }

        [Fact]
        public void ClientDucksOnPriorityOrModeratorSpeechOfOthersOnly()
        {
            var me = Guid.NewGuid();
            var alice = Guid.NewGuid();
            var bob = Guid.NewGuid();
            var carol = Guid.NewGuid();
            var raid = Guid.NewGuid();
            var client = new AurixVoiceClient("ws://127.0.0.1:1", TokenFor(me));
            Assert.Equal(me, client.LocalUserId);

            var events = new List<(Guid, bool, DuckingConfig)>();
            var priorities = new List<(Guid, Guid, bool)>();
            client.OnDuckingChanged += (c, a, d) => events.Add((c, a, d));
            client.OnParticipantPriorityChanged += (c, u, p) => priorities.Add((c, u, p));

            // Join: Alice is already a priority speaker and already talking → ducked immediately, once.
            Msg(client, "{\"type\":\"ChannelJoinAck\",\"data\":{\"channel_id\":\"" + raid + "\",\"participants\":[" +
                Member(me, 1) + "," + Member(alice, 2, priority: true, speaking: true) + "," + Member(bob, 3, role: "moderator") + "," + Member(carol, 4) +
                "],\"ducking\":{\"gain\":0.3,\"attack_ms\":50,\"release_ms\":300,\"hold_ms\":200,\"moderators\":false}}}");
            Assert.Single(events);
            Assert.True(events[0].Item2);
            Assert.Equal(0.3f, events[0].Item3.Gain);
            Assert.True(client.IsDuckingActive(raid));
            Assert.False(client.IsPriority(raid));

            // Bob (moderator, no flag, `moderators: false`) talking changes nothing; Alice stopping releases.
            Speaking(client, raid, bob, true);
            Assert.Single(events);
            Speaking(client, raid, alice, false);
            Assert.Equal(2, events.Count);
            Assert.False(events[1].Item2);
            Assert.False(client.IsDuckingActive(raid));

            // Two priority speakers: ducked while either talks; released only when both are quiet.
            Priority(client, raid, carol, true);
            Assert.Single(priorities);
            Assert.Equal((raid, carol, true), priorities[0]);
            Assert.Contains(client.GetParticipants(raid), p => p.UserId == carol && p.IsPriority);
            Speaking(client, raid, alice, true);
            Speaking(client, raid, carol, true);
            Assert.Equal(3, events.Count);
            Assert.True(events[2].Item2);
            Speaking(client, raid, alice, false);
            Assert.Equal(3, events.Count);
            Assert.True(client.IsDuckingActive(raid));
            Speaking(client, raid, carol, false);
            Assert.Equal(4, events.Count);
            Assert.False(events[3].Item2);

            // Revoking priority mid-speech releases too; a plain speaker never ducks.
            Speaking(client, raid, carol, true);
            Assert.Equal(5, events.Count);
            Priority(client, raid, carol, false);
            Assert.Equal(6, events.Count);
            Assert.False(events[5].Item2);
            Speaking(client, raid, carol, false);
            Assert.Equal(6, events.Count);

            // Our own priority speech never ducks our own game audio.
            Priority(client, raid, me, true);
            Assert.True(client.IsPriority(raid));
            Speaking(client, raid, me, true);
            Assert.Equal(6, events.Count);
            Assert.False(client.IsDuckingActive(raid));
            Speaking(client, raid, me, false);

            // `moderators: true` arriving via ChannelAudioPolicy makes the talking moderator duck (Bob is still speaking).
            Msg(client, "{\"type\":\"ChannelAudioPolicy\",\"data\":{\"channel_id\":\"" + raid +
                "\",\"ducking\":{\"gain\":0.2,\"attack_ms\":50,\"release_ms\":300,\"hold_ms\":200,\"moderators\":true}}}");
            Assert.Equal(7, events.Count);
            Assert.True(events[6].Item2);
            Assert.True(events[6].Item3.Moderators);
            Assert.Equal(0.2f, events[6].Item3.Gain);

            // The ducking speaker leaving the channel releases.
            Msg(client, "{\"type\":\"ParticipantLeft\",\"data\":{\"channel_id\":\"" + raid + "\",\"user_id\":\"" + bob + "\"}}");
            Assert.Equal(8, events.Count);
            Assert.False(events[7].Item2);

            // Ducking switched off for the channel: nothing further, even with a talking priority speaker.
            Speaking(client, raid, alice, true);
            Assert.Equal(9, events.Count);
            Msg(client, "{\"type\":\"ChannelAudioPolicy\",\"data\":{\"channel_id\":\"" + raid + "\",\"ducking\":null}}");
            Assert.Equal(10, events.Count);
            Assert.False(events[9].Item2);
            Speaking(client, raid, alice, false);
            Speaking(client, raid, alice, true);
            Assert.Equal(10, events.Count);

            // Being kicked out of a ducked channel releases it (and one channel never affects another).
            var lobby = Guid.NewGuid();
            Msg(client, "{\"type\":\"ChannelJoinAck\",\"data\":{\"channel_id\":\"" + lobby + "\",\"participants\":[" + Member(alice, 12, priority: true, speaking: true) +
                "],\"ducking\":{\"gain\":0.5,\"attack_ms\":50,\"release_ms\":300,\"hold_ms\":200}}}");
            Assert.Equal(11, events.Count);
            Assert.Equal(lobby, events[10].Item1);
            Assert.True(client.IsDuckingActive(lobby));
            Assert.False(client.IsDuckingActive(raid));
            Msg(client, "{\"type\":\"Kick\",\"data\":{\"channel_id\":\"" + lobby + "\",\"reason\":\"bye\"}}");
            Assert.Equal(12, events.Count);
            Assert.Equal((lobby, false), (events[11].Item1, events[11].Item2));
            Assert.False(client.IsDuckingActive(lobby));
            client.Dispose();
        }

        [Fact]
        public void UnknownLocalUserNeverSuppressesDucking()
        {
            // Without a user_id claim we cannot recognise our own speech; the server never reports it anyway.
            var client = new AurixVoiceClient("ws://127.0.0.1:1", "opaque-token");
            Assert.Equal(Guid.Empty, client.LocalUserId);
            var alice = Guid.NewGuid();
            var ch = Guid.NewGuid();
            int active = 0;
            client.OnDuckingChanged += (c, a, d) => { if (a) active++; };
            Msg(client, "{\"type\":\"ChannelJoinAck\",\"data\":{\"channel_id\":\"" + ch + "\",\"participants\":[" + Member(alice, 2, priority: true) +
                "],\"ducking\":{\"gain\":0.3,\"attack_ms\":50,\"release_ms\":300,\"hold_ms\":200}}}");
            Speaking(client, ch, alice, true);
            Assert.Equal(1, active);
            client.Dispose();
        }

        [Fact]
        public void EnvelopeRampsHoldsAndReleases()
        {
            var env = new DuckingEnvelope();
            Assert.Equal(1f, env.Gain);
            Assert.False(env.Active);
            Assert.False(env.IsDucking);

            var cfg = new DuckingConfig { Gain = 0.25f, AttackMs = 100, ReleaseMs = 400, HoldMs = 200 };
            env.Set(true, cfg);
            Assert.True(env.Active);
            // Attack: linear over 100 ms, so 50 ms → half depth → gain 1 - 0.5 * 0.75.
            Assert.Equal(0.625f, env.Advance(0.05f), 3);
            Assert.Equal(0.25f, env.Advance(0.05f), 3);
            Assert.Equal(0.25f, env.Advance(1f), 3); // saturates
            Assert.Equal(1f, env.Depth, 3);

            // Stop: held fully ducked for 200 ms, then released over 400 ms.
            env.Set(false, cfg);
            Assert.True(env.Active, "hold counts as active");
            Assert.Equal(0.25f, env.Advance(0.1f), 3);
            Assert.Equal(0.25f, env.Advance(0.05f), 3);
            Assert.True(env.Active);
            // One step crossing hold end → the remainder goes into release: 100 ms of a 400 ms ramp.
            Assert.Equal(0.25f + 0.25f * 0.75f, env.Advance(0.15f), 3);
            Assert.False(env.Active);
            Assert.True(env.IsDucking);
            env.Advance(0.3f);
            Assert.Equal(1f, env.Gain, 3);
            Assert.False(env.IsDucking);

            // Speech resuming during the hold cancels it and stays ducked without re-attacking.
            env.Set(true, cfg);
            env.Advance(1f);
            env.Set(false, cfg);
            env.Advance(0.1f);
            env.Set(true, cfg);
            Assert.Equal(0.25f, env.Advance(0.5f), 3);
            Assert.True(env.Active);

            // A new config mid-flight applies to the remaining ramps; zero-length ramps jump.
            var snap = new DuckingConfig { Gain = 0.5f, AttackMs = 0, ReleaseMs = 0, HoldMs = 0 };
            env.Set(true, snap);
            Assert.Equal(0.5f, env.Advance(0.001f), 3);
            env.Set(false, snap);
            Assert.Equal(1f, env.Advance(0.001f), 3);
            Assert.False(env.Active);

            // Negative / NaN time is ignored; Reset drops everything at once.
            env.Set(true, cfg);
            env.Advance(1f);
            Assert.Equal(0.25f, env.Advance(-1f), 3);
            Assert.Equal(0.25f, env.Advance(float.NaN), 3);
            env.Reset();
            Assert.Equal(1f, env.Gain);
            Assert.False(env.Active);
            Assert.False(env.IsDucking);
        }
    }
}
