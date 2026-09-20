using System;
using System.Collections.Generic;
using System.Threading;
using System.Threading.Tasks;
using Aurix.Protocol;
using Aurix.WebGL;
using Xunit;

namespace Aurix.Voice.Tests
{
    /// <summary>
    /// A scripted stand-in for the browser half of the bridge (the Web SDK's <c>AurixBridge</c> behind
    /// <c>AurixWebGL.jslib</c>): records every call, answers with canned JSON and lets tests queue events.
    /// </summary>
    internal sealed class FakeBridge : IWebGLBridge
    {
        public readonly List<(int handle, string method, Dictionary<string, object> args, int rid)> Calls =
            new List<(int, string, Dictionary<string, object>, int)>();
        public readonly List<Dictionary<string, object>> Events = new List<Dictionary<string, object>>();
        public readonly Dictionary<string, Func<Dictionary<string, object>, int, string>> Replies =
            new Dictionary<string, Func<Dictionary<string, object>, int, string>>();
        public readonly HashSet<string> Async = new HashSet<string>();
        public Dictionary<string, object> CreateOptions;
        public WebGLSdkStatus Status = WebGLSdkStatus.Ready;
        public string Error;
        public int Loads;
        public int Destroyed;
        public int NextHandle = 7;

        public void LoadSdk(string url) { Loads++; LastUrl = url; }
        public string LastUrl;
        public WebGLSdkStatus SdkStatus => Status;
        public string SdkError => Error;

        public int Create(string optionsJson)
        {
            CreateOptions = MiniJson.AsObject(MiniJson.Parse(optionsJson));
            return NextHandle;
        }

        public string Invoke(int handle, string method, string argsJson, int rid)
        {
            var args = MiniJson.AsObject(MiniJson.Parse(argsJson));
            Calls.Add((handle, method, args, rid));
            if (Replies.TryGetValue(method, out var reply)) return reply(args, rid);
            if (Async.Contains(method)) return "{\"ok\":true,\"pending\":true}";
            return "{\"ok\":true,\"value\":null}";
        }

        public string Drain(int handle)
        {
            var batch = new List<object>(Events);
            Events.Clear();
            return MiniJson.Serialize(batch);
        }

        public void Destroy(int handle) => Destroyed++;

        public void Emit(string type, params (string key, object value)[] fields)
        {
            var e = new Dictionary<string, object> { { "type", type } };
            foreach (var (key, value) in fields) e[key] = value;
            Events.Add(e);
        }

        public void EmitJson(string json) => Events.Add(MiniJson.AsObject(MiniJson.Parse(json)));

        public void Resolve(int rid, object value) => Emit("result", ("rid", rid), ("ok", true), ("value", value));

        public void Reject(int rid, string message, string code = null)
        {
            var error = new Dictionary<string, object> { { "message", message }, { "name", "Error" } };
            if (code != null) error["code"] = code;
            Emit("result", ("rid", rid), ("ok", false), ("error", error));
        }

        public (int handle, string method, Dictionary<string, object> args, int rid) Last(string method)
        {
            for (int i = Calls.Count - 1; i >= 0; i--)
                if (Calls[i].method == method) return Calls[i];
            throw new Xunit.Sdk.XunitException($"no {method} call; calls: {string.Join(",", Calls.ConvertAll(c => c.method))}");
        }
    }

    public class WebGLBridgeTests
    {
        private static readonly Guid Channel = Guid.Parse("11111111-1111-1111-1111-111111111111");
        private static readonly Guid Alice = Guid.Parse("aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa");
        private static readonly Guid Bob = Guid.Parse("bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb");

        private static (AurixWebGLVoiceClient client, FakeBridge bridge) NewClient()
        {
            var bridge = new FakeBridge();
            bridge.Async.Add("connect");
            bridge.Async.Add("joinChannel");
            bridge.Async.Add("sendMessage");
            bridge.Async.Add("history");
            bridge.Async.Add("speak");
            bridge.Async.Add("getStats");
            bridge.Async.Add("enumerateDevices");
            var client = new AurixWebGLVoiceClient("http://api", "ws://ws/ws", "jwt", bridge);
            return (client, bridge);
        }

        private static Dictionary<string, object> SessionJson(string id = "22222222-2222-2222-2222-222222222222", string endpoint = "ws://node-a/ws") => new Dictionary<string, object>
        {
            { "sessionId", id }, { "userId", Alice.ToString() }, { "ssrc", 1234.0 }, { "endpoint", endpoint },
            { "failover", new List<object> { "ws://node-b/ws" } }, { "resumed", false }, { "migrated", false },
        };

        private static Dictionary<string, object> ParticipantJson(Guid user, string name, bool speaking = false) => new Dictionary<string, object>
        {
            { "userId", user.ToString() }, { "displayName", name }, { "ssrc", 99.0 }, { "role", "speaker" },
            { "muted", false }, { "serverMuted", false }, { "speaking", speaking }, { "energy", 0.25 },
        };

        private static async Task<SessionInfo> Connect(AurixWebGLVoiceClient client, FakeBridge bridge)
        {
            var connect = client.ConnectAsync();
            var rid = bridge.Last("connect").rid;
            bridge.Emit("connectionState", ("state", "connecting"));
            bridge.Emit("connectionState", ("state", "connected"));
            bridge.Emit("sessionReady", ("info", SessionJson()));
            bridge.Resolve(rid, SessionJson());
            client.Update();
            return await connect;
        }

        [Fact]
        public void ConnectCreatesTheBrowserClientWithTheOptionsAndTokenFlags()
        {
            var (client, bridge) = NewClient();
            client.Options.Stereo = true;
            client.Options.UseTurn = true;
            client.Options.InputDeviceId = "mic-2";
            client.Options.RawMessages = true;
            client.TokenRefresher = _ => Task.FromResult("fresh");
            _ = client.ConnectAsync();

            var o = bridge.CreateOptions;
            Assert.Equal("http://api", MiniJson.GetString(o, "apiUrl"));
            Assert.Equal("ws://ws/ws", MiniJson.GetString(o, "wsUrl"));
            Assert.Equal("jwt", MiniJson.GetString(o, "token"));
            Assert.True(MiniJson.GetBool(o, "refreshToken"));
            Assert.False(MiniJson.GetBool(o, "joinToken"));
            Assert.True(MiniJson.GetBool(o, "useTurn"));
            Assert.True(MiniJson.GetBool(o, "rawMessages"));
            Assert.Equal("mic-2", MiniJson.GetString(o, "inputDeviceId"));
            Assert.Equal(2, (int)MiniJson.GetNumber(MiniJson.AsObject(o["audioConstraints"]), "channelCount"));
            Assert.True(MiniJson.GetBool(MiniJson.AsObject(o["opus"]), "stereo"));
            Assert.Equal(15000, (int)MiniJson.GetNumber(o, "pingIntervalMs"));
            Assert.Equal(7, client.Handle);
            Assert.Equal(7, bridge.Last("connect").handle);
            Assert.Equal(0, bridge.Loads);
            Assert.False(o.ContainsKey("participantStreams"), "default: as many dedicated tracks as the node allows");
            Assert.False(o.ContainsKey("spatialAudio"), "default: HRTF");
            Assert.False(o.ContainsKey("e2ee"), "default: E2EE capability announced when the browser supports it");
        }

        [Fact]
        public void E2eeOptionsReachTheBrowserClient()
        {
            var (off, bridgeOff) = NewClient();
            off.Options.E2ee = false;
            _ = off.ConnectAsync();
            Assert.False(MiniJson.GetBool(bridgeOff.CreateOptions, "e2ee", true));

            var (client, bridge) = NewClient();
            var secret = new byte[32];
            for (int i = 0; i < secret.Length; i++) secret[i] = (byte)i;
            client.Options.E2eeIdentity = secret;
            client.Options.E2eeTransform = WebGLE2eeTransform.EncodedStreams;
            client.Options.E2eeWorkerUrl = "/aurix-e2ee.js";
            _ = client.ConnectAsync();
            var e2ee = MiniJson.AsObject(bridge.CreateOptions["e2ee"]);
            Assert.Equal(Convert.ToBase64String(secret), MiniJson.GetString(e2ee, "identity"));
            Assert.Equal("streams", MiniJson.GetString(e2ee, "transform"));
            Assert.Equal("/aurix-e2ee.js", MiniJson.GetString(e2ee, "workerUrl"));

            var (bad, _) = NewClient();
            bad.Options.E2eeIdentity = new byte[31];
            Assert.Throws<ArgumentException>(() => bad.Options.ToBridge("a", "w", "t", false, false));
        }

        [Fact]
        public async Task E2eeCallsAndEventsUseTheSharedTypes()
        {
            var (client, bridge) = NewClient();
            Assert.False(client.E2eeAvailable);
            Assert.Null(client.E2eeFingerprint);
            Assert.Null(client.ExportE2eeIdentity());
            Assert.Empty(client.GetE2eeDecryptablePeers());

            bridge.Replies["e2eeAvailable"] = (_, __) => "{\"ok\":true,\"value\":true}";
            bridge.Replies["e2eeTransformApi"] = (_, __) => "{\"ok\":true,\"value\":\"script\"}";
            bridge.Replies["e2eeFingerprint"] = (_, __) => "{\"ok\":true,\"value\":\"ab12\"}";
            bridge.Replies["e2eeGeneration"] = (_, __) => "{\"ok\":true,\"value\":3}";
            bridge.Replies["e2eeIdentitySecret"] = (_, __) => "{\"ok\":true,\"value\":\"" + Convert.ToBase64String(new byte[32]) + "\"}";
            bridge.Replies["e2eePeerFingerprint"] = (a, __) =>
                MiniJson.GetString(a, "userId") == Alice.ToString() ? "{\"ok\":true,\"value\":\"cd34\"}" : "{\"ok\":true,\"value\":null}";
            bridge.Replies["isE2eePeerDecryptable"] = (_, __) => "{\"ok\":true,\"value\":true}";
            bridge.Replies["e2eeDecryptablePeers"] = (_, __) => "{\"ok\":true,\"value\":[\"" + Alice + "\",\"" + Bob + "\"]}";
            bridge.Replies["isChannelEncrypted"] = (_, __) => "{\"ok\":true,\"value\":true}";
            bridge.Async.Add("refreshE2eeStats");
            bridge.Async.Add("rotateE2eeKey");
            await Connect(client, bridge);

            Assert.True(client.E2eeAvailable);
            Assert.Equal("script", client.E2eeTransformApi);
            Assert.Equal("ab12", client.E2eeFingerprint);
            Assert.Equal(3, client.E2eeGeneration);
            Assert.Equal(new byte[32], client.ExportE2eeIdentity());
            Assert.Equal("cd34", client.E2eePeerFingerprint(Alice));
            Assert.Null(client.E2eePeerFingerprint(Bob));
            Assert.True(client.IsE2eePeerDecryptable(Bob));
            Assert.Equal(new[] { Alice, Bob }, client.GetE2eeDecryptablePeers());
            Assert.True(client.IsChannelEncrypted(Channel));
            Assert.Equal(Channel.ToString(), MiniJson.GetString(bridge.Last("isChannelEncrypted").args, "channelId"));

            var stats = client.GetE2eeStatsAsync();
            bridge.Resolve(bridge.Last("refreshE2eeStats").rid,
                new Dictionary<string, object> { { "framesE2ee", 120.0 }, { "undecryptable", 2.0 }, { "held", 5.0 } });
            client.Update();
            var s = await stats;
            Assert.Equal(120, s.FramesE2ee);
            Assert.Equal(2, s.Undecryptable);
            Assert.Equal(5, s.Held);

            var rotate = client.RotateE2eeKeyAsync();
            bridge.Resolve(bridge.Last("rotateE2eeKey").rid, 4.0);
            client.Update();
            Assert.Equal(4, await rotate);

            var keys = new List<(Guid, string, string)>();
            var decryptable = new List<(Guid, bool)>();
            var generations = new List<int>();
            client.OnE2eePeerKey += (u, fp, prev) => keys.Add((u, fp, prev));
            client.OnE2eePeerDecryptable += (u, d) => decryptable.Add((u, d));
            client.OnE2eeKeyRotated += g => generations.Add(g);
            bridge.Emit("e2eePeerKey", ("userId", Alice.ToString()), ("fingerprint", "cd34"), ("previousFingerprint", null));
            bridge.Emit("e2eePeerKey", ("userId", Alice.ToString()), ("fingerprint", "ef56"), ("previousFingerprint", "cd34"));
            bridge.Emit("e2eePeerDecryptable", ("userId", Alice.ToString()), ("decryptable", true));
            bridge.Emit("e2eePeerDecryptable", ("userId", Alice.ToString()), ("decryptable", false));
            bridge.Emit("e2eeKeyRotated", ("generation", 5.0));
            client.Update();
            Assert.Equal(new[] { (Alice, "cd34", (string)null), (Alice, "ef56", "cd34") }, keys);
            Assert.Equal(new[] { (Alice, true), (Alice, false) }, decryptable);
            Assert.Equal(new[] { 5 }, generations);
        }

        [Fact]
        public void ParticipantStreamOptionsReachTheBrowserClient()
        {
            var (client, bridge) = NewClient();
            client.Options.ParticipantStreams = 4;
            client.Options.SpatialAudio = WebGLSpatialAudio.EqualPower;
            _ = client.ConnectAsync();
            Assert.Equal(4, (int)MiniJson.GetNumber(bridge.CreateOptions, "participantStreams"));
            Assert.Equal("equalpower", MiniJson.GetString(bridge.CreateOptions, "spatialAudio"));

            var (mixedOnly, bridge2) = NewClient();
            mixedOnly.Options.ParticipantStreams = -3;
            mixedOnly.Options.SpatialAudio = WebGLSpatialAudio.None;
            _ = mixedOnly.ConnectAsync();
            Assert.Equal(0, (int)MiniJson.GetNumber(bridge2.CreateOptions, "participantStreams"));
            Assert.False(MiniJson.GetBool(bridge2.CreateOptions, "spatialAudio", true));
        }

        [Fact]
        public async Task ParticipantStreamCallsAndEventsUseTheSharedTypes()
        {
            var (client, bridge) = NewClient();
            bridge.Async.Add("participantStreamCap");
            bridge.Async.Add("participantStreams");
            await Connect(client, bridge);

            await client.SetPinnedParticipantsAsync(new[] { Alice, Bob });
            var ids = MiniJson.AsArray(bridge.Last("setPinnedParticipants").args["userIds"]);
            Assert.Equal(new[] { Alice.ToString(), Bob.ToString() }, ids.ConvertAll(x => (string)x));

            var cap = client.GetParticipantStreamCapAsync();
            bridge.Resolve(bridge.Last("participantStreamCap").rid, 16.0);
            client.Update();
            Assert.Equal(16, await cap);

            var layoutJson = new List<object>
            {
                new Dictionary<string, object> { { "mid", "1" }, { "userId", Bob.ToString() }, { "live", true } },
                new Dictionary<string, object> { { "mid", "2" }, { "userId", null }, { "live", false } },
            };
            var layout = client.GetParticipantStreamsAsync();
            bridge.Resolve(bridge.Last("participantStreams").rid, layoutJson);
            client.Update();
            var streams = await layout;
            Assert.Equal(2, streams.Count);
            Assert.Equal("1", streams[0].Mid);
            Assert.Equal(Bob, streams[0].UserId);
            Assert.True(streams[0].Live);
            Assert.Null(streams[1].UserId);
            Assert.False(streams[1].Live);

            IReadOnlyList<WebGLParticipantStream> fromEvent = null;
            client.OnParticipantStreams += s => fromEvent = s;
            bridge.Emit("participantStreams", ("streams", layoutJson));
            client.Update();
            Assert.Equal(Bob, fromEvent[0].UserId);

            bridge.Replies["isParticipantSpatialized"] = (_, __) => "{\"ok\":true,\"value\":true}";
            Assert.True(client.IsParticipantSpatialized(Bob));
            Assert.Equal(Bob.ToString(), MiniJson.GetString(bridge.Last("isParticipantSpatialized").args, "userId"));
        }

        [Fact]
        public async Task ConnectWaitsForTheSdkBundleAndFailsWhenItDoesNotLoad()
        {
            var (client, bridge) = NewClient();
            bridge.Status = WebGLSdkStatus.NotLoaded;
            client.SdkUrl = "sdk/aurix.js";
            var connect = client.ConnectAsync();
            Assert.Equal(1, bridge.Loads);
            Assert.Equal("sdk/aurix.js", bridge.LastUrl);
            Assert.False(client.IsCreated);
            bridge.Status = WebGLSdkStatus.Loading;
            client.Update();
            Assert.False(connect.IsCompleted);

            bridge.Status = WebGLSdkStatus.Failed;
            bridge.Error = "failed to load sdk/aurix.js";
            client.Update();
            var e = await Assert.ThrowsAsync<WebGLBridgeException>(() => connect);
            Assert.Equal("failed to load sdk/aurix.js", e.Message);
            Assert.False(client.IsCreated);

            bridge.Status = WebGLSdkStatus.NotLoaded;
            var retry = client.ConnectAsync();
            Assert.Equal(2, bridge.Loads);
            bridge.Status = WebGLSdkStatus.Ready;
            client.Update();
            for (int i = 0; i < 200 && !client.IsCreated; i++) await Task.Delay(5);
            Assert.True(client.IsCreated);
            Assert.False(retry.IsCompleted);
            bridge.Resolve(bridge.Last("connect").rid, SessionJson());
            client.Update();
            Assert.Equal(Guid.Parse("22222222-2222-2222-2222-222222222222"), (await retry).SessionId);
        }

        [Fact]
        public async Task ConnectCompletesFromTheResultEventAndTracksSessionAndState()
        {
            var (client, bridge) = NewClient();
            var states = new List<VoiceConnectionState>();
            SessionInfo ready = null;
            client.OnStateChanged += states.Add;
            client.OnSessionReady += s => ready = s;

            var session = await Connect(client, bridge);
            Assert.Equal(1234u, session.Ssrc);
            Assert.Equal(new[] { VoiceConnectionState.Connecting, VoiceConnectionState.Connected }, states);
            Assert.Same(client.Session, ready);
            Assert.Equal(Alice, client.UserId);
            Assert.Equal("ws://node-a/ws", client.Endpoint);
            Assert.Equal(new[] { "ws://node-b/ws" }, client.FailoverEndpoints);

            bridge.Emit("connectionState", ("state", "media-connected"));
            client.Update();
            Assert.Equal(VoiceConnectionState.MediaBound, client.State);
        }

        [Fact]
        public async Task AsyncFailuresSurfaceTheBrowserErrorWithItsCode()
        {
            var (client, bridge) = NewClient();
            await Connect(client, bridge);
            var join = client.JoinChannelAsync(Channel);
            var call = bridge.Last("joinChannel");
            Assert.Equal(Channel.ToString(), MiniJson.GetString(call.args, "channelId"));
            bridge.Reject(call.rid, "channel is full", "CHANNEL_FULL");
            client.Update();
            var e = await Assert.ThrowsAsync<WebGLBridgeException>(() => join);
            Assert.Equal("CHANNEL_FULL", e.Code);
            Assert.Equal("channel is full", e.Message);
        }

        [Fact]
        public async Task SynchronousBridgeErrorsFaultTheTask()
        {
            var (client, bridge) = NewClient();
            await Connect(client, bridge);
            bridge.Replies["setParticipantVolume"] = (_, __) => "{\"ok\":false,\"error\":{\"message\":\"volume must be 0..2\",\"name\":\"RangeError\"}}";
            var e = await Assert.ThrowsAsync<WebGLBridgeException>(() => client.SetParticipantVolumeAsync(Bob, 9f));
            Assert.Equal("RangeError", e.Name);
            Assert.Null(e.Code);
        }

        [Fact]
        public async Task JoinTracksTheRosterThroughParticipantEvents()
        {
            var (client, bridge) = NewClient();
            await Connect(client, bridge);
            var joined = new List<(Guid, Participant)>();
            var left = new List<(Guid, Participant)>();
            var speaking = new List<(Participant, bool)>();
            client.OnParticipantJoined += (c, p) => joined.Add((c, p));
            client.OnParticipantLeft += (c, p) => left.Add((c, p));
            client.OnSpeaking += (_, p, s) => speaking.Add((p, s));

            var join = client.JoinChannelAsync(Channel, "join-token");
            var call = bridge.Last("joinChannel");
            Assert.Equal("join-token", MiniJson.GetString(call.args, "joinToken"));
            var roster = new List<object> { ParticipantJson(Alice, "alice"), ParticipantJson(Bob, "bob") };
            bridge.Emit("channelJoined", ("channelId", Channel.ToString()), ("participants", roster));
            bridge.Resolve(call.rid, roster);
            client.Update();
            var participants = await join;
            Assert.Equal(2, participants.Count);
            Assert.Equal(new[] { Channel }, client.JoinedChannels);
            Assert.Equal("bob", client.FindByUser(Bob).DisplayName);
            Assert.Equal(0.25f, client.FindByUser(Bob).Energy);

            var carol = Guid.NewGuid();
            bridge.Emit("participantJoined", ("channelId", Channel.ToString()), ("participant", ParticipantJson(carol, "carol")));
            bridge.Emit("speaking", ("channelId", Channel.ToString()), ("userId", carol.ToString()), ("speaking", true));
            bridge.Emit("participantUpdated", ("channelId", Channel.ToString()), ("participant", ParticipantJson(Bob, "bobby")));
            bridge.Emit("participantLeft", ("channelId", Channel.ToString()), ("userId", Alice.ToString()));
            client.Update();
            Assert.Single(joined);
            Assert.Equal("carol", joined[0].Item2.DisplayName);
            Assert.Single(speaking);
            Assert.Same(client.FindByUser(carol), speaking[0].Item1);
            Assert.True(client.FindByUser(carol).IsSpeaking);
            Assert.Equal("bobby", client.FindByUser(Bob).DisplayName);
            Assert.Single(left);
            Assert.Equal("alice", left[0].Item2.DisplayName);
            Assert.Equal(2, client.GetParticipants(Channel).Count);

            await client.LeaveChannelAsync(Channel);
            Assert.Equal(Channel.ToString(), MiniJson.GetString(bridge.Last("leaveChannel").args, "channelId"));
            bridge.Emit("channelLeft", ("channelId", Channel.ToString()));
            client.Update();
            Assert.Empty(client.JoinedChannels);
            Assert.Null(client.FindByUser(Bob));
        }

        [Fact]
        public async Task TokenRequestsAreAnsweredThroughTheCSharpCallbacksOnUpdate()
        {
            var (client, bridge) = NewClient();
            var refreshes = 0;
            var joinTokensFor = new List<Guid>();
            client.TokenRefresher = _ => { refreshes++; return Task.FromResult("jwt-2"); };
            client.JoinTokenProvider = (c, _) => { joinTokensFor.Add(c); return Task.FromException<string>(new InvalidOperationException("no token for you")); };
            await Connect(client, bridge);
            Assert.True(MiniJson.GetBool(bridge.CreateOptions, "joinToken"));

            bridge.Emit("tokenRequest", ("requestId", 1.0), ("kind", "refresh"), ("channelId", null));
            bridge.Emit("tokenRequest", ("requestId", 2.0), ("kind", "join"), ("channelId", Channel.ToString()));
            client.Update();
            Assert.Equal(1, refreshes);
            Assert.Equal(new[] { Channel }, joinTokensFor);
            client.Update();
            var answers = bridge.Calls.FindAll(c => c.method == "provideToken");
            Assert.Equal(2, answers.Count);
            Assert.Equal(1, (int)MiniJson.GetNumber(answers[0].args, "requestId"));
            Assert.Equal("jwt-2", MiniJson.GetString(answers[0].args, "token"));
            Assert.Equal(2, (int)MiniJson.GetNumber(answers[1].args, "requestId"));
            Assert.Equal("no token for you", MiniJson.GetString(answers[1].args, "error"));
            Assert.False(answers[1].args.ContainsKey("token"));
        }

        [Fact]
        public async Task MissingTokenProvidersRejectTheRequestInstead()
        {
            var (client, bridge) = NewClient();
            await Connect(client, bridge);
            bridge.Emit("tokenRequest", ("requestId", 5.0), ("kind", "refresh"), ("channelId", null));
            client.Update();
            var answer = bridge.Last("provideToken");
            Assert.Equal(5, (int)MiniJson.GetNumber(answer.args, "requestId"));
            Assert.Equal("no token refresher", MiniJson.GetString(answer.args, "error"));
        }

        [Fact]
        public async Task ReceiverControlsMapToBridgeArguments()
        {
            var (client, bridge) = NewClient();
            await Connect(client, bridge);

            client.SetMuted(true);
            Assert.True(client.IsMuted);
            Assert.True(MiniJson.GetBool(bridge.Last("setMuted").args, "muted"));

            await client.SetParticipantMutedAsync(Bob, true, Channel);
            var mute = bridge.Last("setParticipantMuted").args;
            Assert.Equal(Bob.ToString(), MiniJson.GetString(mute, "userId"));
            Assert.Equal(Channel.ToString(), MiniJson.GetString(mute, "channelId"));

            await client.SetParticipantVolumeAsync(Bob, 0.5f);
            Assert.Equal(0.5, MiniJson.GetNumber(bridge.Last("setParticipantVolume").args, "volume"));

            await client.TransmitToChannelAsync(Channel);
            var mode = MiniJson.AsObject(bridge.Last("setTransmission").args["mode"]);
            Assert.Equal("single", MiniJson.GetString(mode, "type"));
            Assert.Equal(Channel.ToString(), MiniJson.GetString(mode, "channelId"));
            Assert.True(client.TransmitsTo(Channel));
            Assert.False(client.TransmitsTo(Guid.NewGuid()));

            await client.SetChannelFocusAsync(Channel);
            Assert.Equal(Channel, client.FocusChannel);
            await client.SetChannelFocusAsync(null);
            Assert.Empty(bridge.Last("setChannelFocus").args);

            bridge.Replies["isUserBlocked"] = (_, __) => "{\"ok\":true,\"value\":true}";
            Assert.True(client.IsUserBlocked(Bob));
            bridge.Replies["getParticipantVolume"] = (_, __) => "{\"ok\":true,\"value\":0.25}";
            Assert.Equal(0.25f, client.GetParticipantVolume(Bob));
            bridge.Replies["channelInfo"] = (_, __) => "{\"ok\":true,\"value\":{\"role\":\"listener\",\"participantCount\":42,\"hiddenListeners\":true,\"transcription\":false,\"safetyVoice\":true}}";
            var info = client.GetChannelInfo(Channel);
            Assert.True(info.HasValue);
            Assert.Equal(ChannelRole.Listener, info.Value.Role);
            Assert.Equal(42u, info.Value.ParticipantCount);
            Assert.True(info.Value.HiddenListeners);
            Assert.True(info.Value.SafetyVoice);

            bridge.Emit("receiverPreferences", ("prefs", new Dictionary<string, object>
            {
                { "transmission", new Dictionary<string, object> { { "type", "none" } } },
                { "focusChannel", null },
                { "blockedUsers", new List<object> { Bob.ToString() } },
                { "localMutes", new List<object>() },
                { "volumes", new List<object>() },
            }));
            bridge.Emit("transmissionChanged", ("mode", new Dictionary<string, object> { { "mode", "single" }, { "channel_id", Channel.ToString() } }));
            ReceiverPreferences prefs = null;
            client.OnReceiverPreferences += p => prefs = p;
            client.Update();
            Assert.Contains(Bob, prefs.BlockedUsers);
            Assert.Null(client.FocusChannel);
            Assert.Equal(TransmissionMode.Single(Channel), client.Transmission);
        }

        [Fact]
        public async Task TranslationCallsAndEventsUseTheSharedTypes()
        {
            var (client, bridge) = NewClient();
            await Connect(client, bridge);
            Assert.Null(client.Session.Translation);

            await client.SetTranslationAsync(" DE_de ", "EN", speech: true);
            var call = bridge.Last("setTranslation");
            Assert.Equal("de-de", MiniJson.GetString(call.args, "language"));
            Assert.Equal("en", MiniJson.GetString(call.args, "spokenLanguage"));
            Assert.True(MiniJson.GetBool(call.args, "speech"));
            Assert.Equal("de-de", client.TranslationPrefs.Language);

            // Speech without a target is meaningless and not requested.
            await client.SetTranslationAsync(null, null, speech: true);
            call = bridge.Last("setTranslation");
            Assert.Null(call.args["language"]);
            Assert.False(MiniJson.GetBool(call.args, "speech"));
            Assert.False(client.TranslationPrefs.Speech);

            TranslationPrefs applied = null;
            client.OnTranslationChanged += p => applied = p;
            bridge.Emit("translationChanged", ("prefs", new Dictionary<string, object> { { "language", "fr" }, { "speech", false } }));
            client.Update();
            Assert.Equal("fr", applied.Language);
            Assert.Null(applied.SpokenLanguage);
            Assert.Equal("fr", client.TranslationPrefs.Language);

            Transcript transcript = null;
            client.OnTranscript += t => transcript = t;
            bridge.Emit("transcript", ("transcript", new Dictionary<string, object>
            {
                { "id", "44444444-4444-4444-4444-444444444444" }, { "channelId", Channel.ToString() }, { "userId", Bob.ToString() },
                { "text", "Bonjour" }, { "language", "fr" }, { "startedAt", "2024-05-01T10:00:00.000Z" }, { "durationMs", 800.0 },
                { "original", new Dictionary<string, object> { { "text", "hello" }, { "language", "en" } } },
            }));
            client.Update();
            Assert.True(transcript.Translated);
            Assert.Equal("Bonjour", transcript.Text);
            Assert.Equal("hello", transcript.OriginalText);
            Assert.Equal("en", transcript.OriginalLanguage);
        }

        [Fact]
        public async Task ChatCallsAndEventsUseTheSharedTypes()
        {
            var (client, bridge) = NewClient();
            await Connect(client, bridge);

            var send = client.SendMessageAsync(Channel, "hi", new Dictionary<string, object> { { "k", "v" } }, "ref-1");
            var call = bridge.Last("sendMessage");
            Assert.Equal("hi", MiniJson.GetString(call.args, "text"));
            Assert.Equal("ref-1", MiniJson.GetString(call.args, "clientRef"));
            Assert.Equal("v", MiniJson.GetString(MiniJson.AsObject(call.args["metadata"]), "k"));
            var msg = new Dictionary<string, object>
            {
                { "id", "33333333-3333-3333-3333-333333333333" }, { "channelId", Channel.ToString() }, { "fromUserId", Alice.ToString() },
                { "displayName", "alice" }, { "text", "hi" }, { "sentAt", "2024-05-01T10:00:00.000Z" }, { "clientRef", "ref-1" },
            };
            bridge.Resolve(call.rid, msg);
            client.Update();
            var sent = await send;
            Assert.Equal("hi", sent.Text);
            Assert.Equal(Channel, sent.ChannelId);
            Assert.Equal(new DateTimeOffset(2024, 5, 1, 10, 0, 0, TimeSpan.Zero), sent.SentAt);
            Assert.True(sent.IsOwn);

            var history = client.DirectHistoryAsync(Bob, before: "cursor", limit: 10);
            var h = bridge.Last("history");
            Assert.Equal(Bob.ToString(), MiniJson.GetString(h.args, "userId"));
            Assert.Equal("cursor", MiniJson.GetString(h.args, "before"));
            Assert.Equal(10, (int)MiniJson.GetNumber(h.args, "limit"));
            bridge.Resolve(h.rid, new Dictionary<string, object>
            {
                { "messages", new List<object> { msg } }, { "nextBefore", "older" }, { "nextAfter", null },
            });
            client.Update();
            var page = await history;
            Assert.Single(page.Messages);
            Assert.Equal("older", page.NextBefore);
            Assert.Null(page.NextAfter);
            Assert.Equal(Bob, page.PeerUserId);

            ChatMessage received = null;
            ChatReadMarker marker = null;
            var typing = new List<(Guid, Guid, bool)>();
            client.OnChatMessage += m => received = m;
            client.OnChatReadMarker += m => marker = m;
            client.OnParticipantTyping += (c, u, t) => typing.Add((c, u, t));
            var dm = new Dictionary<string, object>
            {
                { "id", "44444444-4444-4444-4444-444444444444" }, { "channelId", null }, { "fromUserId", Bob.ToString() }, { "toUserId", Alice.ToString() },
                { "displayName", "bob" }, { "text", "yo" }, { "sentAt", "2024-05-01T10:00:01Z" }, { "offline", true },
            };
            bridge.Emit("chatMessage", ("message", dm));
            bridge.Emit("chatReadMarker", ("marker", new Dictionary<string, object>
            {
                { "userId", Bob.ToString() }, { "channelId", Channel.ToString() }, { "messageId", "33333333-3333-3333-3333-333333333333" },
                { "messageSentAt", "2024-05-01T10:00:00Z" }, { "readAt", "2024-05-01T10:00:02Z" },
            }));
            bridge.Emit("participantTyping", ("channelId", Channel.ToString()), ("userId", Bob.ToString()), ("typing", true));
            client.Update();
            Assert.True(received.IsDirect);
            Assert.True(received.Offline);
            Assert.Equal(Alice, received.ToUserId);
            Assert.Equal(Channel, marker.ChannelId);
            Assert.Equal(new[] { (Channel, Bob, true) }, typing);

            await client.MarkDirectReadAsync(Bob, received.Id);
            var mark = bridge.Last("markRead").args;
            Assert.Equal(Bob.ToString(), MiniJson.GetString(mark, "userId"));
            Assert.Equal(received.Id.ToString(), MiniJson.GetString(mark, "messageId"));
        }

        [Fact]
        public async Task SpeakCompletesWhenTheTerminalTtsStatusArrives()
        {
            var (client, bridge) = NewClient();
            await Connect(client, bridge);
            var statuses = new List<TtsStatus>();
            client.OnTtsStatus += statuses.Add;

            var speak = client.SpeakAsync("hello", Channel, TtsDestination.Local, voice: "en-1", clientRef: "tts-1");
            var call = bridge.Last("speak");
            Assert.Equal("local", MiniJson.GetString(call.args, "destination"));
            Assert.Equal("en-1", MiniJson.GetString(call.args, "voice"));
            var requestId = Guid.NewGuid();
            bridge.Resolve(call.rid, new Dictionary<string, object> { { "requestId", requestId.ToString() }, { "clientRef", "tts-1" } });
            client.Update();
            var request = await speak;
            Assert.Equal(requestId, request.RequestId);
            Assert.False(request.Done.IsCompleted);

            bridge.Emit("ttsStatus", ("status", new Dictionary<string, object> { { "requestId", requestId.ToString() }, { "state", "playing" }, { "clientRef", "tts-1" } }));
            bridge.Emit("ttsStatus", ("status", new Dictionary<string, object> { { "requestId", requestId.ToString() }, { "state", "finished" }, { "durationMs", 1200.0 } }));
            client.Update();
            var done = await request.Done;
            Assert.Equal(TtsState.Finished, done.State);
            Assert.Equal(1200ul, done.DurationMs);
            Assert.Equal(2, statuses.Count);
        }

        [Fact]
        public async Task QualityStatsAndDeviceEventsAreParsed()
        {
            var (client, bridge) = NewClient();
            await Connect(client, bridge);
            WebGLStats? stats = null;
            WebGLAudioDevices devices = null;
            var remote = new List<(bool, string)>();
            client.OnStats += s => stats = s;
            client.OnDevicesChanged += d => devices = d;
            client.OnRemoteAudio += (p, r) => remote.Add((p, r));

            bridge.Emit("networkQuality", ("quality", new Dictionary<string, object> { { "bars", 4.0 }, { "mos", 4.2 }, { "rttMs", 31.0 }, { "downlinkLossPercent", 0.5 } }));
            bridge.Emit("stats", ("stats", new Dictionary<string, object> { { "rttMs", 30.0 }, { "packetsLost", 3.0 }, { "bars", 5.0 } }));
            bridge.Emit("devicesChanged", ("devices", new Dictionary<string, object>
            {
                { "inputs", new List<object> { new Dictionary<string, object> { { "deviceId", "m1" }, { "label", "Mic" }, { "groupId", "g" } } } },
                { "outputs", new List<object>() },
            }));
            bridge.Emit("remoteAudio", ("playing", false), ("reason", "NotAllowedError: user gesture required"));
            client.Update();
            Assert.Equal(4, client.LastNetworkQuality.Value.Bars);
            Assert.Equal(4.2f, client.LastNetworkQuality.Value.Mos);
            Assert.Equal(3, stats.Value.PacketsLost);
            Assert.Equal(5, stats.Value.Bars);
            Assert.Equal("Mic", Assert.Single(devices.Inputs).Label);
            Assert.Equal(new[] { (false, "NotAllowedError: user gesture required") }, remote);

            var resume = client.ResumeAudioAsync();
            Assert.Equal("resumeAudio", bridge.Last("resumeAudio").method);
            await resume;

            var get = client.GetStatsAsync();
            bridge.Resolve(bridge.Last("getStats").rid, new Dictionary<string, object> { { "rttMs", 12.0 }, { "mos", 4.4 } });
            client.Update();
            Assert.Equal(12f, (await get).RttMs);
        }

        [Fact]
        public async Task ReconnectLifecycleEventsAreForwarded()
        {
            var (client, bridge) = NewClient();
            await Connect(client, bridge);
            var recovering = new List<(int, TimeSpan, string)>();
            SessionInfo recovered = null;
            string endpoint = null, disconnected = null, closed = null;
            Exception failed = null;
            client.OnRecovering += (a, d, c) => recovering.Add((a, d, c));
            client.OnRecovered += s => recovered = s;
            client.OnEndpointChanged += u => endpoint = u;
            client.OnDisconnected += r => disconnected = r;
            client.OnFailedToRecover += e => failed = e;
            client.OnSessionClosed += r => closed = r;

            bridge.Emit("connectionState", ("state", "reconnecting"));
            bridge.Emit("recovering", ("attempt", 1.0), ("delayMs", 500.0), ("cause", "socket closed"));
            bridge.Emit("endpointChanged", ("url", "ws://node-b/ws"));
            bridge.Emit("recovered", ("info", SessionJson(endpoint: "ws://node-b/ws")));
            bridge.Emit("connectionState", ("state", "connected"));
            client.Update();
            Assert.Equal(new[] { (1, TimeSpan.FromMilliseconds(500), "socket closed") }, recovering);
            Assert.NotNull(recovered);
            Assert.Equal("ws://node-b/ws", endpoint);
            Assert.Equal("ws://node-b/ws", client.Endpoint);
            Assert.Null(disconnected);

            bridge.Emit("connectionState", ("state", "reconnecting"));
            bridge.Emit("failedToRecover", ("error", new Dictionary<string, object> { { "message", "gave up" }, { "name", "Error" } }));
            bridge.Emit("connectionState", ("state", "failed"));
            client.Update();
            Assert.Equal("gave up", failed.Message);
            Assert.Equal(VoiceConnectionState.Failed, client.State);
            Assert.Equal("reconnect failed", disconnected);

            await Connect(client, bridge);
            bridge.Emit("sessionClosed", ("reason", "kicked by admin"));
            bridge.Emit("connectionState", ("state", "disconnected"));
            client.Update();
            Assert.Equal("kicked by admin", closed);
            Assert.Equal("kicked by admin", disconnected);
        }

        [Fact]
        public async Task DisconnectReportsTheReasonAndDisposeReleasesTheHandleAndPendingCalls()
        {
            var (client, bridge) = NewClient();
            await Connect(client, bridge);
            string disconnected = null;
            client.OnDisconnected += r => disconnected = r;
            var stats = client.GetStatsAsync();

            bridge.Replies["disconnect"] = (args, _) =>
            {
                bridge.Emit("connectionState", ("state", "disconnected"));
                return "{\"ok\":true,\"value\":null}";
            };
            await client.DisconnectAsync("bye");
            Assert.Equal("bye", MiniJson.GetString(bridge.Last("disconnect").args, "reason"));
            Assert.Equal("bye", disconnected);
            Assert.Equal(VoiceConnectionState.Disconnected, client.State);

            client.Dispose();
            Assert.Equal(1, bridge.Destroyed);
            Assert.False(client.IsCreated);
            await Assert.ThrowsAsync<ObjectDisposedException>(() => stats);
            Assert.Throws<ObjectDisposedException>(() => client.ConnectAsync().GetAwaiter().GetResult());
            client.Dispose();
            Assert.Equal(1, bridge.Destroyed);
        }

        [Fact]
        public async Task PendingRequestsTimeOutAndCancel()
        {
            var (client, bridge) = NewClient();
            await Connect(client, bridge);
            client.RequestTimeout = TimeSpan.FromMilliseconds(1);
            var stats = client.GetStatsAsync();
            await Task.Delay(20);
            client.Update();
            await Assert.ThrowsAsync<TimeoutException>(() => stats);

            client.RequestTimeout = TimeSpan.FromMinutes(1);
            using var cts = new CancellationTokenSource();
            var devices = client.EnumerateDevicesAsync(cts.Token);
            cts.Cancel();
            await Assert.ThrowsAnyAsync<OperationCanceledException>(() => devices);
            bridge.Resolve(bridge.Last("enumerateDevices").rid, new Dictionary<string, object> { { "inputs", new List<object>() }, { "outputs", new List<object>() } });
            client.Update();
        }

        [Fact]
        public async Task OverflowRawMessagesAndUnknownEventsAreHandled()
        {
            var (client, bridge) = NewClient();
            await Connect(client, bridge);
            int dropped = 0;
            ControlMessage raw = null;
            client.OnEventsDropped += d => dropped = d;
            client.OnControlMessage += m => raw = m;
            bridge.Emit("overflow", ("dropped", 12.0));
            bridge.Emit("message", ("message", new Dictionary<string, object> { { "type", "Pong" }, { "data", new Dictionary<string, object> { { "ts", 1.0 } } } }));
            bridge.Emit("somethingNew", ("x", 1.0));
            bridge.Emit("result", ("rid", 999.0), ("ok", true), ("value", null));
            client.Update();
            Assert.Equal(12, dropped);
            Assert.Equal("Pong", raw.Type);
            Assert.Equal(1.0, MiniJson.GetNumber(raw.Data, "ts"));
        }

        [Fact]
        public void WebGLClientImplementsTheSharedInterfaceLikeTheNativeOne()
        {
            IAurixVoiceClient webgl = new AurixWebGLVoiceClient("http://api", "ws://ws/ws", "jwt", new FakeBridge());
            IAurixVoiceClient native = new AurixVoiceClient("ws://ws/ws", "jwt");
            foreach (var c in new[] { webgl, native })
            {
                Assert.Equal(VoiceConnectionState.Disconnected, c.State);
                Assert.Equal("ws://ws/ws", c.Endpoint);
                Assert.Empty(c.JoinedChannels);
                Assert.Equal(TransmissionMode.All, c.Transmission);
                c.Update();
                c.Dispose();
            }
        }

        [Fact]
        public void NativeBridgeIsUnavailableOutsideWebGLPlayers()
        {
            Assert.False(NativeWebGLBridge.IsSupported);
            Assert.Throws<PlatformNotSupportedException>(() => NativeWebGLBridge.Instance.Create("{}"));
            var client = new AurixWebGLVoiceClient("http://api", "ws://ws/ws", "jwt");
            Assert.Throws<PlatformNotSupportedException>(() => client.ConnectAsync().GetAwaiter().GetResult());
            client.Dispose();
        }
    }
}
