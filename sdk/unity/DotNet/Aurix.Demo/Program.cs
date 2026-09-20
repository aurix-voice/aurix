using System;
using System.Collections.Generic;
using System.Linq;
using System.Net.Http;
using System.Net.Http.Headers;
using System.Text;
using System.Threading;
using System.Threading.Tasks;
using Aurix;
using Aurix.Audio;
using Aurix.Protocol;
using Aurix.Transport;
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
            if (Get(opt, "scenario", "audio") == "failover") return await FailoverScenario(ws, Get(opt, "ws-b", ws), channelId, tokenA, tokenB);
            if (Get(opt, "scenario", "audio") == "prefs") return await PrefsScenario(ws, channelId, tokenA, tokenB);
            if (Get(opt, "scenario", "audio") == "chat") return await ChatScenario(ws, channelId, tokenA, tokenB);
            if (Get(opt, "scenario", "audio") == "pcmu") return await PcmuScenario(ws, channelId, tokenA, tokenB);
            if (Get(opt, "scenario", "audio") == "tunnel") return await TunnelScenario(ws, channelId, tokenA, tokenB, opt.ContainsKey("udp-block"));
            if (Get(opt, "scenario", "audio") == "transmission")
            {
                if (string.IsNullOrEmpty(apiKey)) { Console.Error.WriteLine("the transmission scenario needs --api-key (second channel + multi-channel tokens)"); return 2; }
                using var http = new HttpClient { BaseAddress = new Uri(api) };
                http.DefaultRequestHeaders.Authorization = new AuthenticationHeaderValue("Bearer", apiKey);
                var party = MiniJson.GetString(MiniJson.AsObject(MiniJson.Parse(await PostJson(http, "/v1/channels", new Dictionary<string, object> { { "name", "csharp-demo-party" } }))), "id");
                var multiA = await IssueToken(http, new[] { channel, party }, "alice");
                var multiB = await IssueToken(http, new[] { channel, party }, "bob");
                return await TransmissionScenario(ws, channelId, Guid.Parse(party), multiA, multiB);
            }
            if (Get(opt, "scenario", "audio") == "positional")
            {
                if (string.IsNullOrEmpty(apiKey)) { Console.Error.WriteLine("the positional scenario needs --api-key (creates a directional positional channel)"); return 2; }
                using var http = new HttpClient { BaseAddress = new Uri(api) };
                http.DefaultRequestHeaders.Authorization = new AuthenticationHeaderValue("Bearer", apiKey);
                var arena = MiniJson.GetString(MiniJson.AsObject(MiniJson.Parse(await PostJson(http, "/v1/channels", new Dictionary<string, object>
                {
                    { "name", "csharp-demo-arena" },
                    { "config", new Dictionary<string, object>
                        {
                            { "channel_type", "positional" },
                            { "positional_config", new Dictionary<string, object>
                                {
                                    { "near_distance", 2.0 }, { "far_distance", 22.0 }, { "rolloff", "linear" },
                                    { "max_radius", 30.0 }, { "directional", true }, { "coordinate_system", "left_handed" },
                                } },
                        } },
                }))), "id");
                var (arenaA, userA) = await IssueTokenWithUser(http, new[] { arena }, "alice");
                var (arenaB, userB) = await IssueTokenWithUser(http, new[] { arena }, "bob");
                return await PositionalScenario(ws, Guid.Parse(arena), arenaA, Guid.Parse(userA), arenaB, Guid.Parse(userB));
            }
            if (Get(opt, "scenario", "audio") == "echo")
            {
                if (string.IsNullOrEmpty(apiKey)) { Console.Error.WriteLine("the echo scenario needs --api-key (creates an echo channel)"); return 2; }
                using var http = new HttpClient { BaseAddress = new Uri(api) };
                http.DefaultRequestHeaders.Authorization = new AuthenticationHeaderValue("Bearer", apiKey);
                var echo = MiniJson.GetString(MiniJson.AsObject(MiniJson.Parse(await PostJson(http, "/v1/channels", new Dictionary<string, object>
                {
                    { "name", "csharp-demo-echo" },
                    { "config", new Dictionary<string, object> { { "channel_type", "echo" } } },
                }))), "id");
                var echoA = await IssueToken(http, new[] { echo }, "alice");
                var echoB = await IssueToken(http, new[] { echo }, "bob");
                return await EchoScenario(ws, Guid.Parse(echo), echoA, echoB);
            }

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
            bob.Mixer = mixer;
            bob.QualityReportInterval = TimeSpan.FromSeconds(1);
            NetworkQuality? bobServerQuality = null; int serverQualityReports = 0;
            bob.OnNetworkQuality += q => { bobServerQuality = q; serverQualityReports++; };
            var vad = new VoiceActivityDetector();
            float bobSeesAliceEnergy = 0f; int energyReports = 0;
            bob.OnChannelEnergy += (ch, levels) =>
            {
                foreach (var l in levels)
                {
                    var p = bob.GetParticipants(ch).FirstOrDefault(x => x.UserId == l.UserId);
                    if (p != null && p.Ssrc == a.Ssrc) { bobSeesAliceEnergy = Math.Max(bobSeesAliceEnergy, l.Energy); energyReports++; }
                }
            };

            int frames = seconds * 50;
            int spkMuteFrom = frames / 8, spkMuteTo = spkMuteFrom + 25; // bob mutes his speaker for 0.5 s mid-talk
            int mutedFramesSilent = 0;
            var sw = System.Diagnostics.Stopwatch.StartNew();
            for (int i = 0; i < frames; i++)
            {
                for (int n = 0; n < pcm.Length; n++) { pcm[n] = (float)(0.5 * Math.Sin(phase)); phase += 2 * Math.PI * 440 / AudioFormat.SampleRate; }
                int len = enc.Encode(pcm, AudioFormat.FrameSamples, opus);
                vad.Process(pcm, AudioFormat.FrameSamples);
                alice.SendOpusFrame(hash, opus, len, AudioFormat.FrameSamples, vad.Level);
                if (i == frames / 2) alice.SetMuted(true);       // second half: muted → bob should hear silence

                bool spkMuted = i >= spkMuteFrom && i < spkMuteTo;
                mixer.OutputMuted = spkMuted;
                while (bob.TryDequeueAudio(out var incoming)) mixer.Push(incoming.SenderSsrc, incoming.Sequence, incoming.Volume, null, incoming.Codec, incoming.Payload);
                Array.Clear(outBuf, 0, outBuf.Length);
                mixer.Mix(outBuf, 1);
                double e = 0; foreach (var v in outBuf) e += v * v;
                if (spkMuted) { if (e == 0) mutedFramesSilent++; }
                else if (i < frames / 2) { energy += e; samples += outBuf.Length; }
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
            var stats = bob.GetStats();
            Console.WriteLine($"bob stats: {stats.Bars}/5 bars (R {stats.RFactor:F1}, MOS {stats.Mos:F2}), rtt {stats.RttMs}/{stats.RttMinMs}/{stats.RttAvgMs:F1}/{stats.RttMaxMs} ms (last/min/avg/max), " +
                              $"jitter {stats.JitterMs:F2} ms, loss {stats.LossPercent:F1}%, frames lost {stats.FramesLost} late {stats.FramesLate} underruns {stats.Underruns}, " +
                              $"{stats.BytesReceived} B in / {stats.BytesSent} B out, heartbeats lost {stats.HeartbeatsLost}");
            Console.WriteLine(bobServerQuality.HasValue
                ? $"server quality for bob: {bobServerQuality.Value.Bars}/5 (R {bobServerQuality.Value.RFactor:F1}, uplink loss {bobServerQuality.Value.UplinkLossPercent:F1}%, {bobServerQuality.Value.UplinkBitrateKbps} kbps) after {serverQualityReports} report(s)"
                : "server quality for bob: none received");
            Console.WriteLine($"alice local VAD: level {vad.Level} (-dBov), speaking={vad.Speaking}; bob got {energyReports} energy report(s) for alice, peak {bobSeesAliceEnergy:F3} (expect ≈0.35)");
            Console.WriteLine($"bob speaker-muted for {spkMuteTo - spkMuteFrom} frames while alice talked: {mutedFramesSilent} silent (expect all), packets kept flowing");
            long received = bob.Media.PacketsReceived, badAuth = bob.Media.PacketsBadAuth;
            await alice.LeaveChannelAsync(channelId);
            await Task.Delay(200);
            alice.Update(); bob.Update();
            await alice.DisconnectAsync();
            await bob.DisconnectAsync();
            Console.WriteLine("events:");
            foreach (var l in log) Console.WriteLine("  " + l);

            bool ok = received > frames / 4 && rms > 0.2 && badAuth == 0 && mutedFramesSilent == spkMuteTo - spkMuteFrom
                      && log.Contains("bob: speaking alice true") && log.Contains("bob: mute alice muted=True")
                      && vad.Speaking && bobSeesAliceEnergy > 0.2f;
            Console.WriteLine(ok ? "RESULT: PASS" : "RESULT: FAIL");
            return ok ? 0 : 1;
        }

        /// <summary>
        /// Receiver-side controls over real UDP: bob locally mutes alice (channel, then everywhere),
        /// lowers her volume, and finally cross-mutes her; alice never learns about any of it and a
        /// fresh bob session starts with the block already loaded.
        /// </summary>
        private static async Task<int> PrefsScenario(string ws, Guid channelId, string tokenA, string tokenB)
        {
            var log = new List<string>();
            var alice = new AurixVoiceClient(ws, tokenA);
            var bob = new AurixVoiceClient(ws, tokenB);
            Hook(alice, "alice", log);
            Hook(bob, "bob", log);
            var prefsEvents = new List<Protocol.ReceiverPreferences>();
            bob.OnReceiverPreferences += p => { lock (prefsEvents) prefsEvents.Add(p); };
            bob.OnUserBlockChanged += (u, b) => Add(log, $"bob: block {u} {b}");

            var a = await alice.ConnectAsync();
            await bob.ConnectAsync();
            await alice.JoinChannelAsync(channelId);
            var roster = await bob.JoinChannelAsync(channelId);
            var aliceId = roster.First(p => p.Ssrc == a.Ssrc).UserId;
            using var cts = new CancellationTokenSource();
            var pump = Task.Run(async () => { while (!cts.IsCancellationRequested) { alice.Update(); bob.Update(); await Task.Delay(10); } });
            var hash = AurixVoiceClient.ChannelHash(channelId);
            var opus = new byte[] { 0xf8, 0xff, 0xfe };

            // Streams 25 frames from alice and reports how many bob received and at which gain byte.
            async Task<(int count, float volume)> Probe()
            {
                await Task.Delay(250);
                while (bob.TryDequeueAudio(out _)) { }
                for (int i = 0; i < 25; i++) { alice.SendOpusFrame(hash, opus); await Task.Delay(20); }
                await Task.Delay(300);
                int n = 0; float vol = 1f;
                while (bob.TryDequeueAudio(out var inc)) { n++; vol = inc.Volume; }
                return (n, vol);
            }

            bool ok = true;
            void Check(bool cond, string what) { Console.WriteLine($"{(cond ? "ok  " : "FAIL")} {what}"); ok &= cond; }

            await Task.Delay(300);
            Check(prefsEvents.Count == 1 && prefsEvents[0].BlockedUsers.Count == 0, "fresh session announces empty receiver preferences");
            var baseline = await Probe();
            Check(baseline.count >= 20 && Math.Abs(baseline.volume - 1f) < 1e-3, $"baseline: bob hears alice ({baseline.count} pkts, gain {baseline.volume})");

            await bob.SetParticipantMutedAsync(aliceId, true, channelId);
            Check(bob.IsParticipantMuted(aliceId, channelId) && !bob.IsParticipantMuted(aliceId, Guid.NewGuid()), "channel-scoped mute tracked locally");
            var muted = await Probe();
            Check(muted.count == 0, $"channel mute: bob hears nothing ({muted.count} pkts)");
            await bob.SetParticipantMutedAsync(aliceId, false, channelId);
            await bob.SetParticipantMutedAsync(aliceId, true);
            var mutedAll = await Probe();
            Check(mutedAll.count == 0, $"all-channel mute: bob hears nothing ({mutedAll.count} pkts)");
            await bob.SetParticipantMutedAsync(aliceId, false);
            var unmuted = await Probe();
            Check(unmuted.count >= 20, $"unmute restores audio ({unmuted.count} pkts)");

            await bob.SetParticipantVolumeAsync(aliceId, 0.5f);
            var half = await Probe();
            Check(half.count >= 20 && Math.Abs(half.volume - 0.5f) < 0.01f, $"volume 0.5: gain byte decodes to {half.volume}");
            await bob.SetParticipantVolumeAsync(aliceId, 2f);
            var boost = await Probe();
            Check(boost.count >= 20 && boost.volume > 1.9f, $"volume 2.0: gain byte decodes to {boost.volume}");
            await bob.SetParticipantVolumeAsync(aliceId, 1f);
            bool threw = false;
            try { await bob.SetParticipantVolumeAsync(aliceId, 2.5f); } catch (ArgumentOutOfRangeException) { threw = true; }
            Check(threw, "volume above 2.0 rejected client-side");

            await bob.SetUserBlockedAsync(aliceId, true);
            await Task.Delay(300);
            Check(bob.IsUserBlocked(aliceId), "block acknowledged by the server");
            var blocked = await Probe();
            Check(blocked.count == 0, $"blocked: bob hears nothing ({blocked.count} pkts)");
            bool aliceHeard;
            lock (log) aliceHeard = log.Any(l => l.StartsWith("alice: mute") || l.StartsWith("alice: block"));
            Check(!aliceHeard, "alice was never told about bob's local mute / volume / block");

            // Fresh bob session: the block is persistent and arrives with ReceiverPreferences.
            await bob.DisconnectAsync();
            var bob2 = new AurixVoiceClient(ws, tokenB);
            var prefs2 = new TaskCompletionSource<Protocol.ReceiverPreferences>(TaskCreationOptions.RunContinuationsAsynchronously);
            bob2.OnReceiverPreferences += p => prefs2.TrySetResult(p);
            await bob2.ConnectAsync();
            var pump2 = Task.Run(async () => { while (!cts.IsCancellationRequested) { bob2.Update(); await Task.Delay(10); } });
            var loaded = await Task.WhenAny(prefs2.Task, Task.Delay(3000)) == prefs2.Task ? prefs2.Task.Result : null;
            Check(loaded != null && loaded.BlockedUsers.Contains(aliceId) && bob2.IsUserBlocked(aliceId), "persistent block loaded on a new session");
            await bob2.SetUserBlockedAsync(aliceId, false);
            await Task.Delay(300);
            Check(!bob2.IsUserBlocked(aliceId), "unblock acknowledged");

            cts.Cancel();
            await pump; await pump2;
            await alice.DisconnectAsync();
            await bob2.DisconnectAsync();
            Console.WriteLine(ok ? "RESULT: PASS" : "RESULT: FAIL");
            return ok ? 0 : 1;
        }

        /// <summary>
        /// Multi-channel: alice and bob are in `team` and `party`. Alice's <c>TransmitOpusFrame</c> reaches
        /// both, only `team` under <c>Single(team)</c> and nobody under <c>None</c> (the server drops frames the
        /// client still sends by hash). Bob's focus attenuates the other channel; leaving the focused / target
        /// channel resets both, and the state survives a fresh join order (deferred until the channel is joined).
        /// </summary>
        /// <summary>
        /// Directional positional channel: alice plays a tone while bob (and the server) move her around
        /// him; bob's stereo mix must follow — right ear, then straight ahead after bob turns, then
        /// behind-left and quieter with distance and a receiver-local volume.
        /// </summary>
        private static async Task<int> PositionalScenario(string ws, Guid arena, string tokenA, Guid userA, string tokenB, Guid userB)
        {
            var log = new List<string>();
            var alice = new AurixVoiceClient(ws, tokenA);
            var bob = new AurixVoiceClient(ws, tokenB);
            Hook(alice, "alice", log);
            Hook(bob, "bob", log);
            await alice.ConnectAsync();
            await bob.ConnectAsync();
            using var cts = new CancellationTokenSource();
            var pump = Task.Run(async () => { while (!cts.IsCancellationRequested) { alice.Update(); bob.Update(); await Task.Delay(10); } });
            await alice.JoinChannelAsync(arena);
            await bob.JoinChannelAsync(arena);
            var hash = AurixVoiceClient.ChannelHash(arena);

            bool ok = true;
            void Check(bool cond, string what) { Console.WriteLine($"{(cond ? "ok  " : "FAIL")} {what}"); ok &= cond; }
            static Position3D At(float x, float y, float z) => new Position3D { X = x, Y = y, Z = z };
            static Orientation3D Facing(float x, float y, float z) => new Orientation3D { ForwardX = x, ForwardY = y, ForwardZ = z, UpX = 0, UpY = 1, UpZ = 0 };

            using var enc = new ConcentusOpusCodec();
            var pcm = new float[AudioFormat.FrameSamples];
            var opus = new byte[1275];
            double phase = 0;
            var mixer = new RemoteMixer(() => new ConcentusOpusCodec());
            var stereo = new float[AudioFormat.FrameSamples * 2];

            // Streams 50 frames (1 s) of a 0.5-amplitude sine and measures bob's stereo mix over the
            // second half (after the jitter buffer settled). Returns (leftRms, rightRms, volume, azimuth).
            async Task<(double left, double right, float volume, float? azimuth, int frames)> Probe()
            {
                await Task.Delay(250);
                while (bob.TryDequeueAudio(out _)) { }
                double le = 0, re = 0; long n = 0; float vol = 1f; float? az = null; int got = 0;
                var sw = System.Diagnostics.Stopwatch.StartNew();
                for (int i = 0; i < 50; i++)
                {
                    for (int k = 0; k < pcm.Length; k++) { pcm[k] = (float)(0.5 * Math.Sin(phase)); phase += 2 * Math.PI * 440 / AudioFormat.SampleRate; }
                    int len = enc.Encode(pcm, AudioFormat.FrameSamples, opus);
                    alice.SendOpusFrame(hash, opus, len);
                    while (bob.TryDequeueAudio(out var inc))
                    {
                        got++; vol = inc.Volume; az = inc.Direction?.Azimuth;
                        mixer.Push(inc.SenderSsrc, inc.Sequence, inc.Volume, inc.Direction, inc.Codec, inc.Payload);
                    }
                    Array.Clear(stereo, 0, stereo.Length);
                    mixer.Mix(stereo, 2);
                    if (i >= 25)
                    {
                        for (int k = 0; k < stereo.Length; k += 2) { le += stereo[k] * stereo[k]; re += stereo[k + 1] * stereo[k + 1]; }
                        n += stereo.Length / 2;
                    }
                    var wait = TimeSpan.FromMilliseconds((i + 1) * AudioFormat.FrameMs) - sw.Elapsed;
                    if (wait > TimeSpan.Zero) await Task.Delay(wait);
                }
                return (Math.Sqrt(le / Math.Max(1, n)), Math.Sqrt(re / Math.Max(1, n)), vol, az, got);
            }

            // Nothing is placed yet → nothing is forwarded.
            var blind = await Probe();
            Check(blind.frames == 0, $"no poses yet: bob got {blind.frames} frames");

            // Bob at the origin facing +Z, alice 1 m to his right.
            await bob.UpdatePositionAsync(arena, userB, At(0, 0, 0), Facing(0, 0, 1));
            await alice.UpdatePositionAsync(arena, userA, At(1, 0, 0), Facing(0, 0, 1));
            var right = await Probe();
            Check(right.frames >= 40 && right.azimuth is float a1 && Math.Abs(a1 - MathF.PI / 2) < 0.05 && right.volume == 1f,
                $"alice to the right: azimuth {right.azimuth:F2} (expect +1.57), gain {right.volume}, {right.frames} frames");
            Check(right.left < 0.05 && right.right > 0.40,
                $"stereo mix: left RMS {right.left:F3} ≈ 0, right RMS {right.right:F3} ≈ 0.5 (0.35 × √2)");

            // Bob turns to face +X: alice is straight ahead, equal in both ears.
            await bob.UpdatePositionAsync(arena, userB, At(0, 0, 0), Facing(1, 0, 0));
            var ahead = await Probe();
            Check(ahead.azimuth is float a2 && Math.Abs(a2) < 0.05, $"bob turned: azimuth {ahead.azimuth:F2} (expect 0)");
            Check(Math.Abs(ahead.left - ahead.right) < 0.03 && ahead.left > 0.30,
                $"stereo mix: left {ahead.left:F3} ≈ right {ahead.right:F3} ≈ 0.35");

            // Bob faces +Z again, alice walks 12 m behind-left; bob also turns her down to 0.5×.
            await bob.UpdatePositionAsync(arena, userB, At(0, 0, 0), Facing(0, 0, 1));
            float d = 12f / MathF.Sqrt(2f);
            await alice.UpdatePositionAsync(arena, userA, At(-d, 0, -d), Facing(0, 0, 1));
            await bob.SetParticipantVolumeAsync(userA, 0.5f);
            var far = await Probe();
            Check(far.azimuth is float a3 && Math.Abs(a3 + 3 * MathF.PI / 4) < 0.05 && Math.Abs(far.volume - 0.25f) < 0.02f,
                $"behind-left, 12 m, local 0.5×: azimuth {far.azimuth:F2} (expect -2.36), gain {far.volume:F2} (expect 0.25)");
            Check(far.left > 3 * far.right && far.left < 0.20 && far.left > 0.08,
                $"stereo mix: left {far.left:F3} (≈0.35 × 0.25 × 1.38), right {far.right:F3} (≈0.35 × 0.25 × 0.32)");

            // Out of range: silence again.
            await alice.UpdatePositionAsync(arena, userA, At(0, 0, 40), Facing(0, 0, 1));
            var gone = await Probe();
            Check(gone.frames == 0, $"beyond max_radius: bob got {gone.frames} frames");

            cts.Cancel();
            await pump;
            await alice.DisconnectAsync();
            await bob.DisconnectAsync();
            Console.WriteLine("events:");
            foreach (var l in log) Console.WriteLine("  " + l);
            Console.WriteLine(ok ? "positional scenario: OK" : "positional scenario: FAILED");
            return ok ? 0 : 1;
        }

        /// <summary>
        /// Sound test over real UDP: alice has no microphone signal (silent frames) and injects a
        /// 440 Hz clip through <see cref="AudioInjector"/>; the echo channel loops her own audio
        /// back to her — and only to her — while bob, in the same channel, hears nothing.
        /// </summary>
        /// <summary>
        /// Alice negotiates the PCMU fallback and speaks μ-law; Bob stays on Opus. The server transcodes at the
        /// edge in both directions, so each hears the other's tone at the expected level, Alice's downlink
        /// arrives flagged PCMU and Bob's stays Opus. Then Alice switches back to Opus mid-stream.
        /// </summary>
        private static async Task<int> PcmuScenario(string ws, Guid channelId, string tokenA, string tokenB)
        {
            var log = new List<string>();
            var alice = new AurixVoiceClient(ws, tokenA);
            var bob = new AurixVoiceClient(ws, tokenB);
            Hook(alice, "alice", log);
            Hook(bob, "bob", log);
            var codecEvents = new List<AudioCodec>();
            alice.OnAudioCodecChanged += c => { lock (codecEvents) codecEvents.Add(c); };
            var a = await alice.ConnectAsync();
            var b = await bob.ConnectAsync();
            using var cts = new CancellationTokenSource();
            var pump = Task.Run(async () => { while (!cts.IsCancellationRequested) { alice.Update(); bob.Update(); await Task.Delay(10); } });
            await alice.JoinChannelAsync(channelId);
            await bob.JoinChannelAsync(channelId);
            var hash = AurixVoiceClient.ChannelHash(channelId);

            bool ok = true;
            void Check(bool cond, string what) { Console.WriteLine($"{(cond ? "ok  " : "FAIL")} {what}"); ok &= cond; }

            await alice.SetAudioCodecAsync(AudioCodec.Pcmu);
            for (int i = 0; i < 100 && alice.AudioCodec != AudioCodec.Pcmu; i++) await Task.Delay(20);
            Check(alice.AudioCodec == AudioCodec.Pcmu, $"alice negotiated PCMU (active codec {alice.AudioCodec})");
            Check(bob.AudioCodec == AudioCodec.Opus, "bob stays on Opus");

            var pcmuEnc = new PcmuCodec();
            using var opusEnc = new ConcentusOpusCodec();
            var aliceMixer = new RemoteMixer(() => new ConcentusOpusCodec());
            var bobMixer = new RemoteMixer(() => new ConcentusOpusCodec());
            var pcm = new float[AudioFormat.FrameSamples];
            var ulaw = new byte[G711.FrameSamples];
            var opus = new byte[1275];
            var outBuf = new float[AudioFormat.FrameSamples];
            var vad = new VoiceActivityDetector();
            double aliceEnergy = 0, bobEnergy = 0; long aliceSamples = 0, bobSamples = 0;
            int aliceFrames = 0, alicePcmu = 0, bobFrames = 0, bobPcmu = 0;
            var sw = System.Diagnostics.Stopwatch.StartNew();
            for (int i = 0; i < 60; i++)
            {
                // Alice: 440 Hz at 0.5 through G.711; Bob: 660 Hz at 0.3 through Opus.
                for (int k = 0; k < pcm.Length; k++) pcm[k] = (float)(0.5 * Math.Sin(2 * Math.PI * 440 * (i * pcm.Length + k) / 48000.0));
                vad.Process(pcm, AudioFormat.FrameSamples);
                int n = pcmuEnc.Encode(pcm, AudioFormat.FrameSamples, ulaw);
                alice.TransmitAudioFrame(AudioCodec.Pcmu, ulaw, n, AudioFormat.FrameSamples, vad.Level);
                for (int k = 0; k < pcm.Length; k++) pcm[k] = (float)(0.3 * Math.Sin(2 * Math.PI * 660 * (i * pcm.Length + k) / 48000.0));
                int len = opusEnc.Encode(pcm, AudioFormat.FrameSamples, opus);
                bob.SendOpusFrame(hash, opus, len);
                while (alice.TryDequeueAudio(out var inc))
                {
                    aliceFrames++;
                    if (inc.Codec == AudioCodec.Pcmu) alicePcmu++;
                    aliceMixer.Push(inc.SenderSsrc, inc.Sequence, inc.Volume, inc.Direction, inc.Codec, inc.Payload);
                }
                while (bob.TryDequeueAudio(out var inc))
                {
                    bobFrames++;
                    if (inc.Codec == AudioCodec.Pcmu) bobPcmu++;
                    bobMixer.Push(inc.SenderSsrc, inc.Sequence, inc.Volume, inc.Direction, inc.Codec, inc.Payload);
                }
                Array.Clear(outBuf, 0, outBuf.Length);
                aliceMixer.Mix(outBuf, 1);
                if (i >= 10) { foreach (var v in outBuf) aliceEnergy += v * v; aliceSamples += outBuf.Length; }
                Array.Clear(outBuf, 0, outBuf.Length);
                bobMixer.Mix(outBuf, 1);
                if (i >= 10) { foreach (var v in outBuf) bobEnergy += v * v; bobSamples += outBuf.Length; }
                var wait = TimeSpan.FromMilliseconds((i + 1) * AudioFormat.FrameMs) - sw.Elapsed;
                if (wait > TimeSpan.Zero) await Task.Delay(wait);
            }
            double aliceRms = Math.Sqrt(aliceEnergy / Math.Max(1, aliceSamples)), bobRms = Math.Sqrt(bobEnergy / Math.Max(1, bobSamples));
            Check(aliceFrames >= 45 && alicePcmu == aliceFrames, $"alice received {aliceFrames} frames, {alicePcmu} flagged PCMU (all of them expected)");
            Check(bobFrames >= 45 && bobPcmu == 0, $"bob received {bobFrames} frames, {bobPcmu} flagged PCMU (expect 0: Opus after transcoding)");
            Check(aliceRms > 0.15 && aliceRms < 0.27, $"alice hears bob's 0.3 tone through μ-law: RMS {aliceRms:F3} (expect ≈0.21)");
            Check(bobRms > 0.28 && bobRms < 0.42, $"bob hears alice's 0.5 tone after PCMU→Opus: RMS {bobRms:F3} (expect ≈0.35)");

            // Back to Opus: the downlink follows within a few frames.
            await alice.SetAudioCodecAsync(AudioCodec.Opus);
            for (int i = 0; i < 100 && alice.AudioCodec != AudioCodec.Opus; i++) await Task.Delay(20);
            Check(alice.AudioCodec == AudioCodec.Opus, "alice switched back to Opus");
            int opusAfter = 0, pcmuAfter = 0;
            sw.Restart();
            for (int i = 0; i < 20; i++)
            {
                int len = opusEnc.Encode(pcm, AudioFormat.FrameSamples, opus);
                bob.SendOpusFrame(hash, opus, len);
                while (alice.TryDequeueAudio(out var inc)) { if (inc.Codec == AudioCodec.Pcmu) pcmuAfter++; else opusAfter++; }
                var wait = TimeSpan.FromMilliseconds((i + 1) * AudioFormat.FrameMs) - sw.Elapsed;
                if (wait > TimeSpan.Zero) await Task.Delay(wait);
            }
            await Task.Delay(100);
            while (alice.TryDequeueAudio(out var inc)) { if (inc.Codec == AudioCodec.Pcmu) pcmuAfter++; else opusAfter++; }
            Check(opusAfter >= 12 && pcmuAfter <= 3, $"after the switch alice gets Opus again: {opusAfter} Opus / {pcmuAfter} PCMU frames");
            List<AudioCodec> events; lock (codecEvents) events = new List<AudioCodec>(codecEvents);
            Check(events.Count == 2 && events[0] == AudioCodec.Pcmu && events[1] == AudioCodec.Opus, $"OnAudioCodecChanged: {string.Join(",", events)}");
            Check(alice.Media.PacketsBadAuth == 0 && bob.Media.PacketsBadAuth == 0, "no auth failures");

            cts.Cancel();
            await pump;
            await alice.DisconnectAsync();
            await bob.DisconnectAsync();
            aliceMixer.Dispose();
            bobMixer.Dispose();
            Console.WriteLine("events:");
            foreach (var l in log) Console.WriteLine("  " + l);
            Console.WriteLine(ok ? "RESULT: PASS" : "RESULT: FAIL");
            return ok ? 0 : 1;
        }

        /// <summary>
        /// Media over the control WebSocket instead of UDP: alice is forced onto the tunnel while bob
        /// stays on UDP, audio flows both ways, a resume keeps the tunnel, and with <c>--udp-block</c>
        /// (needs passwordless <c>sudo iptables</c>) a third client in Auto mode meets a UDP black hole:
        /// falls back at bind, returns to UDP when the re-probe answers, falls back again when the
        /// heartbeats die mid-session.
        /// </summary>
        private static async Task<int> TunnelScenario(string ws, Guid channelId, string tokenA, string tokenB, bool udpBlock)
        {
            var log = new List<string>();
            var alice = new AurixVoiceClient(ws, tokenA) { MediaPathPolicy = MediaPathPolicy.TunnelOnly, MediaHeartbeatInterval = TimeSpan.FromSeconds(1) };
            var bob = new AurixVoiceClient(ws, tokenB);
            Hook(alice, "alice", log);
            Hook(bob, "bob", log);
            var paths = new List<(MediaPath, string)>();
            alice.OnMediaPathChanged += (p, why) => { lock (paths) paths.Add((p, why)); Add(log, $"alice: media path {p} ({why})"); };
            bob.OnMediaPathChanged += (p, why) => Add(log, $"bob: media path {p} ({why})");
            alice.OnRecovered += i => Add(log, $"alice: recovered resumed={i.Resumed}");

            bool ok = true;
            void Check(bool cond, string what) { Console.WriteLine($"{(cond ? "ok  " : "FAIL")} {what}"); ok &= cond; }

            var a = await alice.ConnectAsync();
            var b = await bob.ConnectAsync();
            using var cts = new CancellationTokenSource();
            var clients = new List<AurixVoiceClient> { alice, bob };
            var pump = Task.Run(async () =>
            {
                while (!cts.IsCancellationRequested)
                {
                    AurixVoiceClient[] snapshot; lock (clients) snapshot = clients.ToArray();
                    foreach (var c in snapshot) c.Update();
                    await Task.Delay(10);
                }
            });
            await alice.JoinChannelAsync(channelId);
            await bob.JoinChannelAsync(channelId);
            var hash = AurixVoiceClient.ChannelHash(channelId);

            Check(a.MediaTunnel, "node advertises media_tunnel in SessionInitAck");
            Check(alice.ActiveMediaPath == MediaPath.Tunnel, $"alice (TunnelOnly) is on {alice.ActiveMediaPath}");
            Check(bob.ActiveMediaPath == MediaPath.Udp, $"bob (Auto) is on {bob.ActiveMediaPath}");
            (MediaPath, string) firstPath; lock (paths) firstPath = paths.Count > 0 ? paths[0] : default;
            Check(firstPath.Item1 == MediaPath.Tunnel && firstPath.Item2 == "tunnel-only policy", $"OnMediaPathChanged: {firstPath}");

            var (aliceRms, bobRms) = await Exchange(alice, bob, hash, 60);
            Check(aliceRms > 0.15 && aliceRms < 0.27, $"alice hears bob over the tunnel: RMS {aliceRms:F3} (expect ≈0.21)");
            Check(bobRms > 0.15 && bobRms < 0.27, $"bob hears alice's tunnelled uplink over UDP: RMS {bobRms:F3} (expect ≈0.21)");
            var stats = alice.GetStats();
            Check(stats.MediaPath == MediaPath.Tunnel && stats.UplinkDropped == 0 && stats.BadAuth == 0 && stats.Replayed == 0,
                $"stats: path={stats.MediaPath} uplinkDropped={stats.UplinkDropped} badAuth={stats.BadAuth} replayed={stats.Replayed}");
            Check(stats.RttMs > 0 && alice.Media.HeartbeatAcks > 0, $"tunnel heartbeats answered: {alice.Media.HeartbeatAcks} acks, RTT {stats.RttMs:F1} ms");

            // Resume while tunnelled: the new control socket carries the media again, sequence numbers continue.
            uint seqBefore = alice.Media.CurrentSequence;
            alice.ForceReconnect("tunnel resume");
            bool back = await WaitState(alice, VoiceConnectionState.MediaBound, TimeSpan.FromSeconds(10));
            Check(back && alice.Session.Resumed && alice.Session.SessionId == a.SessionId, $"resumed same session over a new socket (back={back} resumed={alice.Session?.Resumed})");
            Check(alice.ActiveMediaPath == MediaPath.Tunnel, $"still tunnelled after the resume ({alice.ActiveMediaPath})");
            await Task.Delay(200);
            var (aliceRms2, bobRms2) = await Exchange(alice, bob, hash, 40);
            Check(aliceRms2 > 0.15 && bobRms2 > 0.15, $"audio after the resume: alice {aliceRms2:F3} bob {bobRms2:F3}");
            Check(alice.Media.CurrentSequence > seqBefore, $"uplink sequence continued {seqBefore} → {alice.Media.CurrentSequence}");
            Check(alice.Media.PacketsBadAuth == 0 && bob.Media.PacketsBadAuth == 0 && bob.Media.PacketsReplayed == 0, "no auth/replay failures on either side");

            if (udpBlock)
            {
                int port = int.Parse(a.MediaAddr.Substring(a.MediaAddr.LastIndexOf(':') + 1));
                // Bob shares this host, so the rule spares his UDP socket: only carol falls into the hole.
                int bobPort = bob.Media.LocalEndPoint.Port;
                using var block = new UdpBlock(port, bobPort);
                Console.WriteLine($"blocking inbound UDP from the node's media port {port} except towards bob's port {bobPort} (sudo iptables)");
                block.Set(true);
                var carol = new AurixVoiceClient(ws, tokenA)
                {
                    MediaHeartbeatInterval = TimeSpan.FromMilliseconds(500),
                    UdpFallbackLostHeartbeats = 3,
                    UdpReprobeInterval = TimeSpan.FromSeconds(3),
                };
                var carolPaths = new List<(MediaPath, string)>();
                carol.OnMediaPathChanged += (p, why) => { lock (carolPaths) carolPaths.Add((p, why)); Add(log, $"carol: media path {p} ({why})"); };
                async Task<(MediaPath, string)?> WaitPath(int from, MediaPath want, TimeSpan timeout)
                {
                    var deadline = DateTime.UtcNow + timeout;
                    while (DateTime.UtcNow < deadline)
                    {
                        lock (carolPaths) for (int i = from; i < carolPaths.Count; i++) if (carolPaths[i].Item1 == want) return carolPaths[i];
                        await Task.Delay(20);
                    }
                    return null;
                }
                await alice.DisconnectAsync(); // same user as carol: one session per token keeps the roster simple
                lock (clients) { clients.Remove(alice); clients.Add(carol); }
                var sw = System.Diagnostics.Stopwatch.StartNew();
                await carol.ConnectAsync();
                await carol.JoinChannelAsync(channelId);
                var atBind = await WaitPath(0, MediaPath.Tunnel, TimeSpan.FromSeconds(5));
                Check(atBind != null && atBind.Value.Item2.StartsWith("UDP bind failed"), $"Auto fell back at bind in {sw.ElapsedMilliseconds} ms: {atBind}");
                var (carolRms, bobRms3) = await Exchange(carol, bob, hash, 50);
                Check(carolRms > 0.15 && bobRms3 > 0.15, $"audio through the black hole: carol {carolRms:F3} bob {bobRms3:F3}");

                block.Set(false);
                Console.WriteLine("UDP open again: waiting for the re-probe");
                sw.Restart();
                int seen; lock (carolPaths) seen = carolPaths.Count;
                var backToUdp = await WaitPath(seen, MediaPath.Udp, TimeSpan.FromSeconds(10));
                Check(backToUdp != null && backToUdp.Value.Item2 == "UDP re-probe answered", $"moved back to UDP in {sw.ElapsedMilliseconds} ms: {backToUdp}");
                var (carolRms2, bobRms4) = await Exchange(carol, bob, hash, 50);
                Check(carolRms2 > 0.15 && bobRms4 > 0.15, $"audio back on UDP: carol {carolRms2:F3} bob {bobRms4:F3}");

                block.Set(true);
                Console.WriteLine("UDP black-holed mid-session: waiting for the heartbeat fallback");
                sw.Restart();
                lock (carolPaths) seen = carolPaths.Count;
                var onLoss = await WaitPath(seen, MediaPath.Tunnel, TimeSpan.FromSeconds(10));
                Check(onLoss != null && onLoss.Value.Item2.Contains("heartbeats unanswered"), $"heartbeat fallback in {sw.ElapsedMilliseconds} ms: {onLoss}");
                var (carolRms3, bobRms5) = await Exchange(carol, bob, hash, 50);
                Check(carolRms3 > 0.15 && bobRms5 > 0.15, $"audio after the mid-session fallback: carol {carolRms3:F3} bob {bobRms5:F3}");
                var cs = carol.GetStats();
                Check(cs.BadAuth == 0 && cs.Replayed == 0 && cs.UplinkDropped == 0 && bob.Media.PacketsReplayed == 0,
                    $"carol stats after three path changes: badAuth={cs.BadAuth} replayed={cs.Replayed} uplinkDropped={cs.UplinkDropped} lostHb={cs.HeartbeatsLost}");
                block.Set(false);
                await carol.DisconnectAsync();
            }
            else
            {
                Console.WriteLine("(add --udp-block to also exercise the Auto fallback against an iptables UDP black hole)");
                await alice.DisconnectAsync();
            }

            cts.Cancel();
            await pump;
            await bob.DisconnectAsync();
            Console.WriteLine("events:");
            foreach (var l in log) Console.WriteLine("  " + l);
            Console.WriteLine(ok ? "RESULT: PASS" : "RESULT: FAIL");
            return ok ? 0 : 1;
        }

        /// <summary>Both clients send a 440/660 Hz tone at 0.3 for <paramref name="frames"/> × 20 ms; returns the RMS each one hears (≈0.21 expected).</summary>
        private static async Task<(double, double)> Exchange(AurixVoiceClient x, AurixVoiceClient y, uint hash, int frames)
        {
            using var encX = new ConcentusOpusCodec();
            using var encY = new ConcentusOpusCodec();
            var mixX = new RemoteMixer(() => new ConcentusOpusCodec());
            var mixY = new RemoteMixer(() => new ConcentusOpusCodec());
            var pcm = new float[AudioFormat.FrameSamples];
            var opus = new byte[1275];
            var outBuf = new float[AudioFormat.FrameSamples];
            double ex = 0, ey = 0; long nx = 0, ny = 0;
            var sw = System.Diagnostics.Stopwatch.StartNew();
            for (int i = 0; i < frames; i++)
            {
                for (int k = 0; k < pcm.Length; k++) pcm[k] = (float)(0.3 * Math.Sin(2 * Math.PI * 440 * (i * pcm.Length + k) / 48000.0));
                x.SendOpusFrame(hash, opus, encX.Encode(pcm, AudioFormat.FrameSamples, opus));
                for (int k = 0; k < pcm.Length; k++) pcm[k] = (float)(0.3 * Math.Sin(2 * Math.PI * 660 * (i * pcm.Length + k) / 48000.0));
                y.SendOpusFrame(hash, opus, encY.Encode(pcm, AudioFormat.FrameSamples, opus));
                while (x.TryDequeueAudio(out var inc)) mixX.Push(inc.SenderSsrc, inc.Sequence, inc.Volume, inc.Direction, inc.Codec, inc.Payload);
                while (y.TryDequeueAudio(out var inc)) mixY.Push(inc.SenderSsrc, inc.Sequence, inc.Volume, inc.Direction, inc.Codec, inc.Payload);
                Array.Clear(outBuf, 0, outBuf.Length);
                mixX.Mix(outBuf, 1);
                if (i >= 10) { foreach (var v in outBuf) ex += v * v; nx += outBuf.Length; }
                Array.Clear(outBuf, 0, outBuf.Length);
                mixY.Mix(outBuf, 1);
                if (i >= 10) { foreach (var v in outBuf) ey += v * v; ny += outBuf.Length; }
                var wait = TimeSpan.FromMilliseconds((i + 1) * AudioFormat.FrameMs) - sw.Elapsed;
                if (wait > TimeSpan.Zero) await Task.Delay(wait);
            }
            mixX.Dispose();
            mixY.Dispose();
            return (Math.Sqrt(ex / Math.Max(1, nx)), Math.Sqrt(ey / Math.Max(1, ny)));
        }

        private static async Task<bool> WaitState(AurixVoiceClient c, VoiceConnectionState st, TimeSpan timeout)
        {
            var deadline = DateTime.UtcNow + timeout;
            while (DateTime.UtcNow < deadline) { if (c.State == st) return true; await Task.Delay(20); }
            return false;
        }

        /// <summary>Drops inbound UDP from the node's media port (a black hole: our datagrams leave, nothing comes back), sparing one local port.</summary>
        private sealed class UdpBlock : IDisposable
        {
            private readonly int _port, _exceptDport;
            private bool _active;
            public UdpBlock(int port, int exceptDport) { _port = port; _exceptDport = exceptDport; }
            public void Set(bool on)
            {
                if (on == _active) return;
                var psi = new System.Diagnostics.ProcessStartInfo("sudo", $"-n iptables {(on ? "-A" : "-D")} INPUT -p udp --sport {_port} ! --dport {_exceptDport} -j DROP") { RedirectStandardError = true };
                using var p = System.Diagnostics.Process.Start(psi);
                string err = p.StandardError.ReadToEnd();
                p.WaitForExit();
                if (p.ExitCode != 0) throw new InvalidOperationException($"iptables failed ({p.ExitCode}): {err.Trim()}");
                _active = on;
            }
            public void Dispose() { try { Set(false); } catch (Exception e) { Console.Error.WriteLine(e.Message); } }
        }

        private static async Task<int> EchoScenario(string ws, Guid echo, string tokenA, string tokenB)
        {
            var log = new List<string>();
            var alice = new AurixVoiceClient(ws, tokenA);
            var bob = new AurixVoiceClient(ws, tokenB);
            Hook(alice, "alice", log);
            Hook(bob, "bob", log);
            var a = await alice.ConnectAsync();
            await bob.ConnectAsync();
            using var cts = new CancellationTokenSource();
            var pump = Task.Run(async () => { while (!cts.IsCancellationRequested) { alice.Update(); bob.Update(); await Task.Delay(10); } });
            await alice.JoinChannelAsync(echo);
            await bob.JoinChannelAsync(echo);
            var hash = AurixVoiceClient.ChannelHash(echo);

            bool ok = true;
            void Check(bool cond, string what) { Console.WriteLine($"{(cond ? "ok  " : "FAIL")} {what}"); ok &= cond; }

            // A 0.6 s clip: 0.5-amplitude sine at 440 Hz, stereo 24 kHz to exercise the converter.
            var clip = new float[24000 * 8 / 10 * 2];
            for (int i = 0; i < clip.Length / 2; i++) { float v = (float)(0.5 * Math.Sin(2 * Math.PI * 440 * i / 24000.0)); clip[2 * i] = v; clip[2 * i + 1] = v; }
            var injector = new AudioInjector();
            int ended = 0; injector.Ended += () => ended++;
            injector.Play(clip, 2, 24000);

            using var enc = new ConcentusOpusCodec();
            var pcm = new float[AudioFormat.FrameSamples];
            var opus = new byte[1275];
            var mixer = new RemoteMixer(() => new ConcentusOpusCodec());
            var outBuf = new float[AudioFormat.FrameSamples];
            var vad = new VoiceActivityDetector();
            double energy = 0; long samples = 0; int aliceFrames = 0, bobFrames = 0, wrongSsrc = 0, afterMute = 0;
            var sw = System.Diagnostics.Stopwatch.StartNew();
            for (int i = 0; i < 60; i++)
            {
                Array.Clear(pcm, 0, pcm.Length);              // "microphone": silence
                injector.Fill(pcm, AudioFormat.FrameSamples);  // + injected clip (ends after 40 frames)
                vad.Process(pcm, AudioFormat.FrameSamples);
                int len = enc.Encode(pcm, AudioFormat.FrameSamples, opus);
                alice.SendOpusFrame(hash, opus, len, AudioFormat.FrameSamples, vad.Level);
                if (i == 30) alice.SetMuted(true); // muting the uplink silences the sound test too
                while (alice.TryDequeueAudio(out var inc))
                {
                    aliceFrames++;
                    if (i >= 36) afterMute++;
                    if (inc.SenderSsrc != a.Ssrc) wrongSsrc++;
                    mixer.Push(inc.SenderSsrc, inc.Sequence, inc.Volume, inc.Direction, inc.Codec, inc.Payload);
                }
                while (bob.TryDequeueAudio(out _)) bobFrames++;
                Array.Clear(outBuf, 0, outBuf.Length);
                mixer.Mix(outBuf, 1);
                if (i >= 5 && i < 30) { foreach (var v in outBuf) energy += v * v; samples += outBuf.Length; }
                var wait = TimeSpan.FromMilliseconds((i + 1) * AudioFormat.FrameMs) - sw.Elapsed;
                if (wait > TimeSpan.Zero) await Task.Delay(wait);
            }
            await Task.Delay(200);
            while (alice.TryDequeueAudio(out _)) { aliceFrames++; afterMute++; }
            while (bob.TryDequeueAudio(out _)) bobFrames++;

            double rms = Math.Sqrt(energy / Math.Max(1, samples));
            Check(aliceFrames >= 25 && aliceFrames <= 36 && wrongSsrc == 0, $"alice hears herself: {aliceFrames} frames (expect ≈30, until she muted), all with her own SSRC ({wrongSsrc} foreign)");
            Check(rms > 0.25, $"loopback RMS {rms:F3} (expect ≈0.35 for the injected 0.5-amplitude sine)");
            Check(ended == 1 && !injector.Active, $"clip ended once: {ended}, injector active={injector.Active}");
            Check(afterMute == 0, $"nothing loops back after SetMuted(true): {afterMute} frames");
            Check(bobFrames == 0, $"bob (same echo channel) got {bobFrames} frames (expect 0)");
            Check(alice.Media.PacketsBadAuth == 0 && bob.Media.PacketsBadAuth == 0, "no auth failures");

            cts.Cancel();
            await pump;
            await alice.DisconnectAsync();
            await bob.DisconnectAsync();
            mixer.Dispose();
            Console.WriteLine("events:");
            foreach (var l in log) Console.WriteLine("  " + l);
            Console.WriteLine(ok ? "RESULT: PASS" : "RESULT: FAIL");
            return ok ? 0 : 1;
        }

        private static async Task<int> TransmissionScenario(string ws, Guid team, Guid party, string tokenA, string tokenB)
        {
            var log = new List<string>();
            var alice = new AurixVoiceClient(ws, tokenA);
            var bob = new AurixVoiceClient(ws, tokenB);
            Hook(alice, "alice", log);
            Hook(bob, "bob", log);
            var txEvents = new List<TransmissionMode>();
            var focusEvents = new List<Guid?>();
            alice.OnTransmissionChanged += m => { lock (txEvents) txEvents.Add(m); };
            bob.OnChannelFocusChanged += c => { lock (focusEvents) focusEvents.Add(c); };

            // Set before connecting: `Single(party)` must wait for the party join, `All`-default is untouched.
            await alice.TransmitToChannelAsync(party);
            await alice.ConnectAsync();
            await bob.ConnectAsync();
            using var cts = new CancellationTokenSource();
            var pump = Task.Run(async () => { while (!cts.IsCancellationRequested) { alice.Update(); bob.Update(); await Task.Delay(10); } });
            await alice.JoinChannelAsync(team);
            await bob.JoinChannelAsync(team);
            await bob.JoinChannelAsync(party);
            var teamHash = AurixVoiceClient.ChannelHash(team);
            var partyHash = AurixVoiceClient.ChannelHash(party);
            var opus = new byte[] { 0xf8, 0xff, 0xfe };

            bool ok = true;
            void Check(bool cond, string what) { Console.WriteLine($"{(cond ? "ok  " : "FAIL")} {what}"); ok &= cond; }

            // Streams 25 frames via TransmitOpusFrame and counts what bob got per channel (+ the party gain byte).
            async Task<(int team, int party, float partyVolume, int sentTo)> Probe()
            {
                await Task.Delay(250);
                while (bob.TryDequeueAudio(out _)) { }
                int sent = 0;
                for (int i = 0; i < 25; i++) { sent = alice.TransmitOpusFrame(opus); await Task.Delay(20); }
                await Task.Delay(300);
                int t = 0, p = 0; float vol = 1f;
                while (bob.TryDequeueAudio(out var inc))
                {
                    if (inc.ChannelHash == teamHash) t++;
                    else if (inc.ChannelHash == partyHash) { p++; vol = inc.Volume; }
                }
                return (t, p, vol, sent);
            }

            // Alice is in `team` only, mode `Single(party)` is pending (party not joined): nothing goes out.
            var pending = await Probe();
            Check(alice.Transmission == TransmissionMode.Single(party) && pending.sentTo == 0 && pending.team == 0,
                $"single(party) before joining party: no frames sent ({pending.sentTo} targets, team {pending.team})");
            lock (txEvents) Check(txEvents.Count == 0, "no server round-trip yet for a target that is not joined");

            await alice.JoinChannelAsync(party);
            await Task.Delay(300);
            lock (txEvents) Check(txEvents.Count == 1 && txEvents[0] == TransmissionMode.Single(party), "single(party) sent with the party join and acked");
            var single = await Probe();
            Check(single.sentTo == 1 && single.team == 0 && single.party >= 20, $"single(party): team {single.team}, party {single.party} pkts");

            await alice.SetTransmissionAsync(TransmissionMode.All);
            var all = await Probe();
            Check(all.sentTo == 2 && all.team >= 20 && all.party >= 20 && Math.Abs(all.partyVolume - 1f) < 1e-3,
                $"all: team {all.team}, party {all.party} pkts, party gain {all.partyVolume}");

            await alice.SetTransmissionAsync(TransmissionMode.None);
            var none = await Probe();
            Check(none.sentTo == 0 && none.team == 0 && none.party == 0, $"none: team {none.team}, party {none.party} pkts");
            // Frames addressed by hash while `None` are dropped locally too.
            alice.SendOpusFrame(teamHash, opus);
            await Task.Delay(300);
            Check(!bob.TryDequeueAudio(out _), "SendOpusFrame honours the mode locally");
            await alice.SetTransmissionAsync(TransmissionMode.All);

            // Receiver focus at bob: team at unity, party attenuated (server default gain 0.5).
            await bob.SetChannelFocusAsync(team);
            await Task.Delay(300);
            lock (focusEvents) Check(focusEvents.Count == 1 && focusEvents[0] == team, "focus(team) acked");
            var focused = await Probe();
            Check(focused.team >= 20 && focused.party >= 20 && focused.partyVolume < 0.99f,
                $"focus(team): party gain byte decodes to {focused.partyVolume}");
            Check(alice.FocusChannel == null, "focus is receiver-local: alice's own focus untouched");

            // Leaving resets: alice's `Single(team)` -> None, bob's focus(team) -> null, both via server events.
            await alice.TransmitToChannelAsync(team);
            await Task.Delay(300);
            await alice.LeaveChannelAsync(team);
            await bob.LeaveChannelAsync(team);
            await Task.Delay(400);
            Check(alice.Transmission == TransmissionMode.None, $"leaving the single target resets transmission to {alice.Transmission}");
            Check(bob.FocusChannel == null, "leaving the focused channel clears the focus");
            lock (focusEvents) Check(focusEvents.Count == 2 && focusEvents[1] == null, "focus reset delivered as ChannelFocusChanged{null}");
            var afterLeave = await Probe();
            Check(afterLeave.sentTo == 0 && afterLeave.party == 0, $"after the reset nothing is transmitted ({afterLeave.party} party pkts)");

            cts.Cancel();
            await pump;
            await alice.DisconnectAsync();
            await bob.DisconnectAsync();
            Console.WriteLine(ok ? "RESULT: PASS" : "RESULT: FAIL");
            return ok ? 0 : 1;
        }

        /// <summary>
        /// Text chat: alice and bob share a channel. Channel messages (with metadata) and their sender echo,
        /// typing indicators, a directed message, rejections carrying the client_ref (self-DM, empty text)
        /// and the server's anti-flood limit are exercised over the real control connection.
        /// </summary>
        private static async Task<int> ChatScenario(string ws, Guid channelId, string tokenA, string tokenB)
        {
            var log = new List<string>();
            var alice = new AurixVoiceClient(ws, tokenA);
            var bob = new AurixVoiceClient(ws, tokenB);
            Hook(alice, "alice", log);
            Hook(bob, "bob", log);
            var bobChat = new List<ChatMessage>();
            var aliceChat = new List<ChatMessage>();
            var bobTyping = new List<(Guid user, bool typing)>();
            var aliceTyping = new List<(Guid user, bool typing)>();
            bob.OnChatMessage += m => { lock (bobChat) bobChat.Add(m); };
            alice.OnChatMessage += m => { lock (aliceChat) aliceChat.Add(m); };
            bob.OnParticipantTyping += (_, u, t) => { lock (bobTyping) bobTyping.Add((u, t)); };
            alice.OnParticipantTyping += (_, u, t) => { lock (aliceTyping) aliceTyping.Add((u, t)); };

            var a = await alice.ConnectAsync();
            var b = await bob.ConnectAsync();
            await alice.JoinChannelAsync(channelId);
            var rosterB = await bob.JoinChannelAsync(channelId);
            var aliceId = rosterB.First(p => p.Ssrc == a.Ssrc).UserId;
            using var cts = new CancellationTokenSource();
            var pump = Task.Run(async () => { while (!cts.IsCancellationRequested) { alice.Update(); bob.Update(); await Task.Delay(10); } });
            await Task.Delay(300);
            var bobId = alice.GetParticipants(channelId).First(p => p.Ssrc == b.Ssrc).UserId;

            bool ok = true;
            void Check(bool cond, string what) { Console.WriteLine($"{(cond ? "ok  " : "FAIL")} {what}"); ok &= cond; }
            async Task<Exception> Fails(Func<Task> f) { try { await f(); return null; } catch (Exception e) { return e; } }

            var echo = await alice.SendMessageAsync(channelId, "gg", new Dictionary<string, object> { { "ping", new Dictionary<string, object> { { "x", 1.0 }, { "y", 2.0 } } } }, "ref-1");
            Check(echo.IsOwn && echo.ClientRef == "ref-1" && echo.ChannelId == channelId && echo.FromUserId == aliceId && echo.Text == "gg" && !echo.IsSystem,
                "sender echo completes the send with id/timestamp/client_ref");
            Check(echo.SentAt > DateTimeOffset.UtcNow.AddMinutes(-5), $"sent_at parsed ({echo.SentAt:O})");
            await Task.Delay(300);
            ChatMessage received; lock (bobChat) received = bobChat.FirstOrDefault(m => m.Id == echo.Id);
            Check(received != null && !received.IsOwn && received.ClientRef == null && received.DisplayName == "alice"
                  && MiniJson.GetNumber(MiniJson.AsObject(MiniJson.AsObject(received.Metadata)["ping"]), "y") == 2.0,
                "bob receives the message with metadata and without client_ref");
            lock (aliceChat) Check(aliceChat.Count == 1 && aliceChat[0].Id == echo.Id, "echo also raised through OnChatMessage");

            var e1 = await Fails(() => alice.SendMessageAsync(channelId, "   "));
            Check(e1 != null && e1.Message.StartsWith("VALIDATION_ERROR:"), $"empty text rejected: {e1?.Message}");
            var e2 = await Fails(() => alice.SendDirectMessageAsync(aliceId, "me"));
            Check(e2 != null && e2.Message.StartsWith("VALIDATION_ERROR:"), $"self DM rejected: {e2?.Message}");
            var e3 = await Fails(() => alice.SendDirectMessageAsync(Guid.NewGuid(), "hi"));
            Check(e3 != null && e3.Message.StartsWith("USER_OFFLINE:"), $"offline DM rejected: {e3?.Message}");

            var dm = await alice.SendDirectMessageAsync(bobId, "psst");
            await Task.Delay(300);
            ChatMessage gotDm; lock (bobChat) gotDm = bobChat.FirstOrDefault(m => m.Id == dm.Id);
            Check(dm.IsDirect && dm.ToUserId == bobId && dm.ChannelId == null && gotDm != null && gotDm.Text == "psst" && !gotDm.IsOwn,
                "directed message delivered to the online target");

            await alice.SetTypingAsync(channelId, true);
            await alice.SetTypingAsync(channelId, true);
            await alice.SetTypingAsync(channelId, true);
            await Task.Delay(300);
            await alice.SetTypingAsync(channelId, false);
            await Task.Delay(300);
            lock (bobTyping) Check(bobTyping.Count == 2 && bobTyping[0] == (aliceId, true) && bobTyping[1] == (aliceId, false),
                $"typing coalesced client-side and delivered to bob ({bobTyping.Count} events)");
            lock (aliceTyping) Check(aliceTyping.Count == 0, "alice never receives her own typing");

            var burst = new List<Task<ChatMessage>>();
            for (int i = 0; i < 14; i++) burst.Add(bob.SendMessageAsync(channelId, $"spam {i}"));
            try { await Task.WhenAll(burst); } catch { /* individual results inspected below */ }
            int limited = burst.Count(t => t.IsFaulted && t.Exception.InnerException.Message.StartsWith("RATE_LIMIT_EXCEEDED:"));
            int accepted = burst.Count(t => t.IsCompletedSuccessfully);
            Check(limited >= 1 && accepted >= 1 && limited + accepted == burst.Count, $"anti-flood: {accepted} accepted, {limited} rate-limited, none hanging");

            cts.Cancel();
            await pump;
            await alice.DisconnectAsync();
            await bob.DisconnectAsync();
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
            await alice.SetAudioCodecAsync(AudioCodec.Pcmu); // preference must survive resume and a fresh session
            Console.WriteLine($"alice session {a.SessionId} ssrc {a.Ssrc}; server resume grace {alice.ResumeGrace.TotalSeconds:F0} s");
            using var cts = new CancellationTokenSource();
            var pump = Task.Run(async () => { while (!cts.IsCancellationRequested) { alice.Update(); bob.Update(); await Task.Delay(10); } });
            var hash = AurixVoiceClient.ChannelHash(channelId);
            var ulaw = new byte[G711.FrameSamples]; // 20 ms of μ-law "zero" (0xff)
            Array.Fill(ulaw, (byte)0xff);
            async Task Stream(int frames) { for (int i = 0; i < frames; i++) { alice.SendAudioFrame(hash, AudioCodec.Pcmu, ulaw); await Task.Delay(20); } }
            await Stream(25);
            long sentBefore = alice.Media.PacketsSent, bobBefore = bob.Media.PacketsReceived;

            // 1. Cut within grace → same session resumes, bob never sees a leave.
            Console.WriteLine("cutting control connection (within grace)");
            proxy.Cut();
            bool reconnecting = await WaitState(alice, VoiceConnectionState.Reconnecting, TimeSpan.FromSeconds(5));
            bool back = await WaitState(alice, VoiceConnectionState.MediaBound, TimeSpan.FromSeconds(10));
            bool sameSession = alice.Session.SessionId == a.SessionId && alice.Session.Ssrc == a.Ssrc && alice.Session.Resumed;
            bool codecAfterResume = alice.AudioCodec == AudioCodec.Pcmu;
            await Task.Delay(200);
            await Stream(25);
            await Task.Delay(300);
            // alice.Media is a fresh transport after the rebind, so its counter restarts; bob's does not.
            bool audioAfterResume = bob.Media.PacketsReceived > bobBefore && alice.Media.PacketsSent > 0 && alice.Media.CurrentSequence > sentBefore;
            bool bobSawLeave; lock (log) bobSawLeave = log.Contains("bob: left alice");
            Console.WriteLine($"reconnecting={reconnecting} back={back} sameSession={sameSession} codec={alice.AudioCodec} audioAfterResume={audioAfterResume} " +
                              $"(bob {bobBefore}→{bob.Media.PacketsReceived}, alice seq {sentBefore}→{alice.Media.CurrentSequence}) bobSawLeave={bobSawLeave}");

            // 1b. Mobile: the app knows the network path changed (Wi-Fi → cellular) → ForceReconnect on a
            // healthy connection resumes the same session and rebinds media without waiting for a timeout.
            Console.WriteLine("ForceReconnect (network change) on a healthy connection");
            long bobBeforeForce = bob.Media.PacketsReceived;
            var forcedAt = DateTime.UtcNow;
            alice.ForceReconnect("wifi -> cellular");
            bool forcedReconnecting = await WaitState(alice, VoiceConnectionState.Reconnecting, TimeSpan.FromSeconds(2));
            bool forcedBack = await WaitState(alice, VoiceConnectionState.MediaBound, TimeSpan.FromSeconds(10));
            var forcedTook = DateTime.UtcNow - forcedAt;
            alice.PingInterval = TimeSpan.FromSeconds(30); // regular ping timeout would be 90 s
            bool forcedSame = alice.Session.SessionId == a.SessionId && alice.Session.Resumed;
            await Task.Delay(200);
            await Stream(25);
            await Task.Delay(300);
            bool audioAfterForce = bob.Media.PacketsReceived > bobBeforeForce;
            Console.WriteLine($"forced: reconnecting={forcedReconnecting} back={forcedBack} in {forcedTook.TotalMilliseconds:F0} ms sameSession={forcedSame} audio={audioAfterForce}");

            // 1c. Mobile: back from background over a half-open socket (bytes vanish, no RST) →
            // ProbeConnection detects it within its own timeout instead of 3 × PingInterval.
            Console.WriteLine("stalling the connection, then ProbeConnection(700 ms)");
            proxy.Stall(true);
            var probedAt = DateTime.UtcNow;
            alice.ProbeConnection(TimeSpan.FromMilliseconds(700));
            bool probeReconnecting = await WaitState(alice, VoiceConnectionState.Reconnecting, TimeSpan.FromSeconds(5));
            var probeTook = DateTime.UtcNow - probedAt;
            proxy.Stall(false);
            proxy.Cut(); // the stalled sockets are dead on the client side already; drop the proxy halves too
            bool probeBack = await WaitState(alice, VoiceConnectionState.MediaBound, TimeSpan.FromSeconds(10));
            bool probeSame = alice.Session.SessionId == a.SessionId && alice.Session.Resumed;
            alice.PingInterval = TimeSpan.FromSeconds(1);
            bool probeFast = probeReconnecting && probeTook < TimeSpan.FromSeconds(3);
            string probeReason; lock (log) probeReason = log.FindLast(l => l.StartsWith("alice: recovering #1 in") && l.Contains("probe timeout")) ?? "(no probe-timeout recovery logged)";
            Console.WriteLine($"probe: reconnecting={probeReconnecting} after {probeTook.TotalMilliseconds:F0} ms back={probeBack} sameSession={probeSame} — {probeReason}");

            // 2. Cut for longer than grace → fresh session, channel re-joined.
            bool freshOk = true, rejoined = true, bobSawRejoin = true, codecAfterFresh = true;
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
                for (int i = 0; i < 50 && alice.AudioCodec != AudioCodec.Pcmu; i++) await Task.Delay(20);
                codecAfterFresh = alice.AudioCodec == AudioCodec.Pcmu;
                lock (log) bobSawRejoin = log.Contains("bob: left alice") && log.FindLastIndex(l => l == "bob: joined alice") > log.LastIndexOf("bob: left alice");
                await Stream(25);
                Console.WriteLine($"fresh={freshOk} rejoined={rejoined} codec={alice.AudioCodec} bobSawLeaveThenJoin={bobSawRejoin}");
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
            bool ok = reconnecting && sameSession && codecAfterResume && audioAfterResume && !bobSawLeave
                      && forcedReconnecting && forcedBack && forcedSame && audioAfterForce
                      && probeFast && probeBack && probeSame && probeReason.Contains("probe timeout")
                      && freshOk && rejoined && codecAfterFresh && bobSawRejoin && failed && failedEvent;
            Console.WriteLine(ok ? "RESULT: PASS" : "RESULT: FAIL");
            return ok ? 0 : 1;
        }

        /// <summary>
        /// Cross-node failover. Alice reaches node 1 through a local proxy that is then blocked for good
        /// (the node "died" from her point of view); the node advertised at least one other node in
        /// <c>SessionInitAck.failover</c>, so the reconnect rotation lands on it and that node takes the
        /// session over: same session id and SSRC, <c>Migrated</c>, channel and codec preference kept,
        /// audio flows both ways again (Bob stayed on node 1, so the new path is cascaded). Needs two
        /// nodes with Redis and <c>server.external_ws_url</c> set on both.
        /// </summary>
        private static async Task<int> FailoverScenario(string ws, string wsB, Guid channelId, string tokenA, string tokenB)
        {
            var upstream = new Uri(ws);
            using var proxy = new CutProxy(upstream.Host, upstream.Port);
            var proxied = new UriBuilder(upstream) { Host = "127.0.0.1", Port = proxy.Port }.Uri.ToString();
            var log = new List<string>();
            var alice = new AurixVoiceClient(proxied, tokenA) { PingInterval = TimeSpan.FromSeconds(1) };
            var bob = new AurixVoiceClient(wsB, tokenB);
            alice.Reconnect.InitialDelay = TimeSpan.FromMilliseconds(300);
            alice.Reconnect.MaxDelay = TimeSpan.FromSeconds(1);
            alice.Reconnect.Jitter = 0;
            alice.Reconnect.MaxAttempts = 15;
            alice.RequestTimeout = TimeSpan.FromSeconds(3);
            Hook(alice, "alice", log);
            Hook(bob, "bob", log);
            alice.OnRecovering += (n, d, why) => Add(log, $"alice: recovering #{n} in {d.TotalMilliseconds:F0} ms ({why})");
            alice.OnRecovered += i => Add(log, $"alice: recovered resumed={i.Resumed} migrated={i.Migrated} ssrc={i.Ssrc} endpoint={i.Endpoint}");
            alice.OnEndpointChanged += url => Add(log, $"alice: endpoint -> {url}");
            alice.OnFailedToRecover += e => Add(log, $"alice: failed to recover: {e.Message}");

            var a = await alice.ConnectAsync();
            await bob.ConnectAsync();
            Console.WriteLine($"alice session {a.SessionId} ssrc {a.Ssrc} on {a.Endpoint}; failover: [{string.Join(", ", a.Failover)}]");
            if (a.Failover.Count == 0)
            {
                Console.WriteLine("the node advertised no failover endpoints (second node down or external_ws_url unset)");
                await alice.DisconnectAsync(); await bob.DisconnectAsync();
                return 2;
            }
            await alice.JoinChannelAsync(channelId);
            await bob.JoinChannelAsync(channelId);
            await alice.SetAudioCodecAsync(AudioCodec.Pcmu); // preference must survive the takeover
            await bob.SetAudioCodecAsync(AudioCodec.Pcmu);
            using var cts = new CancellationTokenSource();
            var pump = Task.Run(async () => { while (!cts.IsCancellationRequested) { alice.Update(); bob.Update(); await Task.Delay(10); } });
            var hash = AurixVoiceClient.ChannelHash(channelId);
            var ulaw = new byte[G711.FrameSamples];
            Array.Fill(ulaw, (byte)0xff);
            async Task Stream(int frames)
            {
                for (int i = 0; i < frames; i++)
                {
                    alice.SendAudioFrame(hash, AudioCodec.Pcmu, ulaw);
                    bob.SendAudioFrame(hash, AudioCodec.Pcmu, ulaw);
                    await Task.Delay(20);
                }
            }
            await Stream(25);
            await Task.Delay(300);
            long bobBefore = bob.Media.PacketsReceived;
            bool audioBefore = bobBefore > 0 && alice.Media.PacketsReceived > 0;

            Console.WriteLine("node 1 becomes unreachable for alice (proxy blocked + cut)");
            proxy.Block(true);
            proxy.Cut();
            var lostAt = DateTime.UtcNow;
            bool reconnecting = await WaitState(alice, VoiceConnectionState.Reconnecting, TimeSpan.FromSeconds(5));
            bool back = await WaitState(alice, VoiceConnectionState.MediaBound, TimeSpan.FromSeconds(30));
            var took = DateTime.UtcNow - lostAt;
            var s = alice.Session;
            bool migrated = back && s.SessionId == a.SessionId && s.Ssrc == a.Ssrc && s.Resumed && s.Migrated;
            bool moved = alice.Endpoint != proxied && a.Failover.Contains(alice.Endpoint);
            // Events are delivered through the main-thread pump, so the notification may trail the state flip.
            bool endpointEvent = false;
            for (int i = 0; i < 50 && !endpointEvent; i++)
            {
                lock (log) endpointEvent = log.Exists(l => l.StartsWith("alice: endpoint -> ") && l.EndsWith(alice.Endpoint));
                if (!endpointEvent) await Task.Delay(20);
            }
            bool stillJoined = alice.JoinedChannels.Count == 1;
            for (int i = 0; i < 50 && alice.AudioCodec != AudioCodec.Pcmu; i++) await Task.Delay(20);
            bool codecKept = alice.AudioCodec == AudioCodec.Pcmu;
            await Task.Delay(300);
            await Stream(25);
            await Task.Delay(500);
            // alice.Media is a fresh transport after the rebind (new node, new key), so her counter restarts.
            bool audioAfter = bob.Media.PacketsReceived > bobBefore && alice.Media.PacketsReceived > 0;
            bool bobSawLeave; lock (log) bobSawLeave = log.Contains("bob: left alice");
            Console.WriteLine($"reconnecting={reconnecting} back={back} in {took.TotalMilliseconds:F0} ms migrated={migrated} moved={moved} ({alice.Endpoint}) " +
                              $"endpointEvent={endpointEvent} stillJoined={stillJoined} codec={alice.AudioCodec} audioBefore={audioBefore} audioAfter={audioAfter} " +
                              $"(bob {bobBefore}→{bob.Media.PacketsReceived}, alice rx {alice.Media.PacketsReceived}) bobSawLeave={bobSawLeave}");

            cts.Cancel();
            await pump;
            await alice.DisconnectAsync();
            await bob.DisconnectAsync();
            Console.WriteLine("events:");
            lock (log) foreach (var l in log) Console.WriteLine("  " + l);
            bool ok = reconnecting && migrated && moved && endpointEvent && stillJoined && codecKept && audioBefore && audioAfter && !bobSawLeave;
            Console.WriteLine(ok ? "RESULT: PASS" : "RESULT: FAIL");
            return ok ? 0 : 1;
        }

        /// <summary>Local TCP relay whose connections can be destroyed without a WebSocket close frame.</summary>
        private sealed class CutProxy : IDisposable
        {
            private readonly System.Net.Sockets.TcpListener _listener;
            private readonly List<System.Net.Sockets.TcpClient> _sockets = new List<System.Net.Sockets.TcpClient>();
            private volatile bool _blocked;
            private volatile bool _stalled;
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

            private async Task Pipe(System.Net.Sockets.TcpClient from, System.Net.Sockets.TcpClient to)
            {
                var buf = new byte[16 * 1024];
                try
                {
                    var src = from.GetStream(); var dst = to.GetStream();
                    int n;
                    while ((n = await src.ReadAsync(buf, 0, buf.Length)) > 0)
                    {
                        if (_stalled) continue; // black hole: the connection looks alive but nothing gets through
                        await dst.WriteAsync(buf, 0, n);
                    }
                }
                catch { }
                finally { from.Dispose(); to.Dispose(); }
            }

            /// <summary>Reject new connections (simulates the server being unreachable).</summary>
            public void Block(bool blocked) => _blocked = blocked;

            /// <summary>Keep connections open but drop every byte (simulates a half-open socket after a network change).</summary>
            public void Stall(bool stalled) => _stalled = stalled;

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

        private static Task<string> IssueToken(HttpClient http, string channel, string name) => IssueToken(http, new[] { channel }, name);

        private static async Task<string> IssueToken(HttpClient http, string[] channels, string name) => (await IssueTokenWithUser(http, channels, name)).token;

        private static async Task<(string token, string userId)> IssueTokenWithUser(HttpClient http, string[] channels, string name)
        {
            var grants = new List<object>();
            foreach (var channel in channels)
                grants.Add(new Dictionary<string, object> { { "channel_id", channel }, { "join", true }, { "speak", true }, { "receive", true } });
            var body = new Dictionary<string, object>
            {
                { "external_id", "csharp-" + name }, { "display_name", name },
                { "channels", grants },
            };
            var res = MiniJson.AsObject(MiniJson.Parse(await PostJson(http, "/v1/tokens", body)));
            return (MiniJson.GetString(res, "token"), MiniJson.GetString(res, "user_id"));
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
