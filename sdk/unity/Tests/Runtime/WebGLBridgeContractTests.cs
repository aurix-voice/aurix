using System;
using System.Collections.Generic;
using System.Threading.Tasks;
using Aurix.Protocol;
using Aurix.WebGL;
using NUnit.Framework;

namespace Aurix.Voice.Tests
{
    /// <summary>
    /// The C# half of the WebGL bridge against a scripted stand-in for <c>AurixWebGL.jslib</c>: proves the
    /// handle/JSON contract (create options, pending results, drained events) inside the Unity runtime, on any
    /// platform. The browser half is covered by <c>sdk/web/test/unity-jslib.test.mjs</c> and
    /// <c>BrowserTests~/webgl_bridge_e2e.py</c>.
    /// </summary>
    public class WebGLBridgeContractTests
    {
        private sealed class ScriptedBridge : IWebGLBridge
        {
            public readonly List<(string method, Dictionary<string, object> args, int rid)> Calls = new List<(string, Dictionary<string, object>, int)>();
            public readonly List<object> Events = new List<object>();
            public Dictionary<string, object> CreateOptions;
            public int Destroyed;

            public void LoadSdk(string url) { }
            public WebGLSdkStatus SdkStatus => WebGLSdkStatus.Ready;
            public string SdkError => null;

            public int Create(string optionsJson)
            {
                CreateOptions = MiniJson.AsObject(MiniJson.Parse(optionsJson));
                return 3;
            }

            public string Invoke(int handle, string method, string argsJson, int rid)
            {
                Assert.AreEqual(3, handle);
                Calls.Add((method, MiniJson.AsObject(MiniJson.Parse(argsJson)), rid));
                return rid > 0 ? "{\"ok\":true,\"pending\":true}" : "{\"ok\":true,\"value\":null}";
            }

            public string Drain(int handle)
            {
                var batch = MiniJson.Serialize(new List<object>(Events));
                Events.Clear();
                return batch;
            }

            public void Destroy(int handle) => Destroyed++;

            public void Emit(string json) => Events.Add(MiniJson.Parse(json));

            public int Rid(string method)
            {
                for (int i = Calls.Count - 1; i >= 0; i--)
                    if (Calls[i].method == method) return Calls[i].rid;
                Assert.Fail("no " + method + " call");
                return 0;
            }
        }

        private const string Session = "{\"sessionId\":\"22222222-2222-2222-2222-222222222222\",\"userId\":\"aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa\"," +
                                       "\"ssrc\":1234,\"endpoint\":\"ws://node-a/ws\",\"failover\":[\"ws://node-b/ws\"],\"resumed\":false,\"migrated\":false}";

        [Test]
        public void ConnectJoinAndEventsFollowTheBridgeContract()
        {
            var bridge = new ScriptedBridge();
            var client = new AurixWebGLVoiceClient("http://api", "ws://ws/ws", "jwt", bridge);
            client.Options.UseTurn = true;
            client.Options.ParticipantStreams = 4;

            var states = new List<VoiceConnectionState>();
            client.OnStateChanged += s => states.Add(s);
            Guid joined = Guid.Empty;
            client.OnChannelJoined += (ch, _) => joined = ch;
            IReadOnlyList<WebGLParticipantStream> layout = null;
            client.OnParticipantStreams += l => layout = l;

            var connect = client.ConnectAsync();
            Assert.AreEqual("http://api", MiniJson.GetString(bridge.CreateOptions, "apiUrl"));
            Assert.AreEqual("jwt", MiniJson.GetString(bridge.CreateOptions, "token"));
            Assert.AreEqual(4, (int)MiniJson.GetNumber(bridge.CreateOptions, "participantStreams"));
            Assert.IsFalse(connect.IsCompleted, "connect is asynchronous in the browser");

            int rid = bridge.Rid("connect");
            bridge.Emit("{\"type\":\"connectionState\",\"state\":\"connected\"}");
            bridge.Emit("{\"type\":\"sessionReady\",\"info\":" + Session + "}");
            bridge.Emit("{\"type\":\"result\",\"rid\":" + rid + ",\"ok\":true,\"value\":" + Session + "}");
            client.Update();

            Assert.IsTrue(connect.IsCompleted && !connect.IsFaulted, "connect resolved from the drained result");
            Assert.AreEqual(VoiceConnectionState.Connected, client.State);
            Assert.AreEqual("ws://node-a/ws", client.Endpoint);
            Assert.AreEqual(1, client.FailoverEndpoints.Count);
            CollectionAssert.Contains(states, VoiceConnectionState.Connected);

            var channel = Guid.Parse("11111111-1111-1111-1111-111111111111");
            var join = client.JoinChannelAsync(channel);
            int joinRid = bridge.Rid("joinChannel");
            Assert.AreEqual(channel.ToString(), MiniJson.GetString(bridge.Calls[bridge.Calls.Count - 1].args, "channelId"));
            bridge.Emit("{\"type\":\"channelJoined\",\"channelId\":\"" + channel + "\",\"participants\":[]}");
            bridge.Emit("{\"type\":\"participantStreams\",\"streams\":[{\"mid\":\"1\",\"userId\":null,\"live\":true}]}");
            bridge.Emit("{\"type\":\"result\",\"rid\":" + joinRid + ",\"ok\":true,\"value\":[]}");
            client.Update();

            Assert.IsTrue(join.IsCompleted && !join.IsFaulted);
            Assert.AreEqual(channel, joined);
            CollectionAssert.Contains(client.JoinedChannels, channel);
            Assert.IsNotNull(layout);
            Assert.AreEqual(1, layout.Count);
            Assert.AreEqual("1", layout[0].Mid);

            var failing = client.JoinChannelAsync(Guid.Parse("33333333-3333-3333-3333-333333333333"));
            bridge.Emit("{\"type\":\"result\",\"rid\":" + bridge.Rid("joinChannel") + ",\"ok\":false,\"error\":{\"message\":\"forbidden\",\"code\":\"FORBIDDEN\"}}");
            client.Update();
            Assert.IsTrue(failing.IsFaulted, "rejected results fault the pending task");

            client.Dispose();
            Assert.AreEqual(1, bridge.Destroyed);
        }

        [Test]
        public void ClientRefusesToWorkWithoutTheJslibOutsideWebGL()
        {
            if (NativeWebGLBridge.IsSupported) Assert.Ignore("running inside a WebGL player");
            var client = new AurixWebGLVoiceClient("http://api", "ws://ws/ws", "jwt");
            Assert.ThrowsAsync<PlatformNotSupportedException>(async () => await client.ConnectAsync());
        }
    }
}
