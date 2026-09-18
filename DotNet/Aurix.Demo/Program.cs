using System;
using System.Collections.Generic;
using System.Net.Http;
using System.Net.Http.Headers;
using System.Text;
using System.Threading;
using System.Threading.Tasks;
using Aurix;
using Aurix.Audio;
using Aurix.Protocol;
using Aurix.Samples;

namespace Aurix.Demo
{
    /// <summary>
    /// Headless end-to-end check of the C# SDK against a running Aurix server:
    /// two clients join one channel, "alice" streams a 440 Hz tone encoded with Opus, "bob"
    /// receives, verifies, jitter-buffers, decodes and measures the signal level.
    ///
    /// Usage (local dev only — the API key must never ship inside a game build):
    ///   dotnet run --project Aurix.Demo -- --api http://127.0.0.1:8080 --ws ws://127.0.0.1:8081/ws --api-key aurx_...
    /// Or with pre-issued tokens:
    ///   dotnet run --project Aurix.Demo -- --ws ws://... --channel &lt;uuid&gt; --token-a &lt;jwt&gt; --token-b &lt;jwt&gt;
    /// Add <c>--scenario reconnect</c> to exercise auto-reconnect instead: the control connection goes
    /// through a local TCP proxy that is cut abruptly, once within the server's resume grace (same
    /// session must come back) and once for longer than it (a fresh session must re-join the channel).
    /// </summary>
    public static class Program
    {
        public static async Task<int> Main(string[] args)
        {
            var opt = ParseArgs(args);
            string api = Get(opt, "api", "http://127.0.0.1:8080");
            string ws = Get(opt, "ws", "ws://127.0.0.1:8081/ws");
            string apiKey = Get(opt, "api-key", Environment.GetEnvironmentVariable("AURIX_API_KEY"));
            int seconds = int.Parse(Get(opt, "seconds", "12"));

            string channel = Get(opt, "channel", null), tokenA = Get(opt, "token-a", null), tokenB = Get(opt, "token-b", null);
            if (channel == null || tokenA == null || tokenB == null)
            {
                if (string.IsNullOrEmpty(apiKey)) { Console.Error.WriteLine("need --api-key (or AURIX_API_KEY) or --channel/--token-a/--token-b"); return 2; }
                using var http = new HttpClient { BaseAddress = new Uri(api) };
                http.DefaultRequestHeaders.Authorization = new AuthenticationHeaderValue("Bearer", apiKey);
                channel = MiniJson.GetString(MiniJson.AsObject(MiniJson.Parse(await PostJson(http, "/v1/channels", new Dictionary<string, object> { { "name", "csharp-demo" } }))), "id");
                tokenA = await IssueToken(http, channel, "alice");
                tokenB = await IssueToken(http, channel, "bob");
                Console.WriteLine($"channel {channel}");
            }
            var channelId = Guid.Parse(channel);
            if (Get(opt, "scenario", "audio") == "reconnect") return await ReconnectScenario(ws, channelId, tokenA, tokenB);

            var log = new List<string>();
            var alice = new AurixVoiceClient(ws, tokenA);
            var bob = new AurixVoiceClient(ws, tokenB);
            Hook(alice, "alice", log);
            Hook(bob, "bob", log);

            var a = await alice.ConnectAsync();
            var b = await bob.ConnectAsync();
            Console.WriteLine($"alice session {a.SessionId} ssrc {a.Ssrc} media {a.MediaAddr}");
            Console.WriteLine($"bob   session {b.SessionId} ssrc {b.Ssrc}");
            var rosterA = await alice.JoinChannelAsync(channelId);
            var rosterB = await bob.JoinChannelAsync(channelId);
            Console.WriteLine($"alice sees {rosterA.Count}, bob sees {rosterB.Count} participant(s) on join");

            // Pump both clients from one thread, like a game loop.
            using var cts = new CancellationTokenSource();
            var pump = Task.Run(async () =>
            {
                while (!cts.IsCancellationRequested) { alice.Update(); bob.Update(); await Task.Delay(10); }
            });

            // alice: 440 Hz tone, 20 ms Opus frames, real-time cadence.
            var hash = AurixVoiceClient.ChannelHash(channelId);
            using var enc = new ConcentusOpusCodec();
            var pcm = new float[AudioFormat.FrameSamples];
            var opus = new byte[1275];
            double phase = 0;
            var mixer = new RemoteMixer(() => new ConcentusOpusCodec());
            var outBuf = new float[AudioFormat.FrameSamples];
            double energy = 0; long samples = 0; int framesWithSignal = 0;

            int frames = seconds * 50;
            var sw = System.Diagnostics.Stopwatch.StartNew();
            for (int i = 0; i < frames; i++)
            {
                for (int n = 0; n < pcm.Length; n++) { pcm[n] = (float)(0.5 * Math.Sin(phase)); phase += 2 * Math.PI * 440 / AudioFormat.SampleRate; }
                int len = enc.Encode(pcm, AudioFormat.FrameSamples, opus);
                alice.SendOpusFrame(hash, opus, len);
                if (i == frames / 2) alice.SetMuted(true);       // second half: muted → bob should hear silence

                while (bob.TryDequeueAudio(out var incoming)) mixer.Push(incoming.SenderSsrc, incoming.Sequence, incoming.Volume, incoming.Opus);
                Array.Clear(outBuf, 0, outBuf.Length);
                mixer.Mix(outBuf, 1);
                double e = 0; foreach (var v in outBuf) e += v * v;
                if (i < frames / 2) { energy += e; samples += outBuf.Length; }
                if (e / outBuf.Length > 0.01) framesWithSignal++;

                var target = TimeSpan.FromMilliseconds((i + 1) * AudioFormat.FrameMs);
                var wait = target - sw.Elapsed;
                if (wait > TimeSpan.Zero) await Task.Delay(wait);
            }
            await Task.Delay(300);
            cts.Cancel();
            await pump;

            double rms = Math.Sqrt(energy / Math.Max(1, samples));
            Console.WriteLine($"alice sent {alice.Media.PacketsSent} pkts; bob received {bob.Media.PacketsReceived} verified, {bob.Media.PacketsBadAuth} bad auth, {bob.Media.PacketsReplayed} replayed");
            Console.WriteLine($"bob decoded RMS while alice talked: {rms:F3} (expect ≈0.35 for a 0.5-amplitude sine); frames with signal: {framesWithSignal}/{frames}");
            Console.WriteLine($"heartbeat acks {alice.Media.HeartbeatAcks} (every 5 s), RTT {alice.Media.LastRttMs} ms, alive={alice.Media.IsAlive}, control RTT {alice.ControlRttMs} ms");
            long received = bob.Media.PacketsReceived, badAuth = bob.Media.PacketsBadAuth;
            await alice.LeaveChannelAsync(channelId);
            await Task.Delay(200);
            alice.Update(); bob.Update();
            await alice.DisconnectAsync();
            await bob.DisconnectAsync();
            Console.WriteLine("events:");
            foreach (var l in log) Console.WriteLine("  " + l);

            bool ok = received > frames / 4 && rms > 0.2 && badAuth == 0
                      && log.Contains("bob: speaking alice true") && log.Contains("bob: mute alice muted=True");
            Console.WriteLine(ok ? "RESULT: PASS" : "RESULT: FAIL");
            return ok ? 0 : 1;
        }

        private static async Task<int> ReconnectScenario(string ws, Guid channelId, string tokenA, string tokenB)
        {
            var upstream = new Uri(ws);
            using var proxy = new CutProxy(upstream.Host, upstream.Port);
            var proxied = new UriBuilder(upstream) { Host = "127.0.0.1", Port = proxy.Port }.Uri.ToString();
            var log = new List<string>();
            var alice = new AurixVoiceClient(proxied, tokenA) { PingInterval = TimeSpan.FromSeconds(1) };
            var bob = new AurixVoiceClient(ws, tokenB);
            alice.Reconnect.InitialDelay = TimeSpan.FromMilliseconds(300);
            alice.Reconnect.MaxDelay = TimeSpan.FromSeconds(1);
            alice.Reconnect.Jitter = 0;
            alice.Reconnect.MaxAttempts = 15;
            Hook(alice, "alice", log);
            Hook(bob, "bob", log);
            alice.OnRecovering += (n, d, why) => Add(log, $"alice: recovering #{n} in {d.TotalMilliseconds:F0} ms ({why})");
            alice.OnRecovered += i => Add(log, $"alice: recovered resumed={i.Resumed} ssrc={i.Ssrc}");
            alice.OnFailedToRecover += e => Add(log, $"alice: failed to recover: {e.Message}");
            alice.OnChannelJoined += (_, r) => Add(log, $"alice: channel joined, {r.Count} other(s)");
            bob.OnChannelJoined += (_, r) => Add(log, $"bob: channel joined, {r.Count} other(s)");

            var a = await alice.ConnectAsync();
            await bob.ConnectAsync();
            await alice.JoinChannelAsync(channelId);
            await bob.JoinChannelAsync(channelId);
            Console.WriteLine($"alice session {a.SessionId} ssrc {a.Ssrc}; server resume grace {alice.ResumeGrace.TotalSeconds:F0} s");
            using var cts = new CancellationTokenSource();
            var pump = Task.Run(async () => { while (!cts.IsCancellationRequested) { alice.Update(); bob.Update(); await Task.Delay(10); } });
            var hash = AurixVoiceClient.ChannelHash(channelId);
            var opus = new byte[] { 0xf8, 0xff, 0xfe }; // Opus silence frame
            async Task Stream(int frames) { for (int i = 0; i < frames; i++) { alice.SendOpusFrame(hash, opus); await Task.Delay(20); } }
            async Task<bool> WaitState(AurixVoiceClient c, VoiceConnectionState st, TimeSpan timeout)
            {
                var deadline = DateTime.UtcNow + timeout;
                while (DateTime.UtcNow < deadline) { if (c.State == st) return true; await Task.Delay(20); }
                return false;
            }

            await Stream(25);
            long sentBefore = alice.Media.PacketsSent, bobBefore = bob.Media.PacketsReceived;

            // 1. Cut within grace → same session resumes, bob never sees a leave.
            Console.WriteLine("cutting control connection (within grace)");
            proxy.Cut();
            bool reconnecting = await WaitState(alice, VoiceConnectionState.Reconnecting, TimeSpan.FromSeconds(5));
            bool back = await WaitState(alice, VoiceConnectionState.MediaBound, TimeSpan.FromSeconds(10));
            bool sameSession = alice.Session.SessionId == a.SessionId && alice.Session.Ssrc == a.Ssrc && alice.Session.Resumed;
            await Task.Delay(200);
            await Stream(25);
            await Task.Delay(300);
            // alice.Media is a fresh transport after the rebind, so its counter restarts; bob's does not.
            bool audioAfterResume = bob.Media.PacketsReceived > bobBefore && alice.Media.PacketsSent > 0 && alice.Media.CurrentSequence > sentBefore;
            bool bobSawLeave; lock (log) bobSawLeave = log.Contains("bob: left alice");
            Console.WriteLine($"reconnecting={reconnecting} back={back} sameSession={sameSession} audioAfterResume={audioAfterResume} " +
                              $"(bob {bobBefore}→{bob.Media.PacketsReceived}, alice seq {sentBefore}→{alice.Media.CurrentSequence}) bobSawLeave={bobSawLeave}");

            // 2. Cut for longer than grace → fresh session, channel re-joined.
            bool freshOk = true, rejoined = true, bobSawRejoin = true;
            if (alice.ResumeGrace <= TimeSpan.FromSeconds(10))
            {
                Console.WriteLine($"cutting for {alice.ResumeGrace.TotalSeconds + 1:F0} s (beyond grace)");
                proxy.Block(true);
                proxy.Cut();
                await Task.Delay(alice.ResumeGrace + TimeSpan.FromSeconds(1));
                proxy.Block(false);
                back = await WaitState(alice, VoiceConnectionState.MediaBound, TimeSpan.FromSeconds(15));
                await Task.Delay(500);
                freshOk = back && alice.Session.SessionId != a.SessionId && !alice.Session.Resumed;
                rejoined = alice.JoinedChannels.Count == 1;
                lock (log) bobSawRejoin = log.Contains("bob: left alice") && log.FindLastIndex(l => l == "bob: joined alice") > log.LastIndexOf("bob: left alice");
                await Stream(25);
                Console.WriteLine($"fresh={freshOk} rejoined={rejoined} bobSawLeaveThenJoin={bobSawRejoin}");
            }
            else Console.WriteLine("resume grace > 10 s, skipping the expiry scenario (set AURIX__SERVER__SESSION_RESUME_GRACE_SECS=4)");

            // 3. Server unreachable until attempts run out → Failed + OnFailedToRecover, no zombie loops.
            proxy.Block(true);
            proxy.Cut();
            bool failed = await WaitState(alice, VoiceConnectionState.Failed, TimeSpan.FromSeconds(40));
            bool failedEvent; lock (log) failedEvent = log.Exists(l => l.StartsWith("alice: failed to recover"));
            Console.WriteLine($"gaveUp={failed} failedEvent={failedEvent}");

            cts.Cancel();
            await pump;
            await bob.DisconnectAsync();
            Console.WriteLine("events:");
            lock (log) foreach (var l in log) Console.WriteLine("  " + l);
            bool ok = reconnecting && sameSession && audioAfterResume && !bobSawLeave && freshOk && rejoined && bobSawRejoin && failed && failedEvent;
            Console.WriteLine(ok ? "RESULT: PASS" : "RESULT: FAIL");
            return ok ? 0 : 1;
        }

        /// <summary>Local TCP relay whose connections can be destroyed without a WebSocket close frame.</summary>
        private sealed class CutProxy : IDisposable
        {
            private readonly System.Net.Sockets.TcpListener _listener;
            private readonly List<System.Net.Sockets.TcpClient> _sockets = new List<System.Net.Sockets.TcpClient>();
            private volatile bool _blocked;
            public int Port { get; }

            public CutProxy(string host, int port)
            {
                _listener = new System.Net.Sockets.TcpListener(System.Net.IPAddress.Loopback, 0);
                _listener.Start();
                Port = ((System.Net.IPEndPoint)_listener.LocalEndpoint).Port;
                _ = Task.Run(async () =>
                {
                    while (true)
                    {
                        System.Net.Sockets.TcpClient client;
                        try { client = await _listener.AcceptTcpClientAsync(); } catch { return; }
                        if (_blocked) { client.Dispose(); continue; }
                        var up = new System.Net.Sockets.TcpClient();
                        try { await up.ConnectAsync(host, port); } catch { client.Dispose(); continue; }
                        lock (_sockets) { _sockets.Add(client); _sockets.Add(up); }
                        _ = Pipe(client, up); _ = Pipe(up, client);
                    }
                });
            }

            private static async Task Pipe(System.Net.Sockets.TcpClient from, System.Net.Sockets.TcpClient to)
            {
                var buf = new byte[16 * 1024];
                try
                {
                    var src = from.GetStream(); var dst = to.GetStream();
                    int n;
                    while ((n = await src.ReadAsync(buf, 0, buf.Length)) > 0) await dst.WriteAsync(buf, 0, n);
                }
                catch { }
                finally { from.Dispose(); to.Dispose(); }
            }

            /// <summary>Reject new connections (simulates the server being unreachable).</summary>
            public void Block(bool blocked) => _blocked = blocked;

            /// <summary>Destroy every relayed connection abruptly.</summary>
            public void Cut()
            {
                lock (_sockets) { foreach (var s in _sockets) s.Dispose(); _sockets.Clear(); }
            }

            public void Dispose() { Cut(); _listener.Stop(); }
        }

        private static void Hook(AurixVoiceClient c, string name, List<string> log)
        {
            c.OnStateChanged += s => Add(log, $"{name}: state {s}");
            c.OnParticipantJoined += (_, p) => Add(log, $"{name}: joined {p.DisplayName}");
            c.OnParticipantLeft += (_, p) => Add(log, $"{name}: left {p.DisplayName}");
            c.OnSpeaking += (_, p, sp) => Add(log, $"{name}: speaking {p.DisplayName} {(sp ? "true" : "false")}");
            c.OnParticipantUpdated += (_, p) => Add(log, $"{name}: mute {p.DisplayName} muted={p.IsMuted}");
            c.OnChannelLeft += ch => Add(log, $"{name}: left channel");
            c.OnServerError += (code, msg) => Add(log, $"{name}: server error {code} {msg}");
            c.OnDisconnected += r => Add(log, $"{name}: disconnected ({r})");
        }

        private static void Add(List<string> log, string s) { lock (log) log.Add(s); }

        private static async Task<string> IssueToken(HttpClient http, string channel, string name)
        {
            var body = new Dictionary<string, object>
            {
                { "external_id", "csharp-" + name }, { "display_name", name },
                { "channels", new List<object> { new Dictionary<string, object> { { "channel_id", channel }, { "join", true }, { "speak", true }, { "receive", true } } } },
            };
            return MiniJson.GetString(MiniJson.AsObject(MiniJson.Parse(await PostJson(http, "/v1/tokens", body))), "token");
        }

        private static async Task<string> PostJson(HttpClient http, string path, Dictionary<string, object> body)
        {
            var res = await http.PostAsync(path, new StringContent(MiniJson.Serialize(body), Encoding.UTF8, "application/json"));
            var text = await res.Content.ReadAsStringAsync();
            if (!res.IsSuccessStatusCode) throw new InvalidOperationException($"{path}: {(int)res.StatusCode} {text}");
            return text;
        }

        private static Dictionary<string, string> ParseArgs(string[] args)
        {
            var d = new Dictionary<string, string>();
            for (int i = 0; i < args.Length; i++)
                if (args[i].StartsWith("--") && i + 1 < args.Length) d[args[i].Substring(2)] = args[++i];
            return d;
        }

        private static string Get(Dictionary<string, string> d, string k, string def) => d.TryGetValue(k, out var v) ? v : def;
    }
}
