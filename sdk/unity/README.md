# Aurix Voice SDK for Unity / .NET

Native client for the Aurix Voice Platform. Speaks the **AURX** UDP media protocol directly
(no WebRTC stack needed), with the control plane over WebSocket. Works in Unity 2021.3+ (.NET Standard 2.1,
every platform) and in plain .NET (dedicated servers, bots, tests). **Unity WebGL** players cannot open UDP
sockets, so there the same C# API runs on top of the browser's WebRTC through the [Web SDK](../web) —
see [Unity WebGL](#unity-webgl-browser-webrtc-through-the-web-sdk).

```
sdk/unity/
├── package.json                 UPM package (com.aurix.voice) — add via "Add package from disk…" or a git URL
├── Runtime/
│   ├── AurixVoiceClient.cs      high-level client: connect → bind media → join channels → audio/events
│   ├── IAurixVoiceClient.cs     the API both clients implement (native and WebGL)
│   ├── Protocol/                AURX v2 packet codec (CRC32, AES-256-CTR + HMAC-SHA256, key derivation, replay window), control-message JSON
│   ├── Transport/               ControlChannel (ClientWebSocket), MediaTransport (UDP, SessionBind, heartbeats)
│   ├── Audio/                   IOpusCodec abstraction, JitterBuffer, RemoteMixer
│   ├── WebGL/                   AurixWebGLVoiceClient: the same API over the browser Web SDK (Unity WebGL players)
│   ├── Plugins/WebGL/AurixWebGL.jslib  Emscripten plugin: loads aurix-web-sdk.js, forwards JSON calls to it
│   └── Unity/                   AurixVoiceBehaviour (native: microphone + AudioSource), AurixWebGLVoiceBehaviour (WebGL)
├── Samples~/Concentus/          IOpusCodec implementation on top of Concentus (pure C# Opus)
├── Samples~/VoiceQuickstart/    sample scene: connect, roster, mute/PTT, quality bars, stats, reconnect, chat
└── DotNet/                      .NET solution: library build, xunit tests, Unity compile check, headless two-client E2E demo
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

Fastest path: import the **Voice quick start** sample (see `Samples~/VoiceQuickstart/README.md`) and press Play —
it is a complete, IMGUI-driven client you can copy from. The manual route:

1. Add the package (`Window ▸ Package Manager ▸ + ▸ Add package from disk… ▸ sdk/unity/package.json`).
2. Provide an Opus codec: import the **Concentus** sample from the package and drop the `Concentus` 2.x DLL
   (netstandard2.0 build from NuGet) into `Assets/Plugins/` (pure C#, works everywhere), or ship the native
   core for libopus quality/CPU (`NativeOpusCodec`, see [Opus codec and encoder controls](#opus-codec-and-encoder-controls)).
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
client.Encoder = encoder;                                // policy + BitrateCommand retune it for you

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

### Opus codec and encoder controls

Two codecs implement `IOpusCodec` (plus the optional `IOpusEncoderControls` / `IOpusFecDecoder`):

| | `ConcentusOpusCodec` (sample) | `NativeOpusCodec` |
|---|---|---|
| implementation | pure C# Concentus 2.x, no native binaries | libopus statically linked into the Aurix native core (`aurix_client`) |
| CPU | ~5–10× libopus; keep complexity ≤ 5 on mobile | libopus |
| controls | all of `OpusEncoderSettings` | all of `OpusEncoderSettings` |
| FEC decode | yes | yes |
| platforms | everything Unity runs C# on | wherever you build the core: `cargo build -p aurix-client --release` or `sdk/unreal/AurixVoice/build_native.*` |

Native binaries go where Unity's P/Invoke finds them (`Assets/Plugins/x86_64/aurix_client.dll`,
`Assets/Plugins/Linux/x86_64/libaurix_client.so`, `Assets/Plugins/macOS/libaurix_client.dylib`,
`Assets/Plugins/Android/<abi>/libaurix_client.so`; on iOS link the static `libaurix_client.a` — the
symbols resolve through `__Internal`). Projects that ship no binary are untouched: `NativeOpusCodec.IsAvailable`
probes once and lets you fall back to Concentus:

```csharp
voice.CodecFactory = NativeOpusCodec.IsAvailable
    ? () => new NativeOpusCodec(AudioFormat.SampleRate, 1, voice.EncoderSettingsFromInspector())
    : () => new ConcentusOpusCodec();
```

`OpusEncoderSettings` carries every libopus encoder control: `BitrateBps` (6 000..300 000 mono, ..510 000 stereo), `Complexity` (0..10),
`MaxBandwidth` (narrowband 4 kHz … fullband 20 kHz), `Signal` (Auto/Voice/Music → application + signal hint),
`Vbr`, `ConstrainedVbr`, `Fec`, `ExpectedLossPercent`, `Dtx` and `Channels` (1, or 2 for a stereo uplink — see
below). `AurixVoiceBehaviour` exposes them in the
inspector (*Opus encoder*); `ApplyEncoderSettings()` re-reads them at runtime. Three layers decide what the
encoder actually runs (`client.EffectiveEncoderSettings`):

1. **baseline** — `client.SetEncoderSettings(...)` (the inspector fields);
2. **channel audio policy** — the operator's `ChannelConfig` (bitrate, min bitrate, FEC, DTX, max bandwidth,
   complexity hint, signal), delivered in `ChannelJoinAck.audio` and live via `ChannelAudioPolicy` when the
   channel is edited over REST. Policies of all joined channels are merged (max bitrate, widest bandwidth,
   FEC if any wants it, DTX only if all allow it, Music > Voice > Auto) and raised as `client.OnAudioPolicyChanged`;
   `FollowChannelPolicy = false` ignores it. `client.SetComplexity(n)` pins the complexity — the CPU budget is
   the game's decision, so the behaviour always pins its inspector value;
3. **server bitrate command** — the adaptive `BitrateCommand{TargetBitrateKbps, Reason, ExpectedLossPercent}`
   (`client.OnBitrateCommand`), transient and clamped by the server to the policy's `MinBitrateBps..=BitrateBps`;
   it also raises `ExpectedLossPercent` so FEC covers the observed loss. Cleared by `SetEncoderSettings`.

Set `client.Encoder = codec` and the client pushes every change through `IOpusEncoderControls.Apply`
(or `SetBitrate` for a codec without controls); `client.OnEncoderSettingsChanged` reports what was applied.
`RemoteMixer` uses `IOpusFecDecoder` when a packet is missing and its successor has already arrived
(`VoiceStats.FramesFecRecovered`), and falls back to PLC otherwise.

### Stereo uplink (music / broadcast sources)

Voice is mono end to end. For a music bot, DJ deck or stereo microphone pair set `Channels = 2`
(`AurixVoiceBehaviour.Stereo`) and give the behaviour a two-channel codec:

```csharp
voice.Stereo = true;                                                  // inspector: Stereo
voice.Signal = OpusSignal.Music;                                      // OPUS_APPLICATION_AUDIO
voice.StereoCodecFactory = () => new ConcentusOpusCodec(48000, 2);    // or NativeOpusCodec(48000, 2)
```

* The channel must allow it: `AudioPolicy.Stereo` (`ChannelConfig.stereo`) — in a voice channel the followed
  policy forces the encoder to one channel; `FollowChannelPolicy = false` leaves the decision to you. The
  stereo bitrate ceiling is 510 kbit/s. PCMU is always mono.
* The microphone's first two channels are L/R (a mono device is duplicated). The capture DSP (AEC/NS/AGC)
  is voice-only and is bypassed for stereo frames; `InputGain`, VAD and energy still apply (on the L/R
  average). Without a `StereoCodecFactory` the behaviour logs a warning and stays mono.
* Receiving needs no setup: `RemoteMixer` reads each Opus packet's channel flag (`OpusPacket.IsStereo`),
  swaps a stream to the stereo factory on its first stereo packet (a stereo decoder plays later mono
  packets fine), keeps the L/R image for non-positional senders, downmixes before panning directional
  ones and averages L/R for a mono output. With no stereo factory the mono factory decodes stereo packets
  downmixed.

### PCMU (G.711) fallback for weak devices

A session can run on **G.711 μ-law** instead of Opus — 8 kHz, 64 kbit/s, a lookup table with no encoder
state, for hardware where even Concentus at complexity 0 does not fit. The codec is negotiated per
session; the server transcodes at the edge, so every other participant keeps Opus and notices nothing
(the PCMU listener hears narrowband audio).

```csharp
// AurixVoiceBehaviour: PreferredCodec = AudioCodec.Pcmu in the inspector, or at runtime:
await voice.SetAudioCodec(AudioCodec.Pcmu);      // ActiveCodec flips when the server acks

// AurixVoiceClient with your own capture:
var pcmu = new PcmuCodec();                      // IOpusCodec: 48 kHz float in/out, μ-law on the wire
client.OnAudioCodecChanged += codec => Debug.Log($"codec now {codec}");
await client.SetAudioCodecAsync(AudioCodec.Pcmu);
int n = pcmu.Encode(mono48k, AudioFormat.FrameSamples, ulaw);          // 960 samples → 160 bytes
client.TransmitAudioFrame(client.AudioCodec, ulaw, n, AudioFormat.FrameSamples, vad.Level);
await client.SetAudioCodecAsync(AudioCodec.Opus);                       // back to Opus
```

* `client.AudioCodec` is what the server acknowledged (`AudioCodecChanged`), `PreferredAudioCodec` what
  you asked for; the behaviour picks its encoder from `AudioCodec` on every frame, so no frame is sent in
  the wrong codec around a switch. A resumed session keeps the codec; after a fresh session the client
  re-sends `SetAudioCodec` for a non-Opus preference.
* Downlink frames arrive flagged `PacketFlags.Pcmu`; `IncomingAudio.Codec`/`Payload` tell you which
  (the old `Opus` field is kept but obsolete), and `RemoteMixer` decodes PCMU and Opus streams side by
  side, replacing a stream's decoder when its codec changes. `SendOpusFrame`/`TransmitOpusFrame` still
  work and simply mean `AudioCodec.Opus`.
* `PcmuCodec.SetBitrate` is a no-op and `OpusEncoderSettings` / channel audio policies are not applied
  while PCMU is active. Not available on WebRTC sessions or for E2EE frames; the node may refuse with
  `CODEC_NOT_AVAILABLE` (`media.pcmu_fallback = false`).

### When UDP is blocked: the WebSocket tunnel

`MediaPathPolicy.Auto` (default) binds media over UDP and, if the node never answers, sends the very same
sealed AURX packets as binary frames over the control WebSocket instead — no extra connection or
credential. While on UDP, `UdpFallbackLostHeartbeats` (3) unanswered heartbeats (`MediaHeartbeatInterval`,
5 s) switch to the tunnel mid-call; while tunnelled, every `UdpReprobeInterval` (30 s, `TimeSpan.Zero` =
never) UDP is tried again and taken back the moment it answers. `UdpOnly` keeps the old behaviour,
`TunnelOnly` never opens a socket. The behaviour exposes the same three knobs (`MediaPath`,
`UdpFallbackLostHeartbeats`, `UdpReprobeIntervalSeconds`).

```csharp
client.OnMediaPathChanged += (path, why) => Debug.Log($"media over {path}: {why}");
client.ActiveMediaPath;                       // MediaPath.Udp / Tunnel (None before the first bind)
client.Session.MediaTunnel;                   // node advertises the tunnel (media.media_tunnel)
var s = client.GetStats();                    // s.MediaPath, s.HeartbeatsLostConsecutive, s.UplinkDropped
```

* One `SequenceCounter` is shared by both links, so a switch is invisible to the server's replay window
  and to other players' jitter buffers; codecs, mute, quality reports, reconnect and resume work the same.
* `UplinkDropped` counts frames the tunnel's bounded outbox (`ControlChannel.MediaQueueLength` = 64)
  refused while TCP was stalled — audio never blocks the game thread. The tunnel inherits TCP head-of-line
  blocking: it keeps the player in the call, UDP remains the path to be on.
* Custom pipelines: `MediaTransport.OverTunnel(IMediaTunnel, sessionId, ssrc, mediaKey, sequence)`;
  `ControlChannel` implements `IMediaTunnel`.

### Choosing a region

Session resume is node-local, so a player should connect to the *nearest node with capacity* and keep its
direct URL. Either use the `endpoint` your backend receives from `POST /v1/tokens` (pass `region` /
`location` hints there), or measure from the device:

```csharp
using var http = new HttpClient();
var regions = await RegionDiscovery.DiscoverAsync(http, "https://voice.example.com", jwt, new RegionDiscoveryOptions
{
    PreferredRegion = partyLeaderRegion,           // optional: ranks first when reachable
    Latitude = 48.9, Longitude = 2.3,              // optional: server orders by distance
    // Probe = true (default): GET each region's ProbeUrl a few times, best RTT wins
});
var best = regions.FirstOrDefault();               // null → no node advertised for discovery
var client = new AurixVoiceClient(best.WsUrl, jwt);
```

Ranking: preferred region (unless its probe failed) → measured RTT in 15 ms buckets (ties keep the
server's distance/load order) → unprobed regions → regions whose probe failed. `RegionDiscovery.Rank` and
`ProbeRttAsync` are public for custom policies; each `RegionEndpoint` also carries `Nodes`,
`LoadFactor` and `DistanceKm`.

### Statistics and network quality bars

`client.GetStats()` returns a `VoiceStats` snapshot; `AurixVoiceBehaviour` wires its `RemoteMixer` into
`client.Mixer` so jitter-buffer counters are included (do the same when you drive the mixer yourself):

```csharp
var s = client.GetStats();
s.Bars;                              // 1 (unusable) … 5 (excellent), from R-factor
s.RFactor; s.Mos;                    // simplified E-model rating 0..100, MOS 1..4.5
s.RttMs; s.RttMinMs; s.RttAvgMs; s.RttMaxMs; // heartbeat RTT on the media path; s.ControlRttMs for the WebSocket
s.JitterMs;                          // RFC 3550 inter-arrival jitter of downlink audio
s.LossPercent;                       // downlink loss over the last period, 0..100
s.PacketsSent; s.BytesSent; s.PacketsReceived; s.BytesReceived;
s.BadAuth; s.Replayed; s.HeartbeatsLost;
s.FramesLost; s.FramesLate; s.Underruns; s.ActiveStreams;  // mixer / jitter buffers, lifetime
s.Server;                            // NetworkQuality? — the server's merged view (see below)

client.OnStats          += s => hud.SetBars(s.Bars);
client.OnNetworkQuality += q => hud.SetServerBars(q.Bars, q.UplinkLossPercent);
```

Every `QualityReportInterval` (5 s; `TimeSpan.Zero` disables) `Update()` samples the stats, raises `OnStats`
and sends a `QualityReport` (RTT, jitter, loss **percent**) that drives the server's adaptive bitrate.
The server merges that downlink report with what the SFU measures on your uplink (sequence gaps, jitter,
bitrate) into a `NetworkQuality` message whose `Bars` is the worse of the two directions; it arrives when
the bar count changes and periodically as a summary (`client.LastNetworkQuality`). Bars use the same
thresholds everywhere (server, native, Web, Unity): R ≥ 80 → 5, ≥ 70 → 4, ≥ 60 → 3, ≥ 50 → 2, else 1.
Packet/byte/frame counters are cumulative for the current media transport; `LossPercent`, `RFactor`,
`Mos` and `Bars` describe the last period.

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

### Per-participant playback (Unity spatialization, HRTF, occlusion)

When you want Unity — or a spatializer plugin (Steam Audio, Resonance, Oculus/Meta XR Audio) — to position each
voice instead of the server's stereo panning, give each avatar its own `AudioSource` + `AurixParticipantAudioSource`
and bind it to the participant:

```csharp
voice.Playback = VoicePlaybackMode.PerParticipant;   // on the AurixVoiceBehaviour (Inspector or code)

// on the avatar prefab: AudioSource (spatialBlend = 1, your rolloff / spatializer / mixer group) +
var src = avatar.GetComponent<AurixParticipantAudioSource>();
src.Bind(participant.UserId);                        // resolves the SSRC, claims the streams
src.OnActiveChanged += talking => lipSync.enabled = talking;
// … avatar destroyed → src.Unbind() (OnDisable/OnDestroy do it too)
```

`AurixParticipantAudioSource.OnAudioFilterRead` pulls that user's decoded PCM (microphone **and** TTS voice) from the
shared `RemoteMixer` with **no** local panning — `RemoteMixer.PullParticipant(ssrc, buf, offset, frames, channels)`
writes the raw voice and Unity's `AudioSource` spatialization (spatial blend, rolloff, spatializer plugin, filters,
reverb zones, mixer routing) is applied afterwards like on any other source. Per-participant volume, the server's
gain byte, master volume and speaker mute still apply; a stereo (music) sender keeps L/R on a stereo output and is
downmixed for mono.

Playback modes on the behaviour:

* `Mixed` (default) — everything through the behaviour's own `AudioSource`, server panning, as before.
* `PerParticipant` — bound sources claim their streams and the behaviour's aggregate `AudioSource` mixes only the
  *unclaimed* participants (`RemoteMixer.Mix(..., exclude)`), so avatars in view can be spatialized while everyone
  else stays 2D and nobody is heard twice.
* `PerParticipantOnly` — the aggregate source is silent; only bound participants are audible.

Claims are per user: a participant that rejoins with a new SSRC (or after node failover) is picked up automatically
(`Bind` re-resolves via `FindByUser`), one that leaves just goes silent (`IsActive` false). `Mixed` server downlink
(`DownlinkMode.Mixed`) carries no per-participant streams. The pulled audio is not fed to echo cancellation by
itself: add `AurixListenerTap` to the `AudioListener` (it forwards Unity's final rendered output, resampled to
48 kHz, to `Dsp.PushRender` and flips `voice.RenderFedExternally` so the behaviour stops pushing only its own
mix). Low-level: `RemoteMixer.Pull(ssrcs, buf, offset, frames, channels)` renders any set of SSRCs, and
`RemoteMixer.GetStreams(list)` (`StreamInfo`: ssrc, mixed, stereo, active, buffered frames, synthesized) enumerates what is buffered.

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
* If the node itself is gone, reconnect attempts rotate through the failover nodes the server advertised
  (`FailoverEndpoints`; attempt 1 → current node, 2 → first failover, 3 → second, … then around again).
  A node that answers takes the session over from its Redis mirror: `OnEndpointChanged(url)` fires, then
  `OnRecovered` with `info.Migrated == true` — same session id and SSRC, new media key and media endpoint
  (re-bound transparently), channels/mutes/codec/downlink mode restored, other players see no leave.
  `Endpoint` names the node in use from then on. Without Redis mirrors on the server such a reconnect is a
  fresh session.
* The UDP media path is re-bound from a new local port either way (`SessionBind` with the kept key), and the
  uplink sequence continues where it left off so the server's replay window keeps accepting packets.
* `SendOpusFrame` is a silent no-op while `State == Reconnecting`; keep the microphone running.
* Three unanswered pings (`PingInterval`) close a half-open socket and start the reconnect.
  `ReconnectNow()` skips the current backoff delay (e.g. when the OS reports connectivity is back).
* `ForceReconnect(reason)` drops a *healthy* control connection on purpose and resumes right away — use
  it when the app learns the network path changed (Wi-Fi ↔ cellular) so media is re-bound from the new
  address instead of waiting for the old socket to time out (~320 ms in the demo).
* `ProbeConnection(timeout = 2 s)` sends an out-of-band ping; with no pong before the deadline the socket
  is treated as dead and the reconnect starts. Meant for the return from background, where the OS often
  kills the TCP socket silently (bytes vanish, no RST) and the regular timeout would take 3 × `PingInterval`.
  A pong for a ping sent *before* the probe does not satisfy it (nonces are compared).
* `DisconnectAsync()` and a server-side `SessionClose` never trigger a reconnect.

### Mobile (iOS / Android)

`AurixVoiceBehaviour` handles the platform quirks; the protocol/media code is the same on every platform.

| Inspector field | Default | Effect |
|---|---|---|
| `RequestMicrophonePermission` | on | Ask for the microphone at runtime before `Microphone.Start` (Android `RECORD_AUDIO` via `UnityEngine.Android.Permission`, iOS via `Application.RequestUserAuthorization`). Off = never prompt, capture only if already granted. |
| `StopMicrophoneInBackground` | on | Release the microphone in `OnApplicationPause(true)`, restart it on return (otherwise the OS shows a "recording" indicator or suspends the app anyway). |
| `ProbeAfterBackgroundSeconds` | 2 | After at least this long in the background, `ProbeConnection()` on resume. 0 = off. |
| `ReconnectOnNetworkChange` | on | Poll `Application.internetReachability` once a second; a change between two *reachable* states calls `ForceReconnect`. Going offline waits (the ping timeout or the OS closing the socket takes over). |

* **Permission denied is not a failure.** The client stays connected as a listener, `PermissionState`
  becomes `Denied`, `OnMicrophonePermissionDenied` fires (show your own explanation UI), and
  `RetryMicrophonePermission()` asks again — Android's "don't ask again" then needs the system settings.
  On desktop the state goes straight to `Granted`.
* **iOS project settings:** fill *Player Settings → Microphone Usage Description* (becomes
  `NSMicrophoneUsageDescription`; the app is killed on first `Microphone.Start` without it). Enable
  *Prepare iOS for Recording* to avoid the first-capture stall and *Force iOS Speakers when Recording* unless
  you want earpiece routing. To keep talking in the background set `StopMicrophoneInBackground = false` and
  enable the *Audio, AirPlay and Picture in Picture* background mode — Apple review expects a visible reason.
* **Android:** Unity adds `RECORD_AUDIO` to the manifest automatically because the `Microphone` class is
  referenced. By default Unity asks for every dangerous permission at startup; add
  `<meta-data android:name="unityplayer.SkipPermissionsDialog" android:value="true" />` to your manifest
  to defer the prompt until `AurixVoiceBehaviour` needs the microphone. Bluetooth
  headsets switch the output rate (44.1 → 16/48 kHz) — see below. `INTERNET` is required (always added).
* **Output sample rate.** Mobile mixers commonly run at 44.1 kHz or 24 kHz, not the 48 kHz Aurix decodes
  at. `OnAudioFilterRead` goes through `OutputResampler` (linear, fractional phase carried across
  callbacks, additive so the AudioSource mix is preserved); at 48 kHz the mixer writes straight through.
  `AudioSettings.OnAudioConfigurationChanged` (headphones/Bluetooth route change) picks up the new rate and
  re-`Play()`s the AudioSource Unity stopped.
* **Microphone sample rate.** Devices that cannot capture at 48 kHz are opened at their maximum
  (`Microphone.GetDeviceCaps`) and downmixed/resampled to 48 kHz mono before VAD/Opus, as on desktop.
* **Battery:** the client is a single UDP socket plus one WebSocket; VAD gating (`GateOnVad`) is the
  biggest saving because silent frames are not encoded or sent.

What was verified here: the Unity-only code compiles under `UNITY_5_3_OR_NEWER` (and `UNITY_ANDROID`)
against a stub of the referenced `UnityEngine` API, the resampler is unit-tested at 44.1/24/96 kHz with
odd block sizes, and `--scenario reconnect` exercises `ForceReconnect` and `ProbeConnection` through a
TCP proxy that stalls the connection. It was **not** run on a phone: permission dialogs, background
suspension and route changes need a device test in your project.

### Unity WebGL (browser WebRTC through the Web SDK)

A WebGL player runs inside a browser tab: no UDP, no `ClientWebSocket`, no threads, and the
microphone/speakers belong to the browser. `AurixVoiceClient` therefore throws
`PlatformNotSupportedException` in WebGL players, and the package ships a second implementation of
the same `IAurixVoiceClient` interface, `Aurix.WebGL.AurixWebGLVoiceClient`, that drives the
browser-native [Web SDK](../web) (WebSocket control plane, **WebRTC** media, browser Opus/AEC/NS/AGC,
browser playback) through a small JavaScript bridge:

```
C#  AurixWebGLVoiceClient ──JSON──▶ Plugins/WebGL/AurixWebGL.jslib ──▶ window.AurixWebSdk.AurixBridge ──▶ AurixClient (Web SDK)
     ▲ Update() drains events  ◀──JSON──                             ◀── ordered, bounded event queue
```

Setup:

1. Build the standalone bundle once: `cd sdk/web && npm ci && npm run build` produces
   `sdk/web/dist/aurix-web-sdk.js` (plain script, no module system, defines `window.AurixWebSdk`).
   Copy it into `Assets/StreamingAssets/` of your project — the default `SdkUrl`
   (`StreamingAssets/aurix-web-sdk.js`) resolves relative to the player's `index.html`. Alternatively
   add `<script src="aurix-web-sdk.js"></script>` to your WebGL template; the plugin picks up an
   already-present `window.AurixWebSdk` and skips the download.
2. `Runtime/Plugins/WebGL/AurixWebGL.jslib` is part of the package and is linked automatically into WebGL
   players (it is ignored on every other platform; the assembly definition compiles for all platforms and
   the native transport/audio classes are compiled out of WebGL players with
   `#if !(UNITY_WEBGL && !UNITY_EDITOR)`). No native `.dll`/`.so` is needed or used in WebGL.
3. Drop `AurixWebGLVoiceBehaviour` on a GameObject (or construct `AurixWebGLVoiceClient` yourself and call
   `Update()` every frame). Inspector fields: API URL (`https://…:8080`, used for TURN credentials and
   chat history), WebSocket URL (`wss://` on `https://` pages), token, channel id(s), `SdkUrl`,
   browser microphone processing (echo cancellation / noise suppression / AGC), input gain, output
   volume / speaker mute, `UseTurn`, `ParticipantStreams` (-1 = node's cap) and `SpatialAudio`.
4. The page origin must be allowed by `AURIX__SERVER__CORS_ORIGINS`, and `media.external_ip` (UDP) or the
   built-in TURN server must be reachable from the browser — the same requirements as the Web SDK.

```csharp
var voice = gameObject.AddComponent<AurixWebGLVoiceBehaviour>();
voice.ApiUrl = "https://voice.example.com:8080";
voice.WebSocketUrl = "wss://voice.example.com:8081/ws";
voice.Token = playerJwt;                                       // from your backend, POST /v1/tokens
voice.TokenRefresher = ct => Backend.FreshTokenAsync();         // optional: refreshToken action tokens
voice.JoinTokenProvider = (channel, ct) => Backend.JoinTokenAsync(channel); // optional: joinToken
await voice.Connect();
var roster = await voice.Client.JoinChannelAsync(channelId);   // same calls and events as AurixVoiceClient
voice.Client.OnSpeaking += (ch, p, on) => hud.SetSpeaking(p.UserId, on);
```

How it works and what is different from the native client:

* **Same API, same events.** Everything in `IAurixVoiceClient` works — join/leave/moderate, local mute,
  per-participant volume, block, transmission mode/focus, positions, recording consent, chat + history +
  read markers + typing, transcripts, TTS (`SpeakAsync(...).Done`), quality/stats events, reconnect with
  resume and cross-node failover (`OnRecovering` / `OnEndpointChanged` / `OnRecovered`). Calls are JSON
  round trips through the bridge; results that need the server come back as tasks completed from
  `Update()`, so continuations run on the main thread (there is no thread pool in WebGL). Token
  callbacks are inverted: when the browser client needs a fresh JWT or a join token it queues a
  `tokenRequest`, the C# client calls your `TokenRefresher` / `JoinTokenProvider` and answers.
* **Audio is the browser's.** Remote voices play through the browser, not through an `AudioSource` —
  no `AudioListener`, mixer groups, spatializer plugins or `AurixParticipantAudioSource`, and there are
  no PCM frames to pull. The server mix plays in a hidden `<audio>` element; on top of it the Web SDK
  negotiates up to `WebGLClientOptions.ParticipantStreams` **per-participant WebRTC tracks** (default:
  as many as the node allows, `media.webrtc_participant_streams`, ≤ 64; `0` = mix only) and
  spatializes them itself with Web Audio — `SpatialAudio = Hrtf` (default), `EqualPower` or `None`
  (tracks negotiated, rendering left to the page) — from the positions you send with
  `UpdatePositionAsync`, exactly as the native client would pan per-participant PCM. Speakers beyond
  the tracks stay in the mix; ambient channels are always mixed. `SetPinnedParticipantsAsync(ids)` keeps
  chosen users on their own track while audible (at most the cap), `OnParticipantStreams` /
  `GetParticipantStreamsAsync()` give the current `WebGLParticipantStream { Mid, UserId?, Live }`
  layout, `GetParticipantStreamCapAsync()` the node's cap, `IsParticipantSpatialized(id)` whether a voice
  currently goes through the HRTF panner.
  Capture uses `getUserMedia` with the browser's echo cancellation / noise suppression / AGC; the
  `IOpusCodec`, `Dsp*`, `MediaPathPolicy`, `PreferredCodec`/PCMU and downlink-mix settings do not apply
  (WebRTC negotiates Opus itself). `GetStatsAsync()` returns `WebGLStats` (WebRTC RTT/jitter/loss/MOS).
* **Autoplay.** Browsers only start audio after a user gesture. `OnRemoteAudio(playing: false, reason)`
  (and `RemoteAudioPlaying` / `RemoteAudioBlockedReason` on the behaviour) tell you playback is blocked;
  call `ResumeAudioAsync()` from a UI button/tap handler to retry — it resumes both the `<audio>` element
  and the Web Audio graph of the per-participant tracks (the browser's transient user activation
  covers a Unity click processed in the same frame). `getUserMedia` requires `https://` (or `localhost`) and prompts for the microphone on connect.
* **Devices.** `EnumerateDevicesAsync`, `SetInputDeviceAsync`, `SetOutputDeviceAsync` (output selection
  only where the browser supports `setSinkId`), `SetInputGain`, `SetOutputVolume`, `SetOutputMuted`.
* **Loading.** `ConnectAsync` loads `SdkUrl` on first use (`PreloadSdk()` to start earlier); a missing or
  broken bundle fails the connect with `WebGLBridgeException` naming the URL. Each browser event queue is
  bounded; if a frame stalls long enough to overflow it, `OnEventsDropped(count)` fires and the client
  re-syncs from the next roster/state events.
* **Editor and other platforms.** The WebGL classes compile everywhere, but the jslib is only linked
  into WebGL players: in the Editor (Play mode) and on desktop/mobile `NativeWebGLBridge` throws
  `PlatformNotSupportedException` — use `AurixVoiceBehaviour` there, or inject your own `IWebGLBridge`
  (the unit tests do exactly that with a scripted fake).

What was verified here: the `.jslib` is evaluated under an Emscripten-like harness against the real
browser bundle (`sdk/web/test/unity-jslib.test.mjs`), the C# client is tested against a scripted
bridge (`WebGLBridgeTests`: results, events, token callbacks, timeouts, overflow, disposal,
participant-track options/layout), and the
Unity compile check runs with `UNITY_WEBGL` (player and editor). A real Unity WebGL build in a browser
was **not** run in this repository — that is the first thing to try in your project.

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
await client.SendDirectMessageAsync(userId, "psst");  // offline target: queued when chat.persist + offline_delivery, else USER_OFFLINE
await client.SetTypingAsync(channelId, true);         // call on every keystroke; coalesced to one frame per 1.5 s
await client.SetTypingAsync(channelId, false);        // always sent

// Stored chat (server runs chat.persist = true): cursor-paged history and read markers.
var page = await client.HistoryAsync(channelId, limit: 50);          // newest first
var older = await client.HistoryAsync(channelId, before: page.NextBefore);
var dm = await client.DirectHistoryAsync(userId, after: lastSeen.Cursor);
await client.MarkReadAsync(channelId, page.Messages[0].Id);          // only moves forward
var markers = await client.ReadMarkersAsync(channelId);              // markers.UnreadCount, markers.Markers
client.OnChatReadMarker += m => ui.ShowReadUpTo(m.UserId, m.MessageId);
client.OnChatInboxSynced += (delivered, truncated) => ui.InboxReady(); // after the offline replay on connect
```

Directed messages that waited for you arrive after connect as ordinary `OnChatMessage` with
`Offline = true` (oldest first), then `OnChatInboxSynced`; every device replays what is still
unread, so dedupe by `Id` and call `MarkDirectReadAsync` once the user has seen them.

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

### Capture processing: echo cancellation, noise suppression, AGC

The microphone is cleaned right after downmix/resampling and before `InputGain`, the injector, VAD and the
encoder. Two implementations sit behind one interface (`ICaptureProcessor`):

| Mode | Implementation | High-pass | AEC | Noise suppression | AGC |
|---|---|---|---|---|---|
| `Native` | `NativeCaptureDsp` — the Aurix native core (`aurix_client` in `Plugins/`, same binary as `NativeOpusCodec`) | 80 Hz | frequency-domain, 40–500 ms tail, delay estimation | RNNoise-derived neural NS | speech-gated, soft limiter |
| `Managed` | `ManagedCaptureDsp` — pure C#, no native dependency, works everywhere incl. IL2CPP | 80 Hz | — | — | speech-gated, soft limiter |

`DspMode = Auto` (default) picks the native core when its library loads and the managed chain otherwise;
`Off` disables the stage. Settings are inspector fields (`HighPass`, `EchoCancellation`, `EchoTailMs`,
`NoiseSuppression`, `Agc`, `AgcTargetDbfs`, `AgcMaxGainDb`) and can be changed live:

```csharp
voice.NoiseSuppression = NoiseSuppression.Moderate;
voice.EchoTailMs = 300;                       // open speakers in a big room
voice.ApplyDspSettings();                     // re-read the fields; switching DspMode swaps the processor

var dsp = voice.Dsp;                          // null when Off / not connected
bool realAec = dsp != null && dsp.SupportsEchoCancellation;   // false on the managed chain
var s = dsp.Stats;                            // ErleDb, EchoDelayMs, EchoConverged, FarEndActive,
                                              // SpeechProbability, AgcGainDb, FarEndUnderruns
```

The echo canceller's reference is the remote mix this behaviour plays through its `AudioSource`
(`OnAudioFilterRead` feeds it automatically, at 48 kHz before the output resampler). If you play voice through
your own audio graph — or want game music/SFX cancelled too — call `voice.Dsp.PushRender(pcm, offset, count,
channels)` from that graph's filter callback with the 48 kHz interleaved speaker signal in playout order; the
delay estimator absorbs up to 500 ms of buffering. Unity's own `Microphone` has no AEC, so on desktop the
native chain is what stops players hearing themselves through open speakers.

The managed chain never pretends: `ManagedCaptureDsp.Settings` reports `EchoCancellation = false` and
`NoiseSuppression = Off` whatever you asked for, so a UI can grey those toggles out. Own pipeline:
`CaptureDsp.Create(mode, DspSettings)` → `Process(mono48k, frames)` (whole 480-sample blocks; a 20 ms frame is
two) plus `PushRender` from the speaker side. Values outside the native ranges are clamped (`DspSettings.Clamped()`).

### Echo channel (mic test) & audio injection

```csharp
// backend: POST /v1/channels {"name":"mic-test","config":{"channel_type":"echo"}}
voice.ChannelId = echoChannelId; await voice.Connect();      // you hear only yourself, nobody hears you

voice.InjectClip(testClip, loop: true, gain: 0.8f);           // mixed over the microphone
voice.InjectClip(botLine, mixWithMicrophone: false);          // replaces the microphone until it ends
voice.Injector.Ended += () => testButton.interactable = true; // clip finished or StopInjection()
voice.IsInjecting;  voice.StopInjection();                    // microphone is audible again immediately

// live PCM (TTS, in-game radio) with your own pipeline:
var inj = new AudioInjector { Gain = 1f, MixWithMicrophone = true };
inj.OpenStream();                                             // stays active (silence when starved) until Stop()
inj.Push(pcm, channels, sampleRate);                          // any layout/rate, converted to mono 48 kHz
inj.Fill(micFrame, 960);                                      // after ApplyGain, before the VAD and Opus
```

An `echo` channel loops each participant's own frames back through the real uplink → server → downlink path
(encrypted, authenticated, with the receiver's own local volume/mute applied) and never forwards them to anybody
else or to other nodes. `AudioInjector` is plain C# (no Unity API): `Play` takes a decoded clip, `OpenStream` /
`Push` a live feed (queue capped at `MaxQueuedSamples`, oldest samples dropped), `Fill` sums the injected signal
into a 48 kHz mono microphone frame — or clears the frame first when `MixWithMicrophone` is `false` — and
hard-clips at ±1. The behaviour calls it after the input gain and before the VAD, so `GateOnVad`, `SetMuted`,
the transmission mode and focus treat injected audio exactly like speech; with no microphone (denied, none
attached) the behaviour still pumps injection-only 20 ms frames. `Play`/`OpenStream` replace whatever is playing;
drive the injector from the thread that produces the microphone frames (it is not thread-safe).

### Transcripts & text-to-speech

```csharp
// backend: channel config {"transcription": true} + [stt] configured on the server
voice.Client.OnTranscript += t => captions.Show(t.UserId, t.Text, t.StartedAt, t.DurationMs, t.Words);
voice.Client.IsChannelTranscribed(channelId);        // from ChannelJoinAck.transcription
voice.Client.IsChannelMonitored(channelId);          // ChannelJoinAck.safety_voice: speech is analysed by the [safety] classifier — disclose it
await voice.Client.SetTranscriptsAsync(false);       // stop receiving captions (replayed after reconnect)
voice.Client.TranscriptsEnabled;                     // true by default

// [tts] configured on the server (GET /v1/tts/voices lists voices and limits)
var req = await voice.Client.SpeakAsync("Enemy spotted at B", channelId, TtsDestination.Channel, voice: "nova");
voice.Client.OnTtsStatus += s => Debug.Log($"{s.ClientRef} {s.State} {s.DurationMs} {s.Message}");
var final = await req.Done;                          // Finished | Cancelled | Failed
await voice.Client.SpeakAsync("Reading your message…", channelId, TtsDestination.Local); // only you hear it
await voice.Client.CancelSpeechAsync();              // drops everything still queued or playing

AurixVoiceClient.IsSynthesizedSsrc(frame.Ssrc);      // TTS voice vs. microphone; FindBySsrc resolves both
```

Transcripts arrive only for channels the operator marked `transcription: true`, only from participants you
would hear (local mute, block and zero gain suppress their captions), never for end-to-end-encrypted audio, and
are not stored by the server — the event is your only copy. `SpeakAsync` completes once the server queued the
request and faults with `CODE: message` on refusal (`FEATURE_DISABLED`, `AUTH_DENIED` not a member, `USER_MUTED`,
`VALIDATION_ERROR` too long / unknown voice / control characters / ambiguous channel, `RATE_LIMIT_EXCEEDED`
queue or per-minute budget, `MESSAGE_BLOCKED` by the content filter); `channelId` may be null when the session
transmits to exactly one channel. Synthesized speech is routed exactly like your microphone (transmission mode,
mutes, blocks, focus, other nodes) and arrives on the participant's SSRC with the top bit set (`SynthSsrcFlag`),
so the `RemoteMixer` gives it its own jitter buffer and `FindBySsrc` still returns the speaker; channel
announcements (`POST /v1/channels/:id/tts`) use a per-channel synthetic SSRC with no participant behind it.

### Live translation

```csharp
// server: [translation] configured; Session.Translation says what the node offers (null = none)
var t = voice.Client.Session?.Translation;
if (t != null && (t.Languages.Count == 0 || t.Languages.Contains("de")))
    await voice.Client.SetTranslationAsync("de", spokenLanguage: "en", speech: t.Speech);
voice.Client.OnTranslationChanged += p => Debug.Log($"{p.Language} {p.SpokenLanguage} {p.Speech}");
voice.Client.OnTranscript += t =>
    captions.Show(t.UserId, t.Text, t.Language, t.Translated ? $"({t.OriginalLanguage}: {t.OriginalText})" : null);
voice.Client.TranslationPrefs;                        // as applied by the server (normalised tags)
await voice.Client.SetTranslationAsync(null);         // originals only
```

`SetTranslationAsync` translates the captions *you* receive; the speaker and listeners of other languages keep
theirs. A segment already in your language, or one the provider could not translate in time / that exceeds the
node's length limit, arrives as the original (`Translated == false`). With `speech: true` the translation is also
spoken to you alone on the channel's translator SSRC (a synthetic SSRC with no participant behind it — `FindBySsrc`
returns null, `IsSynthesizedSsrc` is true). Tags are BCP-47 (`DE_de` → `de-de`); the task faults with
`VALIDATION_ERROR` for a language the node does not offer and `TRANSLATION_DISABLED` when translation is off. The
preference is replayed after reconnect and failover, and the WebGL client exposes the same members.
Statuses go to the requesting session only; disconnecting cancels pending requests.

### Voice effects (robot, monster, radio, …)

```csharp
// inspector: AurixVoiceBehaviour.VoiceEffect = Robot | Monster | Radio | Helium | Ghost | Custom (+ CustomVoiceEffect)
// or from code, at any time, connected or not:
await voice.Client.SetVoiceEffectsAsync(VoiceEffectPreset.Radio);
var p = VoiceEffectParams.Preset(VoiceEffectPreset.Monster);
p.PitchSemitones = -10f; p.ReverbMix = 0.3f;              // tweak a preset …
await voice.Client.SetVoiceEffectsAsync(p);                // … (clamped to the library's limits)
await voice.Client.SetVoiceEffectsAsync(VoiceEffectParams.Bypass);
voice.Client.SupportsVoiceEffects;                         // native library present (false = plain microphone)
voice.Client.VoiceEffects.IsBypass;
```

`VoiceEffectParams` is the same parameter set as the native core and the Web SDK — `HighpassHz` /
`LowpassHz`, `FormantSemitones` (±12), `PitchSemitones` (±24), `RingModHz` (≤ 2 kHz), `DistortionDrive`
(≤ 20), `TremoloHz` / `TremoloDepth`, `StaticLevel`, `ReverbMix` / `ReverbSize` / `ReverbDamping`
(`0` = stage off; `Sanitized()` clamps). The chain runs on the **capture path only**, after the
capture DSP and `InputGain` and before VAD, lip-sync analysis, injection mixing and the encoder — so the
server, the other players, the level meters and any transcript get the effected voice, while
`InjectClip` / `Injector` audio and everything you hear are untouched. Mono and stereo uplinks are
both processed (channels are independent). On native players the stages are the native core's
(`aurix_voice_effects_*` in the `aurix_client` library that also carries `NativeOpusCodec` /
`NativeCaptureDsp`); without the library `SupportsVoiceEffects` is `false`, `SetVoiceEffectsAsync`
of anything but bypass faults with `PlatformNotSupportedException`, and `AurixVoiceBehaviour` logs one
warning and sends the plain microphone. In WebGL the same presets and parameters run in a browser
`AudioWorklet` inside the Web SDK (`AurixWebGLVoiceBehaviour.VoiceEffect`, `SupportsVoiceEffects`
= worklets available). The setting is client-side state: it survives reconnects and failover, its
delay lines are cleared whenever the microphone stops (`ResetLocalVoice()` — device switch, mute,
disconnect), and `Dispose` frees it.

### Lip-sync (visemes)

```csharp
// on the avatar: AurixLipSync (Target = face SkinnedMeshRenderer, BlendShapes per viseme, JawBlendShape)
var lips = avatar.GetComponent<AurixLipSync>();
lips.Bind(participant.UserId);                             // or lips.BindLocal() for your own avatar
lips.OnFrame += f => myRig.SetMouth(f.Dominant, f.MouthOpen);   // custom rigs / sprite sheets

// or raw, per render frame:
await voice.Client.SetVisemesAsync(true);                  // client-wide switch (AurixLipSync does this itself)
var f = voice.Client.GetParticipantVisemes(userId);        // VisemeFrame? — null with analysis off / unknown user
var mine = voice.Client.GetLocalVisemes();
// f.Weights[(int)Viseme.aa], f.Dominant, f.MouthOpen (0..1), f.Energy, f.Confidence, f.Sequence
```

`VisemeFrame` holds a weight per `Viseme` (`sil PP FF SS aa E ih oh ou` — `VisemeFrame.Names` in the same
order; `AurixLipSync.BlendShapes` defaults to the common `viseme_PP` … `viseme_U` blend-shape names, `""` =
not driven), the dominant bucket, the
mouth openness from the level, energy, confidence and a `Sequence` that advances per analysed 20 ms frame
(`AurixLipSync.IsAnalysing` turns off when frames stop arriving; the component eases to a closed mouth after
0.1 s without audio and smooths with `Smoothing`). The analysis runs **on this device** over audio it
plays anyway: every heard participant's frames are analysed right after decoding (and, in E2EE channels,
decrypting) — before the per-participant volume, local mute, panning and positional attenuation, so a
quiet or far-away speaker still moves their mouth — and your own microphone is analysed as sent (after
DSP, gain and effects, without injected clips). Nothing about the mouth shapes is sent anywhere. It is a
spectral heuristic (level, voiced/fricative split, two formants → nearest vowel), good for openness and
vowel motion, not for text-accurate articulation. Analysers are dropped when a participant leaves, on
`SetVisemesAsync(false)`, reconnect and disconnect. Native players need the `aurix_client` library
(`SupportsVisemes`); WebGL uses the Web SDK's worklet and — like the browser — only gets frames for
participants on a dedicated per-participant track, not for voices heard through the mix.

### Priority speakers and ducking game audio

```csharp
// channel: PUT /v1/channels/{id}/config {"ducking": {"gain": 0.25, "attack_ms": 60, "release_ms": 400, "hold_ms": 250, "moderators": false}}
// token grant: channels[].priority = true (raid leader) — or promote at runtime as a moderator:
await voice.Client.SetPriorityAsync(raidId, leaderUserId, true);
await voice.Client.SetPriorityAsync(raidId, null, false);   // a granted member toggles themselves
voice.Client.IsPriority(raidId);                            // me
participant.IsPriority;                                     // roster flag
voice.Client.GetChannelInfo(raidId)?.Ducking;               // DuckingConfig? (null = no ducking)
voice.Client.OnParticipantPriorityChanged += (ch, user, priority) => { … };
voice.Client.OnDuckingChanged += (ch, active, cfg) => { … }; // another priority speaker starts / stops holding the duck
voice.Client.IsDuckingActive(raidId);
```

While a priority speaker talks the **node** attenuates every other voice you receive with the channel's
envelope (attack → `Gain` → hold → release), multiplied with your own participant volumes and after your
local mutes / blocks — nothing to do on the client, and your own priority voice is never ducked. To make
the game's music and SFX follow, add `AurixGameAudioDucker` next to the voice behaviour and give it an
`AudioMixer` exposed parameter (`MixerParameter`, in dB relative to its value when ducking starts) or a set
of `AudioSource`s (`ChannelId` to follow one channel only); it tracks `OnDuckingChanged` across channels
(several priority speakers keep it engaged),
runs the same `DuckingEnvelope` on the main thread (`OverrideEnvelope` for your own curve), exposes
`CurrentGain` / `IsDucked` / `IsActive` / `OnGainChanged` for custom targets and releases on leave, kick and
disconnect. `SetPriorityAsync` for someone else needs a moderator role; promoting yourself needs a
`priority` grant (`AUTH_DENIED` otherwise), and a channel without `ducking` rejects it with
`VALIDATION_ERROR`. WebGL: identical members; the browser applies the envelope itself to its
per-participant tracks from the priority members' speaking state.

## .NET: build, test, end-to-end demo

```bash
cd sdk/unity/DotNet
dotnet build                     # library (netstandard2.1) + tests + demo + Unity compile check (Runtime/Unity + Samples~ against UnityEngine stubs), warnings as errors
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

`--scenario echo` (needs the API key: it creates an echo channel) checks loopback over real UDP: alice and bob
join the same echo channel, alice injects a stereo 24 kHz sine through `AudioInjector` → Opus → AURX and hears
only her own SSRC back (RMS ≈ 0.35, `Ended` fired once), nothing after `SetMuted(true)`, bob receives 0 frames.

`--scenario pcmu` negotiates alice onto PCMU while bob stays on Opus: alice's μ-law tone reaches bob as Opus
(RMS ≈ 0.35 for a 0.5 tone), bob's Opus reaches alice flagged `Pcmu` and decodes to RMS ≈ 0.21 for a 0.3
tone, alice switches back to Opus and the frames follow, `OnAudioCodecChanged` fires `Pcmu, Opus`, 0 auth
failures. `--scenario reconnect` also negotiates PCMU first and checks the codec survives a resume and is
re-negotiated after a fresh session.

`--scenario tunnel` puts alice on `TunnelOnly` and bob on `Auto`/UDP: the node advertises `media_tunnel`,
alice's binary-frame uplink reaches bob over UDP and bob's UDP uplink reaches alice over the tunnel (both
RMS ≈ 0.21), tunnel heartbeats are acked, a forced reconnect resumes the session still tunnelled with the
sequence continuing, 0 auth/replay failures. `--udp-block 1` (needs passwordless `sudo iptables`) adds carol
on `Auto` behind a UDP black hole scoped to her port: fallback at bind, audio through the tunnel, return to
UDP on the re-probe once the rule is lifted, fallback again on heartbeat loss when it comes back.

`--scenario failover` (needs two nodes: `--ws ws://node1/ws --ws-b ws://node2/ws`) cuts alice's control
connection through a local proxy that never comes back: the node advertised failover endpoints, alice's
reconnect rotates to node 2 within ~1 s (`OnRecovering` → `OnEndpointChanged` → `OnRecovered` with
`Migrated == true`), session id and SSRC are unchanged, `Endpoint` moved, she is still joined with her
codec preference, bob on node 2 hears her before and after the move and never sees a `ParticipantLeft`.

## Notes

* Threading: network I/O runs on thread-pool tasks; all events fire from `Update()` on the caller's thread.
  `JoinChannelAsync` completes even if `Update()` is not being pumped yet.
* Audio format: 48 kHz, 20 ms frames (`AudioFormat`). Mono uplink; the mixer outputs to any channel count.
* Capture DSP with echo cancellation and neural noise suppression needs the native core in `Plugins/`; without
  it the managed chain (high-pass + AGC) is used and `Dsp.SupportsEchoCancellation` is `false`.
* WebGL players use `AurixWebGLVoiceClient` (browser WebRTC via the Web SDK), not the native AURX/UDP path
  — see [Unity WebGL](#unity-webgl-browser-webrtc-through-the-web-sdk). IL2CPP works (no reflection, no dynamic code).
