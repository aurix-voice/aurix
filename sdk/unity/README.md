# Aurix Voice SDK for Unity / .NET

Native client for the Aurix Voice Platform. Speaks the **AURX** UDP media protocol directly
(no WebRTC stack needed), with the control plane over WebSocket. Works in Unity 2021.3+ (.NET Standard 2.1,
all platforms except WebGL — use the [Web SDK](../web) there) and in plain .NET (dedicated servers, bots, tests).

```
sdk/unity/
├── package.json                 UPM package (com.aurix.voice) — add via "Add package from disk…" or a git URL
├── Runtime/
│   ├── AurixVoiceClient.cs      high-level client: connect → bind media → join channels → audio/events
│   ├── Protocol/                AURX v2 packet codec (CRC32, AES-256-CTR + HMAC-SHA256, key derivation, replay window), control-message JSON
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
3. The client derives the session `MediaKeys` (auth / encryption / IV-salt sub-keys) from the media key and sends an
   HMAC-signed `SessionBind` (session id, timestamp, nonce; the only packet that is not encrypted) over UDP, then
   waits for a sealed `SessionBindAck`. From then on the server only accepts media for this SSRC from that source address.
4. Every uplink packet is sealed (AES-256-CTR payload + HMAC tag over header and ciphertext); every downlink packet is
   opened (verified, then decrypted) before it reaches your audio code (`PacketsBadAuth` / `PacketsReplayed` counters
   expose rejected traffic). Replays are dropped with a 64-packet window per remote SSRC, identical to the server.

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

`UpdatePositionAsync` sends the player's pose for server-side positional audio (see below),
`RespondToRecordingAsync` answers consent prompts, `ReportQualityAsync` feeds the server's bitrate adaptation.

### Positional / directional audio

In a `positional` channel the server places every speaker for every listener from the poses the clients publish.
Send the local player's transform whenever it changes (a few times per second is enough); the orientation is the
listener's forward and up vectors in the game's own coordinates:

```csharp
var t = playerTransform;
await client.UpdatePositionAsync(arenaId, myUserId,
    new Position3D { X = t.position.x, Y = t.position.y, Z = t.position.z },
    new Orientation3D { ForwardX = t.forward.x, ForwardY = t.forward.y, ForwardZ = t.forward.z,
                        UpX = t.up.x, UpY = t.up.y, UpZ = t.up.z });
client.OnPositions += (channelId, poses) => { /* others' poses, if you want to mirror them */ };
```

Nothing is heard until both sides have reported a pose, nothing beyond the channel's `max_radius`, and the distance
roll-off (`near_distance` → `far_distance`) arrives in the per-packet volume byte together with your participant
volumes and channel focus. When the channel was created with `positional_config.directional: true` every downlink
packet also carries `PacketFlags.Directional` + two bytes — azimuth (`0` ahead, `+π/2` right, `±π` behind) and
elevation (`+π/2` above) of the speaker relative to *your* orientation, quantised to `-127..127`
(`AurxPacket.TakeDownlinkMeta` → `Direction`). `RemoteMixer.Push(ssrc, seq, volume, direction, opus)` pans the
decoded mono stream across the first two output channels with constant-power gains (`Direction.StereoGains`:
centre `(1, 1)`, hard right `(0, √2)`), so a teammate on your right stays on your right when you turn; elevation is
exposed but not rendered. Left/right follow the channel's `coordinate_system` (`left_handed`, Unity's `X` right /
`Y` up / `Z` forward, by default; `right_handed` mirrors them). Keep the `AudioSource` non-spatialised (2D) — the
server already did the panning; packets without the flag stay centred, and a mono output ignores the pan.

### Local mute, per-participant volume, block

Receiver-local controls: they change what *this* client hears, are enforced by the server before audio is
forwarded (so muted/blocked players cost no downlink bandwidth), and the other player is never notified.

```csharp
await client.SetParticipantMutedAsync(userId, true, channelId);   // silence them in one channel
await client.SetParticipantMutedAsync(userId, true);              // …or everywhere
bool muted = client.IsParticipantMuted(userId, channelId);
await client.SetParticipantVolumeAsync(userId, 0.5f);             // 0 … 1 (unity) … MaxParticipantVolume (2, ≈ +6 dB)
await client.SetUserBlockedAsync(userId, true);                   // persistent, mutual; survives sessions and channels
client.OnUserBlockChanged    += (uid, blocked) => RefreshBlockList(client.BlockedUsers);
client.OnReceiverPreferences += prefs => { /* server-side state at session start (blocks from the DB) */ };
```

The volume is folded into the per-packet volume byte (`128` = unity, so `a.Volume` already includes it together with
positional attenuation). Mutes and volumes are session state and are replayed automatically after a non-resumed
reconnect (channel-scoped mutes when the channel is re-joined); blocks are stored per application on the server
and can also be managed from your backend via `/v1/users/:id/blocks`.

### Multiple channels: transmission policy, focus, channel limit

A session may sit in several channels at once (team + party + proximity…). `AurixVoiceBehaviour.ChannelId`
takes a comma-separated list and sends the microphone with `TransmitOpusFrame`, which addresses every joined
channel the current transmission mode allows:

```csharp
await client.JoinChannelAsync(teamId);
await client.JoinChannelAsync(partyId);

await client.TransmitToChannelAsync(partyId);            // speak to the party only, keep hearing the team
await client.SetTransmissionAsync(TransmissionMode.All);  // default: every joined channel
await client.SetTransmissionAsync(TransmissionMode.None); // listen only (server-side push-to-talk release)
bool reaches = client.TransmitsTo(teamId);
client.OnTransmissionChanged += mode => pttIndicator.Set(mode);

int sentTo = client.TransmitOpusFrame(opusBytes, opusLen, AudioFormat.FrameSamples, vad.Level);
client.SendOpusFrame(AurixVoiceClient.ChannelHash(teamId), opusBytes, opusLen); // explicit target; skipped if the mode excludes it

await client.SetChannelFocusAsync(teamId);                // team at full volume, the rest attenuated
await client.SetChannelFocusAsync(null);                  // everything at full volume again
client.OnChannelFocusChanged += channelId => Highlight(channelId);
```

The mode is enforced by the server (frames for channels outside it are dropped before fan-out), the client just
saves the uplink. `Single` and the focus must point at a joined channel: set before the join they are sent with its
`ChannelJoinAck`; leaving that channel resets them (`OnTransmissionChanged(None)` / `OnChannelFocusChanged(null)`).
Focus is receiver-local — the other channels are scaled by the server's `media.unfocused_channel_gain` (0.5 by
default) into the per-packet volume byte, on top of per-participant volume; mutes and blocks still win. Both are
replayed after a reconnect. `IncomingAudio.ChannelHash` tells which channel a frame was forwarded through.

The server caps memberships per session (`media.max_channels_per_session`, default 10; positional channels
`media.max_positional_channels_per_session`, default 1) — `JoinChannelAsync` faults with `CHANNEL_LIMIT_EXCEEDED`.

### Reconnect / session resume

`AutoReconnect` is on by default. When the control connection drops unexpectedly (Wi-Fi ↔ LTE
handover, NAT rebinding, server restart) the client keeps its session and retries with exponential
backoff (`client.Reconnect`: 500 ms → 8 s, ±30 % jitter, 10 attempts), presenting the one-time
resume token from `SessionInitAck`:

```csharp
client.OnRecovering      += (attempt, delay, cause) => ShowBanner($"Reconnecting… ({attempt})");
client.OnRecovered       += info => HideBanner(info.Resumed ? "resumed" : "rejoined");
client.OnFailedToRecover += e => ShowError(e.Message);        // State is now Failed
client.OnSessionClosed   += reason => { /* kicked/banned/shutdown: no reconnect follows */ };
```

* Within the server's grace window (`client.ResumeGrace`, default 30 s) the same session comes back
  (`info.Resumed == true`): same SSRC, same media key, same channels — other players never see a leave.
  The server replays one `ChannelJoinAck` per channel, so `OnChannelJoined` fires again with a fresh roster.
* After the grace window a fresh session is issued (`info.Resumed == false`): `OnChannelLeft` fires for the
  old channels and they are re-joined with the same token; the SSRC changes.
* The UDP media path is re-bound from a new local port either way (`SessionBind` with the kept key), and the
  uplink sequence continues where it left off so the server's replay window keeps accepting packets.
* `SendOpusFrame` is a silent no-op while `State == Reconnecting`; keep the microphone running.
* Two unanswered pings (`PingInterval`) close a half-open socket and start the reconnect.
  `ReconnectNow()` skips the current backoff delay (e.g. when the OS reports connectivity is back).
* `DisconnectAsync()` and a server-side `SessionClose` never trigger a reconnect.

### One-time action tokens

Instead of the reusable player JWT the backend can mint single-use tokens with
`POST /v1/tokens/action` (90 s by default): one `login`, `join`, `kick`, `mute` or `unmute` each;
a replay on any node fails with `TOKEN_REUSED`. With `auth.require_action_tokens = true` on the
server they are the only accepted credentials for `OpenSessionAsync` and `JoinChannelAsync`.

```csharp
var client = new AurixVoiceClient(wsUrl, await Backend.ActionTokenAsync("login"))
{
    TokenRefresher    = ct => Backend.ActionTokenAsync("login"),               // before each reconnect
    JoinTokenProvider = (channelId, ct) => Backend.JoinTokenAsync(channelId),  // per JoinChannelAsync
};
await client.OpenSessionAsync();
await client.JoinChannelAsync(channelId);                 // uses JoinTokenProvider
await client.JoinChannelAsync(channelId, explicitToken);  // …or pass one yourself

// in-game moderation: the backend binds the token to actor + channel + target
var kick = await Backend.ModerationTokenAsync("kick", channelId, targetUserId);
await client.ModerateAsync(channelId, targetUserId, ModerationAction.Kick, kick, "afk");
```

A `login` token that opened a session may still be presented to *resume* that session inside the
grace window; `TokenRefresher` supplies the credential for every reconnect attempt (a fresh one is
required once the server hands out a new session). `ModerateAsync` completes on
`ModerateParticipantAck` and otherwise throws `InvalidOperationException("<CODE>: <message>")`
with the server's error code (`TOKEN_REUSED`, `ACTION_TOKEN_REQUIRED`, `AUTH_DENIED`, …).

### Text chat (lite)

Party/team text, `/`-commands and map pings ride on the control WebSocket: channel messages to
channels you are a member of, directed messages to a user who is online in the same app, and
typing indicators. No history, no offline delivery, no read state. Persistent blocks
(`SetUserBlockedAsync`) suppress text like voice, both ways.

```csharp
client.OnChatMessage += m =>
{
    // m.IsOwn — my echo (the send task completes with the same object), m.IsSystem — from the
    // server (POST /v1/channels/:id/messages, FromUserId == ChatMessage.SystemUserId),
    // m.IsDirect — DM; m.Metadata is the parsed JSON (Dictionary<string, object> / List<object>)
    ui.Append(m.ChannelId, m.DisplayName, m.Text, m.SentAt);
};
client.OnParticipantTyping += (channelId, userId, typing) => ui.ShowTyping(userId, typing);

var sent = await client.SendMessageAsync(channelId, "gg",
    metadata: new Dictionary<string, object> { { "ping", new Dictionary<string, object> { { "x", 12.5 }, { "y", 8.0 } } } },
    clientRef: localId);                              // optional; echoed only to you → reconcile optimistic UI
await client.SendDirectMessageAsync(userId, "psst");  // target must be online in this app
await client.SetTypingAsync(channelId, true);         // call on every keystroke; coalesced to one frame per 1.5 s
await client.SetTypingAsync(channelId, false);        // always sent
```

The tasks complete with the server-stamped message (`Id`, `SentAt`) and otherwise throw
`InvalidOperationException("<CODE>: <message>")`: `AUTH_DENIED` (not a member, or a block between
the two users), `USER_MUTED` (server-muted while `chat.server_mute_blocks_text`),
`VALIDATION_ERROR` (empty, too long, control characters, self-DM), `USER_OFFLINE`,
`RATE_LIMIT_EXCEEDED` (anti-flood, per session), `MESSAGE_BLOCKED` (content filter),
`CHAT_DISABLED`. Only the send whose `client_ref` the server echoes back fails; unrelated `Error`
frames go to `OnServerError`. Events fire from `Update()` like everything else.

### Audio energy / voice activity

`AurixVoiceBehaviour` runs a `VoiceActivityDetector` on the mono PCM it captures (inspector:
`VadThreshold` — linear RMS, 0.01 ≈ −40 dBov; `VadHangoverFrames` — 20 ms frames of silence before
speech ends, 15 ≈ 300 ms; `GateOnVad` — stop sending frames while silent, off by default) and
raises `OnLocalSpeaking`. Every frame is labelled with the measured level: the AURX packet gets
`PacketFlags.Energy` and a leading RFC 6464 byte (`0` = full scale, `127` = silence, else `-dBov`),
which the server strips before forwarding and uses for `SpeakingStateChanged` (only frames at or
above `media.speaking_energy_threshold` count) and for `ChannelEnergy`. With your own pipeline:

```csharp
var vad = new VoiceActivityDetector { Threshold = 0.01f, HangoverFrames = 15 };
if (vad.Process(monoPcm, AudioFormat.FrameSamples)) OnMicActivity(vad.Speaking);
if (gate && !vad.Speaking) client.SkipFrame();            // keep the RTP clock running without sending
else client.SendOpusFrame(hash, opus, len, AudioFormat.FrameSamples, vad.Level);

client.OnChannelEnergy += (channel, levels) => { foreach (var l in levels) SetBar(l.UserId, l.Energy); };
// participant.Energy holds the last reported linear level 0..1 (0 when silent / decayed)
```

`AudioLevel.Encode/Decode/Rms` convert between linear RMS and the wire byte. Frames sent without a
level (`SendOpusFrame(hash, opus, len)`) still work — the server then treats every arriving frame as
speech, as before.

### Devices, input gain, speaker mute

```csharp
foreach (var d in AurixVoiceBehaviour.InputDevices) micDropdown.Add(d);   // Microphone.devices
voice.SetInputDevice(micDropdown.Selected);   // hot-swap; null/"" = system default; false for an unknown name
voice.OnInputDeviceChanged += name => micDropdown.Selected = name;   // also fired on fallback
voice.ActiveInputDevice;                      // what is actually being captured
voice.SetInputGain(1.5f);                     // 0..AudioLevel.MaxInputGain (4); 1 = unity, applied before VAD + Opus
voice.SetOutputVolume(0.8f);                  // 0..AudioLevel.MaxOutputVolume (2) master volume of the remote mix
voice.SetOutputMuted(true);                   // speaker mute: keep sending, hear nothing
```

`InputGain`, `OutputVolume`, `OutputMuted` are also inspector fields. Switching the microphone keeps the
encoder, VAD and mute state; if `Microphone.Start` fails or the device stops recording (unplugged), capture
falls back to the system default and `OnInputDeviceChanged` reports it. Gain scales the mono PCM and
hard-clips at ±1, so the transmitted level and the local VAD see the same signal. With your own pipeline:
`AudioLevel.ApplyGain(pcm, count, gain)` on the uplink, `RemoteMixer.OutputVolume` / `OutputMuted` on the
downlink — a muted mixer still consumes and decodes frames, so the jitter buffers stay in sync and unmuting is
instant. None of this is signalled to the server; use `SetMuted` for a microphone mute other players see.

## .NET: build, test, end-to-end demo

```bash
cd sdk/unity/DotNet
dotnet build                     # library (netstandard2.1) + tests + demo, warnings as errors
dotnet test                      # packet layout, CRC32/UUID vectors, seal/open + tamper detection, server wire vectors, replay window, JSON, jitter buffer
AURIX_API_KEY=aurx_... dotnet run --project Aurix.Demo -- --api http://127.0.0.1:8080 --ws ws://127.0.0.1:8081/ws
```

The demo creates a channel, issues two tokens, connects "alice" and "bob" over real UDP, streams an Opus-encoded
440 Hz tone for half the run and mutes for the other half, and asserts: all packets verified (0 bad auth / replays),
decoded RMS ≈ 0.35, speaking / mute / leave events observed by the peer, the local VAD reports speech, the peer
receives a matching `ChannelEnergy` level (≈ 0.35), and a speaker-muted mixer outputs silence while packets keep
flowing and decoding. It prints `RESULT: PASS` and exits 0.

`--scenario reconnect` runs the reconnect check instead: alice's control connection goes through a local
TCP proxy that is cut abruptly — once within the grace window (same session and SSRC must resume, audio
must keep flowing, bob must not see a leave), once for longer than it (a fresh session must re-join the
channel), and once for good (the client must give up with `OnFailedToRecover`). Run the server with
`AURIX__SERVER__SESSION_RESUME_GRACE_SECS=4` to keep the run short.

`--scenario prefs` checks receiver-local mute / volume / block end to end over real UDP: bob mutes alice in the
channel and everywhere (0 packets), unmutes (audio resumes), sets volume 0.5 and 2.0 (volume byte decodes to
0.5 / ≈1.99), blocks her (silence, persisted into a fresh session, alice never notified) and unblocks.

`--scenario chat` exercises text over the real control connection: channel message with metadata (sender echo
with `client_ref`, recipient copy without), rejections carried back to the right send (empty text, self-DM,
offline target), a directed message, typing coalescing (3 calls → 1 frame, origin never notified) and the
anti-flood limit (a 14-message burst: `message_burst` accepted, the rest `RATE_LIMIT_EXCEEDED`, none hanging).

`--scenario transmission` (needs the API key: it creates a second channel and multi-channel tokens) checks the
multi-channel controls over real UDP: `Single(party)` set before the party join sends nothing and is acked with
the join, then only party frames arrive; `All` reaches both channels; `None` reaches nobody and `SendOpusFrame`
drops locally; bob's `focus(team)` turns the party volume byte into 0.5 while alice's focus stays untouched;
leaving the target / focused channel resets both through server events.

`--scenario positional` (needs the API key: it creates a directional positional channel) checks directional audio
over real UDP: no frames before both poses are known; alice 2 m to bob's right → azimuth `+π/2` and the decoded
stereo mix lands entirely in the right channel; bob turns to face her → azimuth `0`, both channels equal;
alice behind-left at 12 m with bob's local volume 0.5 → azimuth `-3π/4`, volume byte 0.25 and a left-heavy mix;
beyond `max_radius` → nothing.

## Notes

* Threading: network I/O runs on thread-pool tasks; all events fire from `Update()` on the caller's thread.
  `JoinChannelAsync` completes even if `Update()` is not being pumped yet.
* Audio format: 48 kHz, 20 ms frames (`AudioFormat`). Mono uplink; the mixer outputs to any channel count.
* Not supported: WebGL (no UDP) — use the browser SDK; IL2CPP works (no reflection, no dynamic code).
