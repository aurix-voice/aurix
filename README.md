# Aurix Voice SDK for Unity / .NET

Native client for the Aurix Voice Platform. Speaks the **AURX** UDP media protocol directly
(no WebRTC stack needed), with the control plane over WebSocket. Works in Unity 2021.3+ (.NET Standard 2.1,
all platforms except WebGL — use the [Web SDK](../web) there) and in plain .NET (dedicated servers, bots, tests).

```
sdk/unity/
├── package.json                 UPM package (com.aurix.voice) — add via "Add package from disk…" or a git URL
├── Runtime/
│   ├── AurixVoiceClient.cs      high-level client: connect → bind media → join channels → audio/events
│   ├── Protocol/                AURX packet codec (CRC32, HMAC-SHA256, replay window), control-message JSON
│   ├── Transport/               ControlChannel (ClientWebSocket), MediaTransport (UDP, SessionBind, heartbeats)
│   ├── Audio/                   IOpusCodec abstraction, JitterBuffer, RemoteMixer
│   └── Unity/AurixVoiceBehaviour.cs  MonoBehaviour: microphone capture + AudioSource playback
├── Samples~/Concentus/          IOpusCodec implementation on top of Concentus (pure C# Opus)
└── DotNet/                      .NET solution: library build, xunit tests, headless two-client E2E demo
```

## Security model (mirrors the server)

1. The game backend calls `POST /v1/tokens` with its API key and hands the **per-user JWT** to the client.
   API keys never ship inside a build.
2. The client opens the WebSocket with subprotocols `aurix` and `bearer.<jwt>`; the server replies `SessionInitAck`
   with `session_id`, `ssrc`, `media_addr` and a base64 **media key** unique to this session.
3. The client sends an HMAC-SHA256-signed `SessionBind` (session id, timestamp, nonce) over UDP and waits for a signed
   `SessionBindAck`. From then on the server only accepts media for this SSRC from that source address.
4. Every uplink packet is signed with the media key; every downlink packet is verified before it reaches your audio
   code (`PacketsBadAuth` / `PacketsReplayed` counters expose rejected traffic). Replays are dropped with a 64-packet
   window per remote SSRC, identical to the server.

## Unity quick start

1. Add the package (`Window ▸ Package Manager ▸ + ▸ Add package from disk… ▸ sdk/unity/package.json`).
2. Provide an Opus codec: import the **Concentus** sample from the package and drop the `Concentus` 2.x DLL
   (netstandard2.0 build from NuGet) into `Assets/Plugins/`, or write your own `IOpusCodec` around libopus/UnityOpus.
3. Set *Project Settings ▸ Audio ▸ System Sample Rate* to **48000**.
4. Add `AurixVoiceBehaviour` to a GameObject with an `AudioSource`, then from your code:

```csharp
var voice = GetComponent<AurixVoiceBehaviour>();
voice.CodecFactory = () => new Aurix.Samples.ConcentusOpusCodec();
voice.WebSocketUrl = "wss://voice.example.com/ws";
voice.Token = jwtFromYourBackend;
voice.ChannelId = channelUuid;
await voice.Connect();
voice.SetMuted(false);
```

Or drive `AurixVoiceClient` directly for full control (custom mic pipeline, spatial audio per participant):

```csharp
var client = new AurixVoiceClient(wsUrl, jwt);
client.OnParticipantJoined += (channel, p) => Debug.Log($"{p.DisplayName} joined (ssrc {p.Ssrc})");
client.OnSpeaking += (channel, p, speaking) => ShowIndicator(p.UserId, speaking);
client.OnRecordingNotification += (rec, ch, state) => { if (state == "consent_requested") Prompt(rec); };
client.OnBitrateCommand += (kbps, _) => encoder.SetBitrate((int)kbps * 1000);

var session = await client.ConnectAsync();              // WS auth + UDP SessionBind
var roster  = await client.JoinChannelAsync(channelId);  // ChannelJoinAck roster

void Update() {
    client.Update();                                     // raise events on the main thread, send pings
    client.SendOpusFrame(AurixVoiceClient.ChannelHash(channelId), opusBytes, opusLen); // one 20 ms frame
    while (client.TryDequeueAudio(out var a)) mixer.Push(a.SenderSsrc, a.Sequence, a.Volume, a.Opus);
}
void OnAudioFilterRead(float[] data, int ch) => mixer.Mix(data, ch);
```

`UpdatePositionAsync` sends 3D positions for server-side attenuation (the downlink carries a per-packet volume byte
the mixer applies), `RespondToRecordingAsync` answers consent prompts, `ReportQualityAsync` feeds the server's
bitrate adaptation.

## .NET: build, test, end-to-end demo

```bash
cd sdk/unity/DotNet
dotnet build                     # library (netstandard2.1) + tests + demo, warnings as errors
dotnet test                      # packet layout, CRC32/UUID vectors, HMAC tamper detection, replay window, JSON, jitter buffer
AURIX_API_KEY=aurx_... dotnet run --project Aurix.Demo -- --api http://127.0.0.1:8080 --ws ws://127.0.0.1:8081/ws
```

The demo creates a channel, issues two tokens, connects "alice" and "bob" over real UDP, streams an Opus-encoded
440 Hz tone for half the run and mutes for the other half, and asserts: all packets verified (0 bad auth / replays),
decoded RMS ≈ 0.35, speaking / mute / leave events observed by the peer. It prints `RESULT: PASS` and exits 0.

## Notes

* Threading: network I/O runs on thread-pool tasks; all events fire from `Update()` on the caller's thread.
  `JoinChannelAsync` completes even if `Update()` is not being pumped yet.
* Audio format: 48 kHz, 20 ms frames (`AudioFormat`). Mono uplink; the mixer outputs to any channel count.
* Not supported: WebGL (no UDP) — use the browser SDK; IL2CPP works (no reflection, no dynamic code).
