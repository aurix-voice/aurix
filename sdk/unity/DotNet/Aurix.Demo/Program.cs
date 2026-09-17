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
