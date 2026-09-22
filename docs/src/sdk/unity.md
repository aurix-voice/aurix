# Unity / .NET SDK (`com.aurix.voice`)

Native client in C#: AURX v2 over UDP for media, WebSocket for control, no WebRTC stack. Runs in
Unity 2021.3+ (netstandard2.1; IL2CPP-safe — no reflection, no `unsafe`; WebGL players use the
same API over the browser Web SDK, see [Unity WebGL](#unity-webgl)) and in plain .NET for dedicated servers, bots and tests. Full API reference:
`sdk/unity/README.md`.

```
sdk/unity/
├── package.json                     UPM package (com.aurix.voice); CHANGELOG.md, Documentation~/com.aurix.voice.md
├── Runtime/
│   ├── AurixVoiceClient.cs          the client: connect → bind media → join → audio/events
│   ├── Protocol/                    AURX v2 codec (AES-256-CTR + HMAC, replay window), control JSON
│   ├── Transport/                   ControlChannel (ClientWebSocket), MediaTransport (UDP)
│   ├── Audio/                       IOpusCodec, JitterBuffer, RemoteMixer, VAD, AudioInjector, OutputResampler
│   ├── WebGL/                       AurixWebGLVoiceClient: same API over the browser Web SDK (WebGL players)
│   ├── Plugins/WebGL/AurixWebGL.jslib Emscripten plugin bridging to window.AurixWebSdk
│   └── Unity/                       AurixVoiceBehaviour (microphone → Opus → uplink, downlink → AudioSource), AurixWebGLVoiceBehaviour
├── Editor/                          Aurix Voice menu: project setup check, copy Web SDK bundle to StreamingAssets, docs
├── Tests/Runtime/                   NUnit tests for the Unity Test Runner (wire format, E2EE vectors, WebGL bridge contract)
├── Samples~/Concentus/              IOpusCodec on top of Concentus (pure C# Opus)
├── Samples~/VoiceQuickstart/        sample scene (see below)
├── Samples~/WebGLQuickstart/        the same lobby for WebGL players + WebGL template shipping aurix-web-sdk.js
├── DotNet~/                         development-only solution: library, xunit tests, Unity compile check, headless E2E demo
└── BrowserTests~/                   Chromium test of the real .jslib + Web SDK bundle (Emscripten stand-in, live node optional)
```

## Install

1. *Window ▸ Package Manager ▸ + ▸ Add package from git URL…* →
   `https://github.com/aurix-voice/aurix.git?path=sdk/unity#<tag-or-commit>` (the same string works
   as a dependency in `Packages/manifest.json`; pin the tag/commit of the server you deploy), or
   *Add package from disk…* → `sdk/unity/package.json` of a checkout.
2. On the package page import the **Concentus Opus codec** sample and put the
   [Concentus](https://www.nuget.org/packages/Concentus) 2.x `netstandard2.0` assembly into
   `Assets/Plugins/` — or ship the native core and use `NativeOpusCodec` (libopus); see
   [Opus codec and controls](#opus-codec-and-controls). Any other `IOpusCodec` works too.
3. *Project Settings ▸ Audio ▸ System Sample Rate* = **48000** (other rates work through the
   built-in resampler, 48 kHz avoids it).

## The sample scene: Voice quick start

Import the **Voice quick start** sample from the same package page and open
`Assets/Samples/Aurix Voice SDK/<version>/Voice quick start/VoiceQuickstart.unity`. A single
`MonoBehaviour` (`VoiceQuickstart.cs`) drives the SDK's `AurixVoiceBehaviour` and draws an IMGUI
panel — no prefabs or extra packages, so it doubles as copy-paste reference code.

1. Run a server and create a channel with your API key (`POST /v1/channels`).
2. Mint a player token: `POST /v1/tokens` with `user_id`, `display_name`, `channels: [<id>]`. In a
   real game **your backend** does this — the API key must never be in a Unity build.
3. Press Play, fill in *WebSocket URL* (`ws://<host>:8081/ws`), token and channel id, press
   **Connect**. Start a second instance (or `dotnet run --project sdk/unity/DotNet~/Aurix.Demo`)
   with a token for another `user_id` to hear each other.

| Panel control | SDK call |
|---|---|
| Connect / Disconnect | `AurixVoiceBehaviour.Connect()` / `Disconnect()` |
| Reconnect | `Client.ForceReconnect()` — resumes the session, same SSRC |
| Mute microphone / push-to-talk (`PushToTalkKey`) | `SetMuted(bool)` |
| Mute speakers / volume | `SetOutputMuted(bool)` / `SetOutputVolume(float)` |
| Quality bars, R-factor, MOS, RTT, loss | `OnNetworkQuality`, `OnStats`, `Client.GetStats()` |
| Roster (speaking, energy, mutes) | `OnChannelJoined`, `OnParticipantJoined/Left/Updated`, `OnSpeaking`, `OnChannelEnergy` |
| Chat | `Client.SendMessageAsync(channelId, text)`, `OnChatMessage` |
| Log | `OnStateChanged`, `OnRecovering/OnRecovered/OnFailedToRecover`, `OnKicked`, `OnRecording`, `OnServerError` |

The sample is compiled in CI together with the Unity-only runtime code
(`DotNet~/Aurix.Voice.UnityCheck`, against `UnityEngine` stubs, warnings as errors), and the
`Tests/Runtime` NUnit tests compile the same way, but Unity Editor itself is not part of CI —
import the package into your project once before relying on it.

## The WebGL sample: WebGL quick start

The same lobby for browser players: `WebGLQuickstart.cs` drives `AurixWebGLVoiceBehaviour`, reads
`?ws=…&api=…&token=…&channel=…` from the page URL (so a backend can hand out a ready link), shows
the browser autoplay state with an **Enable audio** button (`ResumeAudioAsync`), per-participant
"HRTF" markers (`IsParticipantSpatialized`), WebRTC stats/MOS, chat and the event log. Its
`WebGLTemplates/Aurix/index.html` loads `aurix-web-sdk.js` *before* the Unity loader, reports a
missing bundle and an insecure origin on the page, and hides the loading overlay once
`createUnityInstance` resolves. The sample README covers bundle placement, CORS, `https://`,
TURN and compression. Verified here in Chromium with the real `.jslib` and bundle under an
Emscripten stand-in (`BrowserTests~/webgl_bridge_e2e.py`, in CI against a live node); a
Unity-built player was not.

## Minimal integration

```csharp
var voice = GetComponent<AurixVoiceBehaviour>();       // needs an AudioSource on the same object
voice.CodecFactory = () => new Aurix.Samples.ConcentusOpusCodec();
voice.WebSocketUrl = "wss://voice.example.com/ws";
voice.Token = jwtFromYourBackend;
voice.ChannelId = channelUuid;                          // comma-separated list for several channels
await voice.Connect();
voice.SetMuted(false);
```

Inspector fields cover microphone device, input gain (0–4), output volume/mute, bitrate, VAD
threshold/hang-over and `GateOnVad` (send only detected speech), and the mobile switches
(`RequestMicrophonePermission`, `StopMicrophoneInBackground`, `ProbeAfterBackgroundSeconds`,
`ReconnectOnNetworkChange`). `voice.Client` exposes the full `AurixVoiceClient`.

For a custom pipeline (own capture, per-participant spatialisation) drive the client directly:
`ConnectAsync()` → `JoinChannelAsync()`, `SendOpusFrame(channelHash, opus, len)` for every 20 ms
frame, `TryDequeueAudio(out IncomingAudio)` → `RemoteMixer.Push(...)` and `Mix(...)` inside
`OnAudioFilterRead`, and call `Client.Update()` every frame — all events are raised from
`Update()` on the calling thread.

## Region selection

Connect to the nearest node with capacity and keep its direct URL — a resume on the same node
moves nothing, a resume elsewhere is a
[takeover](../operations/high-availability.md#cross-node-session-failover). Either use the
`endpoint.ws_url` your backend receives from `POST /v1/tokens` (with
`region` / `location` hints), or measure from the device:

```csharp
using var http = new HttpClient();
var regions = await RegionDiscovery.DiscoverAsync(http, "https://voice.example.com", jwt,
    new RegionDiscoveryOptions
    {
        PreferredRegion = partyLeaderRegion,   // optional: ranks first when reachable
        Latitude = 48.9, Longitude = 2.3,      // optional: server orders by distance
        // Probe = true (default): GET each region's ProbeUrl, best RTT wins
    });
var best = regions.FirstOrDefault();           // null → no node advertised for discovery
voice.WebSocketUrl = best.WsUrl;
```

`DiscoverAsync` calls `GET /v1/me/regions` with the player JWT, then probes each `ProbeUrl`
(one warm-up discarded, `ProbeSamples` = 3 timed, minimum kept, `ProbeTimeout` = 2 s). Ranking:
preferred region unless its probe failed → RTT in `RttToleranceMs` (15) buckets, ties keeping
the server's distance/load order → unprobed regions → regions whose probe failed
(`ProbeFailed`). `RegionDiscovery.Rank` / `ProbeRttAsync` / `ParseResponse` are public for
custom policies; every `RegionEndpoint` carries `Nodes`, `LoadFactor`, `DistanceKm` and `RttMs`.
Server side: [Regions](../operations/scaling.md#regions).

## Feature map

| Feature | API |
|---|---|
| Reconnect / resume | `AutoReconnect`, `Reconnect` policy, `ForceReconnect()`, `OnRecovering(attempt, delay, cause)`, `OnRecovered(SessionInfo)`, `OnFailedToRecover`, `OnSessionClosed` |
| Failover | `Endpoint`, `FailoverEndpoints`, `OnEndpointChanged(url)`, `SessionInfo.Migrated`, `SessionInfo.Failover` — attempts rotate current node → advertised failover nodes; a takeover keeps session id/SSRC, new media key/endpoint |
| Action tokens | `TokenRefresher`, `JoinTokenProvider`, `JoinChannelAsync(id, joinToken)`, `ModerateAsync(channel, user, ModerationAction, token, reason)` |
| Local mute / volume / block | `SetParticipantMutedAsync(user, muted, channel?)`, `SetParticipantVolumeAsync(user, 0..2)`, `SetUserBlockedAsync`, `OnReceiverPreferences`, `OnUserBlockChanged` |
| Multiple channels | `SetTransmissionAsync(TransmissionMode)`, `TransmitToChannelAsync`, `SetChannelFocusAsync(Guid?)`, `OnTransmissionChanged`, `OnChannelFocusChanged`, `TransmitOpusFrame` (one frame to every allowed channel) |
| Positional / directional | `UpdatePositionAsync(channel, selfUserId, Position3D, Orientation3D)`, `OnPositions`; `RemoteMixer` pans by the per-packet direction — keep the `AudioSource` 2D |
| Per-participant playback (Unity spatialization) | behaviour `Playback` (`VoicePlaybackMode` `Mixed` / `PerParticipant` / `PerParticipantOnly`); `AurixParticipantAudioSource` on the avatar (`Bind(userId)`, `Unbind()`, `IsActive`, `OnActiveChanged`) plays one user unpanned through its own 3D `AudioSource` and claims the streams out of the aggregate mix; `AurixListenerTap` on the `AudioListener` feeds the final render to AEC; `RemoteMixer.Pull` / `PullParticipant` / `GetStreams` — see `sdk/unity/README.md` ("Per-participant playback") |
| Presence / text range | `GetChannelScope(channel)` → `ChannelScope? { RosterRadius, TextRadius }`; `OnParticipantJoined` / `OnParticipantLeft` also fire as players move in and out of the roster radius (`Participant.Role` / `IsMuted` filled from the event) — see [radius-scoped presence](../features/channels.md#radius-scoped-presence-and-text) |
| Energy / VAD | `VoiceActivityDetector` (`Speaking`, `Level`), `GateOnVad`, `OnChannelEnergy`, `participant.Energy` |
| Devices | `InputDevices`, `SetInputDevice()`, `SetInputGain()`, `SetOutputVolume()`, `SetOutputMuted()`; `OutputResampler` for non-48 kHz mixers |
| Capture DSP | `DspMode` (`Auto`/`Native`/`Managed`/`Off`), `HighPass`, `EchoCancellation`, `EchoTailMs`, `NoiseSuppression`, `Agc`, `AgcTargetDbfs`, `AgcMaxGainDb`, `ApplyDspSettings()`, `Dsp` (`ICaptureProcessor`: `Stats`, `PushRender`, `SupportsEchoCancellation`); `NativeCaptureDsp`, `ManagedCaptureDsp`, `CaptureDsp.Create` — [below](#capture-processing-echo-cancellation-noise-suppression-agc) |
| Echo test / injection | echo channel + `InjectClip(clip, loop, gain, mixWithMicrophone)`, `Injector` (live PCM), `StopInjection()` |
| Chat | `SendMessageAsync`, `SendDirectMessageAsync`, `SetTypingAsync`, `OnChatMessage` (`Offline`), `OnParticipantTyping`; stored chat: `HistoryAsync` / `DirectHistoryAsync` (cursor pages), `MarkReadAsync` / `MarkDirectReadAsync`, `ReadMarkersAsync` / `DirectReadMarkersAsync`, `OnChatReadMarker`, `OnChatInboxSynced` (`perDevice`), `DeviceId` for the [exactly-once per-device queue](../features/chat.md#exactly-once-per-device), `EditMessageAsync` / `DeleteMessageAsync`, `ReactAsync`, `SearchAsync` / `SearchDirectAsync`, `OnChatMessageUpdated`, `OnChatReactionChanged` |
| Transcripts / TTS | `OnTranscript`, `SetTranscriptsAsync`, `IsChannelMonitored`, `SpeakAsync(text, channel?, TtsDestination, voice, clientRef)` → `SpeechRequest`, `OnTtsStatus`, `CancelSpeechAsync`, `IsSynthesizedSsrc` |
| Live translation | `Session.Translation` (`TranslationInfo { Speech, Languages }`, null when the node does not translate), `SetTranslationAsync(language, spokenLanguage, speech)`, `TranslationPrefs`, `OnTranslationChanged`; `Transcript.Translated` / `OriginalText` / `OriginalLanguage` — replayed on reconnect, same API on WebGL; see [live translation](../features/speech.md#live-translation) |
| Voice effects | `SetVoiceEffectsAsync(VoiceEffectParams)` / `SetVoiceEffectsAsync(VoiceEffectPreset)`, `VoiceEffects`, `SupportsVoiceEffects`, `VoiceEffectParams.Preset / Bypass / Sanitized()`; behaviour `VoiceEffect` (`None / Robot / Monster / Radio / Helium / Ghost / Custom`) + `CustomVoiceEffect`, `ApplyVoiceSettings()` — microphone uplink only, after DSP/gain, before VAD/encode; native library on players, `AudioWorklet` on WebGL — see [voice effects](../features/speech.md#voice-effects) |
| Lip-sync (visemes) | `SetVisemesAsync(bool)`, `VisemesEnabled`, `SupportsVisemes`, `GetParticipantVisemes(user)` / `GetLocalVisemes()` → `VisemeFrame? { Weights[9], Dominant, MouthOpen, Energy, Confidence, Sequence }`; `AurixLipSync` on the avatar (`Bind(userId)` / `BindLocal()`, blend shapes or `OnFrame`, `Current`, `IsAnalysing`); behaviour `LipSync` — on-device analysis of decoded audio, nothing sent; WebGL: dedicated tracks only — see [lip-sync](../features/speech.md#visemes-lip-sync) |
| Priority speakers / ducking | `SetPriorityAsync(channel, user?, priority)`, `IsPriority(channel)`, `Participant.IsPriority`, `ChannelInfo.Ducking`, `IsDuckingActive(channel)`, `OnParticipantPriorityChanged(channel, user, priority)`, `OnDuckingChanged(channel, active, DuckingConfig)`; `AurixGameAudioDucker` (mixer parameter / `AudioSource`s, `DuckingEnvelope`, `CurrentGain`, `IsDucked`, `IsActive`, `OnGainChanged`) — voices are ducked by the node, game audio by the helper; see [priority speakers](../features/channels.md#priority-speakers-and-ducking) |
| Recording consent | `OnRecording(RecordingNotice)`, `RespondToRecordingAsync(id, RecordingConsent)` |
| Stats / quality | `GetStats()` → `VoiceStats`, `OnStats`, `OnNetworkQuality`, `LastNetworkQuality`, `QualityReportInterval`, `OnBitrateCommand(BitrateCommand)` |
| Opus controls | `OpusEncoderSettings`, `SetEncoderSettings`, `SetComplexity`, `FollowChannelPolicy`, `Encoder`, `EffectiveEncoderSettings`, `AudioPolicy`, `OnAudioPolicyChanged`, `OnEncoderSettingsChanged`; `NativeOpusCodec` / `ConcentusOpusCodec`, `IOpusEncoderControls`, `IOpusFecDecoder` |
| PCMU / PCMA (G.711) fallback | `SetAudioCodecAsync(AudioCodec)`, `AudioCodec` / `PreferredAudioCodec`, `OnAudioCodecChanged`; behaviour `PreferredCodec`, `SetAudioCodec()`, `ActiveCodec`; `G711Codec` (`PcmuCodec`), `G711`, `TransmitAudioFrame(codec, …)`, `IncomingAudio.Codec` |
| Server-side noise suppression | `SetServerNoiseSuppressionAsync(bool)`, `ServerNoiseSuppression`, `OnServerNoiseSuppressionChanged`, `SessionInfo.NoiseSuppression` — the node denoises this session's uplink (for PCMU devices or builds without the client DSP); `NOISE_SUPPRESSION_UNAVAILABLE` when the node has it off or full; kept across reconnects; never for E2EE / stereo — see [server-side noise suppression](../features/channels.md#server-side-noise-suppression) |
| Large channels | `GetChannelInfo(channel)` → `ChannelInfo? { Role, ParticipantCount, HiddenListeners, Transcription, SafetyVoice, CanSpeak }`, `CanSpeakIn(channel)`, `SetDownlinkModeAsync(DownlinkMode)`, `DownlinkMode` / `PreferredDownlinkMode`, `OnDownlinkModeChanged`, `SessionInfo.DownlinkMix`, `IncomingAudio.Mixed`; behaviour `PreferredDownlinkMode`, `SetDownlinkMode()`, `ActiveDownlinkMode`, `StereoCodecFactory` — [below](#large-channels-listeners-and-the-server-mix) |
| Blocked UDP | `MediaPathPolicy` (`Auto`/`UdpOnly`/`TunnelOnly`), `UdpFallbackLostHeartbeats`, `UdpReprobeInterval`, `MediaHeartbeatInterval`, `ActiveMediaPath`, `OnMediaPathChanged(path, reason)`, `SessionInfo.MediaTunnel`, `VoiceStats.MediaPath` / `HeartbeatsLostConsecutive` / `UplinkDropped`; behaviour `MediaPath`, `UdpFallbackLostHeartbeats`, `UdpReprobeIntervalSeconds` — [below](#when-udp-is-blocked-the-websocket-tunnel) |
| Mobile | runtime microphone permission (`PermissionState`, `OnMicrophonePermissionDenied`, `RetryMicrophonePermission()`), background/foreground handling, `ProbeConnection()`, reconnect on Wi-Fi ↔ cellular |
| Unity WebGL | `Aurix.WebGL.AurixWebGLVoiceClient` / `AurixWebGLVoiceBehaviour` — the same `IAurixVoiceClient` over the browser Web SDK (WebRTC); `SdkUrl`, `PreloadSdk()`, `ResumeAudioAsync()`, `OnRemoteAudio(playing, reason)`, `EnumerateDevicesAsync`, `SetInputDeviceAsync` / `SetOutputDeviceAsync`, `GetStatsAsync()` → `WebGLStats`, `OnEventsDropped`, `WebGLClientOptions` — [below](#unity-webgl) |

## Capture processing: echo cancellation, noise suppression, AGC

Microphone frames are cleaned after downmix/resampling and before `InputGain`, the injector,
VAD and the encoder — the same position as in the [native core](native.md#capture-dsp-echo-cancellation-noise-suppression-agc):

| `DspMode` | Implementation | High-pass | AEC | Noise suppression | AGC |
|---|---|---|---|---|---|
| `Native` | `NativeCaptureDsp` over `aurix_dsp_*` in the native core (`Plugins/`, same binary as `NativeOpusCodec`) | 80 Hz | frequency-domain, 40–500 ms tail, delay estimation | RNNoise-derived neural NS | speech-gated + soft limiter |
| `Managed` | `ManagedCaptureDsp`, pure C# (IL2CPP-safe, no native dependency) | 80 Hz | — | — | speech-gated + soft limiter |

`Auto` (default) takes the native chain when the library loads, the managed chain otherwise;
`Off` skips the stage. Inspector fields map 1:1 onto `DspSettings`; `ApplyDspSettings()` pushes
edits live and swaps the implementation when `DspMode` changed. `Dsp.Stats` (`DspStats`) shows
ERLE, estimated echo delay, convergence, far-end activity, speech probability and AGC gain for
an overlay.

The canceller's reference is the remote mix this behaviour plays (`OnAudioFilterRead` pushes the
48 kHz mixer output before the device-rate resampler). Voice through your own audio graph, or
music/SFX you also want cancelled, goes through `voice.Dsp.PushRender(pcm, offset, count,
channels)` from that graph's callback, in playout order. The managed chain reports what it does
not do (`SupportsEchoCancellation == false`, `Settings.EchoCancellation == false`) rather than
claiming an AEC it does not have. Own pipeline: `CaptureDsp.Create(mode, settings)` →
`Process(mono48k, frames)` in whole 480-sample blocks.

## Opus codec and controls

The SDK does not bundle Opus; two codecs are provided and the client is codec-agnostic:

* **`ConcentusOpusCodec`** (sample) — pure C# Concentus 2.x, no native binaries, runs everywhere
  Unity runs C#; roughly 5–10× the CPU of libopus, keep complexity ≤ 5 on mobile.
* **`NativeOpusCodec`** — libopus statically linked into the Aurix native core (`aurix_client`,
  the same library the Unreal plugin uses; `cargo build -p aurix-client --release` or
  `sdk/unreal/AurixVoice/build_native.*`). Put the binary where P/Invoke finds it
  (`Assets/Plugins/x86_64/aurix_client.dll`, `Assets/Plugins/Linux/x86_64/libaurix_client.so`,
  `Assets/Plugins/macOS/libaurix_client.dylib`, `Assets/Plugins/Android/<abi>/libaurix_client.so`;
  iOS links the static `libaurix_client.a` and resolves through `__Internal`). Nothing loads the
  library unless you construct the codec; `NativeOpusCodec.IsAvailable` probes once so a project
  can fall back to Concentus — the quick-start scene does exactly that.

Both expose every libopus encoder control through `OpusEncoderSettings` (`BitrateBps`
6 000..300 000 mono / ..510 000 stereo, `Complexity` 0..10, `MaxBandwidth`, `Signal`
Auto/Voice/Music, `Vbr`, `ConstrainedVbr`, `Fec`, `ExpectedLossPercent`, `Dtx`, `Channels`
1|2, `DredDurationMs` 0..1040) and recover lost frames from the next packet's FEC data
(`IOpusFecDecoder`; `RemoteMixer` uses it before falling back to PLC,
`VoiceStats.FramesFecRecovered`).

**Packet loss (libopus 1.6 in `NativeOpusCodec`).** The native codec also implements
`IOpusDredDecoder` (`DecodeDred(laterPacket, framesBefore, pcm, n)` rebuilds a frame from the
Deep REDundancy a later packet carries; `NativeOpusCodec.DredSupported`) and
`IOpusDecoderControls` (`OpusDecoderSettings { Complexity, OsceBwe }`: `>= 5` neural PLC,
`>= 6` OSCE; `RemoteMixer.DecoderSettings` applies it to every stream). `RemoteMixer` repairs a
gap in the order FEC (one frame) → DRED (bursts, as far as the packet's coverage reaches) → PLC
when the packet that ends it arrives, keeps reordered packets, drops ones for slots already
played (`VoiceStats.FramesLate`) and counts `FramesFecRecovered` / `FramesDredRecovered`;
Concentus has FEC and classic PLC only. The client's **loss profile** follows the server's
`NetworkQuality.ProtectLossPercent` (the higher of `UplinkLossPercent` and the worst receiver's
`ReceiversLossPercent`) — `LossProfile.Low / Moderate / High` at ≥ 3 % / ≥ 10 %
(FEC on, `ExpectedLossPercent` ≥ 10 / 20, DRED ≥ 400 ms and a 28 kbit/s floor in `High`),
relaxing after a 6 s dwell below 1 % / 5 % — `client.LossProfile`, `SetLossProfile(profile)`
to pin / `SetLossProfile(null)` for automatic, `OnLossProfileChanged(profile, lossPercent)`,
`VoiceStats.LossProfile`; same thresholds and shaping as the native core
([Packet loss](native.md#packet-loss-fec-dred-and-the-neural-plc)). Not available in WebGL
players (the browser owns its codec).

The encoder the client drives (`client.Encoder = codec`) runs the **baseline** you set
(`SetEncoderSettings`, the *Opus encoder* inspector block of `AurixVoiceBehaviour`) with the
**channel audio policy** layered on top (`ChannelJoinAck.audio`, live `ChannelAudioPolicy`;
merged across joined channels, `OnAudioPolicyChanged`, opt out with
`FollowChannelPolicy = false`, pin the CPU budget with `SetComplexity`), and the server's
transient **`BitrateCommand`** (clamped to the policy's floor/target, also raises
`ExpectedLossPercent`) on top of that — `EffectiveEncoderSettings` shows the result. Semantics
are identical in the native and Web SDKs; see [Channels](../features/channels.md#configuration)
and [Network quality](../features/quality.md).

**Stereo uplink.** `Channels = 2` (behaviour: *Stereo*) encodes the microphone's first two
channels as L/R (a mono device is duplicated) and needs a codec built for two channels —
`AurixVoiceBehaviour.StereoCodecFactory` (e.g. `() => new ConcentusOpusCodec(48000, 2)`),
the same factory the server mix uses; without it the behaviour logs a warning and stays mono.
It is honoured only in channels whose policy has `AudioPolicy.Stereo`
([Stereo and music uplinks](../features/channels.md#stereo-and-music-uplinks)); voice channels
force mono and PCMU is always mono. The capture DSP is bypassed for stereo frames (input gain,
VAD and energy run on the L/R average). Receiving needs nothing: `RemoteMixer` inspects each
Opus packet (`OpusPacket.IsStereo`), switches a stream to the stereo factory on its first stereo
packet (falling back to the mono factory, which downmixes, when none is set), keeps the image
for non-positional senders, downmixes before panning directional ones and averages L/R for a
mono output.

## PCMU / PCMA (G.711) fallback

For devices where even Concentus at low complexity is too expensive, a session can run on
**G.711** — μ-law (`AudioCodec.Pcmu`) or A-law (`AudioCodec.Pcma`) — instead of Opus (8 kHz,
64 kbit/s, telephone quality; a lookup table, no encoder state). It is negotiated per session
with the server, which transcodes at the edge in plaintext channels — everyone else in the
channel keeps Opus — and relays sealed G.711 frames untouched in E2EE channels
([codecs](../features/channels.md#codecs-opus-and-the-pcmu-fallback)).

```csharp
// AurixVoiceBehaviour: set PreferredCodec = AudioCodec.Pcmu in the inspector (or before Connect),
// or switch at runtime:
await voice.SetAudioCodec(AudioCodec.Pcmu);   // await voice.SetAudioCodec(AudioCodec.Opus) to go back
voice.ActiveCodec;                            // what the server acknowledged

// AurixVoiceClient with your own capture pipeline:
client.OnAudioCodecChanged += codec => encoder = codec.IsG711() ? new G711Codec(codec) : opus;
await client.SetAudioCodecAsync(AudioCodec.Pcma);
int n = encoder.Encode(pcm48k, AudioFormat.FrameSamples, alaw);   // G711Codec: 960 → 160 bytes
client.TransmitAudioFrame(client.AudioCodec, alaw, n, AudioFormat.FrameSamples, vad.Level);
```

The behaviour switches its encoder when `OnAudioCodecChanged` fires, so frames always match the
codec the server expects; `PreferredCodec` is re-negotiated automatically after a fresh session
(a resumed session keeps it). `G711Codec` (`PcmuCodec` is the μ-law alias) implements
`IOpusCodec`, so `RemoteMixer` decodes PCMU and PCMA downlink streams (`IncomingAudio.Codec`)
next to Opus ones — a stream that changes codec gets a fresh decoder. `SetBitrate` is a no-op on
it (G.711 is fixed-rate), and `OpusEncoderSettings` / channel audio policies do not apply while
G.711 is active. Rejected with `CODEC_NOT_AVAILABLE` when the node runs
`media.pcmu_fallback = false`.

## Large channels: listeners and the server mix

The join ack says what kind of member you are
([large channels](../features/channels.md#large-channels-and-audiences)):

```csharp
client.OnChannelJoined += (channel, participants) =>
{
    var info = client.GetChannelInfo(channel).Value;
    // participants = the visible roster; info.ParticipantCount = everyone, on every node,
    // including listeners hidden from the roster when info.HiddenListeners is true.
    if (!client.CanSpeakIn(channel))       // info.Role == ChannelRole.Listener
        pushToTalk.interactable = false;   // the node drops our frames anyway
};
```

A session may also ask for **one server-mixed stereo stream per channel** instead of one
stream per speaker — constant downlink bandwidth and decode cost however many people talk;
mutes, volumes, focus and positional gains are applied by the node before mixing:

```csharp
// AurixVoiceBehaviour: PreferredDownlinkMode = DownlinkMode.Mixed in the inspector, and
voice.StereoCodecFactory = () => new ConcentusOpusCodec(48000, 2);   // or NativeOpusCodec(48000, 2)
await voice.SetDownlinkMode(DownlinkMode.Mixed);                    // runtime switch; voice.ActiveDownlinkMode

// AurixVoiceClient + your own RemoteMixer:
var mixer = new RemoteMixer(() => new ConcentusOpusCodec(), () => new ConcentusOpusCodec(48000, 2));
await client.SetDownlinkModeAsync(DownlinkMode.Mixed);              // OnDownlinkModeChanged confirms
while (client.TryDequeueAudio(out var a)) mixer.Push(in a);         // a.Mixed → stereo decoder, no panning
```

Mixed frames carry `PacketFlags.Mixed` under the channel's synthetic SSRC; `RemoteMixer` decodes
them with the stereo factory (or the mono `CodecFactory`, downmixing, when none is set) and never
pans them, so positional channels sound the same in both modes. Speakers using E2EE still arrive
as separate streams. `PreferredDownlinkMode` is re-applied after a fresh session (a resumed one
keeps it); rejected with `VALIDATION_ERROR` when the node runs `media.downlink_mix = false`
(`SessionInfo.DownlinkMix`). Channels configured with `audience.mix_for_listeners` mix for
listeners regardless of the requested mode.

## End-to-end encryption

`AurixVoiceClient.E2ee` (default `true`) announces the capability on connect and lets the client
join [`e2ee` channels](../features/e2ee.md): every Opus uplink frame into such a channel is
sealed with the client's own sender key (`MediaTransport.SendAudioE2ee`, `PacketFlags.E2ee`),
peers' frames are opened by SSRC before they reach `RemoteMixer`, wrapped keys travel over the
control WebSocket, and rotation on join/leave/re-key is automatic. The crypto is managed C#
(X25519, HKDF, AES-CTR, HMAC — no native dependency) and shares test vectors with the Rust core
and the Web SDK.

```csharp
var client = new AurixVoiceClient(options) { E2ee = true };
if (PlayerPrefs.HasKey("aurix.e2ee")) client.SetE2eeIdentity(Convert.FromBase64String(PlayerPrefs.GetString("aurix.e2ee")));
Debug.Log($"my fingerprint {client.E2eeFingerprint}");
client.OnE2eePeerKey += (userId, fingerprint, previous) => { /* show it; previous != null → the peer re-keyed */ };
client.OnE2eePeerDecryptable += (userId, ok) => { /* their voice will (not) decode */ };
client.OnE2eeKeyRotated += generation => {};
client.IsChannelEncrypted(channelId); client.E2eePeerFingerprint(userId);
client.IsE2eePeerDecryptable(userId); client.GetE2eeDecryptablePeers();
client.RotateE2eeKey();                                  // manual; returns the new generation
var s = client.GetStats();                               // s.FramesE2ee, s.E2eeUndecryptable
PlayerPrefs.SetString("aurix.e2ee", Convert.ToBase64String(client.ExportE2eeIdentity()));
```

With `E2ee = false` the client never joins encrypted channels (`E2EE_REQUIRED`); it never sends
plaintext into one or plays plaintext coming out of one — such frames count as
`E2eeUndecryptable`. Encrypted speakers always arrive as separate streams, also in `Mixed`
downlink mode; a G.711 session seals its μ-law / A-law frames the same way and the node relays
them with their codec flag (`AurxPacket.CodecOf(flags)` after decryption picks the decoder).
`AurixWebGLVoiceClient` exposes the same surface (`E2ee` / `E2eeIdentity` / `E2eeTransform` /
`E2eeWorkerUrl` in `WebGLClientOptions`, `E2eeAvailable`, `E2eeTransformApi`,
`GetE2eeStatsAsync`, `RotateE2eeKeyAsync`) over the browser implementation — see
[Unity WebGL](#unity-webgl) and the Web SDK's [browser matrix](../features/e2ee.md#browser-support).

## When UDP is blocked: the WebSocket tunnel

Some networks drop UDP altogether. In `MediaPathPolicy.Auto` (default) the client binds media
over UDP first and, if the node never answers, carries the very same sealed AURX packets as
binary frames over the control WebSocket it already holds — no second connection, no new
credentials ([protocol](../api/aurx.md#tunnel-aurx-over-the-control-websocket)). Later, while
on UDP, `UdpFallbackLostHeartbeats` (3) unanswered heartbeats in a row switch to the tunnel
mid-call; while tunnelled, every `UdpReprobeInterval` (30 s, `TimeSpan.Zero` = never) a fresh
UDP bind is tried and the media moves back the moment it answers. `MediaHeartbeatInterval`
(5 s) sets how quickly a dead UDP path is noticed. `UdpOnly` restores the old behaviour (a dead
path is a reconnect), `TunnelOnly` never opens a UDP socket. `Auto` needs the node to advertise
the tunnel (`SessionInfo.MediaTunnel`, `media.media_tunnel = true`).

The C# transport does not speak the node's
[QUIC media path](../api/aurx.md#quic-aurx-datagrams-with-0-rtt-resume-and-connection-migration)
(0-RTT resume, connection migration); it ignores `SessionInitAck.quic` and stays on UDP/tunnel.
QUIC reaches Unity only where the native core does — through the C ABI (`NativeOpusCodec` is a
codec binding, not a transport).

```csharp
client.MediaPathPolicy = MediaPathPolicy.Auto;
client.UdpFallbackLostHeartbeats = 3;
client.UdpReprobeInterval = TimeSpan.FromSeconds(30);
client.OnMediaPathChanged += (path, why) => Debug.Log($"media over {path}: {why}");
// "Tunnel: UDP bind failed: …", "Tunnel: 3 UDP heartbeats unanswered", "Udp: UDP re-probe answered"
var stats = client.GetStats();   // stats.MediaPath, stats.HeartbeatsLostConsecutive, stats.UplinkDropped
```

Both links share one uplink sequence counter, so a switch is invisible to the server's replay
window and to other players' jitter buffers; codecs, mute, quality reports, reconnect and resume
behave the same on either link. `UplinkDropped` counts frames the tunnel's bounded send queue
(`ControlChannel.MediaQueueLength`, 64 frames) refused because the TCP connection was stalled —
audio never blocks the game thread. Expect higher and burstier latency on the tunnel (TCP
retransmits stall everything behind a lost segment): it keeps the player in the call, UDP
remains the path to be on. `MediaTransport.OverTunnel(IMediaTunnel, …)` exposes the link for
custom pipelines; `ControlChannel` implements `IMediaTunnel`.

## .NET: build, test, demo

```bash
cd sdk/unity/DotNet~
dotnet build          # library + tests + demo + Unity compile check (warnings as errors)
dotnet test           # packet layout, wire vectors shared with the Rust tests, seal/open, replay, JSON, jitter buffer, resampler
AURIX_API_KEY=aurx_... dotnet run --project Aurix.Demo -- --api http://127.0.0.1:8080 --ws ws://127.0.0.1:8081/ws
```

The demo connects two headless clients over real UDP and asserts audio, events and counters;
`--scenario reconnect | prefs | chat | transmission | positional | echo | pcmu | tunnel` cover
the other features against a live server (see the README for what each checks). `tunnel` adds
`--udp-block 1` to black-hole UDP with `sudo iptables` on the fly and watch an `Auto` client fall
back at bind, return on the re-probe and fall back again on heartbeat loss.

## iOS / Android notes

* Fill *Microphone Usage Description* on iOS (`NSMicrophoneUsageDescription`), enable *Prepare
  iOS for Recording*; talking in the background needs `StopMicrophoneInBackground = false` and the
  *Audio* background mode.
* Android gets `RECORD_AUDIO` automatically; add `unityplayer.SkipPermissionsDialog` to defer the
  prompt to the moment the behaviour needs the microphone. A denied permission leaves the client
  connected as a listener.
* Bluetooth/headset route changes alter the output rate; the behaviour re-reads
  `AudioSettings` and restarts the `AudioSource` automatically.

## Unity WebGL

WebGL players have no UDP, no `ClientWebSocket` and no threads, and the microphone and speakers
belong to the browser. `AurixVoiceClient` throws `PlatformNotSupportedException` there; instead the
package provides `Aurix.WebGL.AurixWebGLVoiceClient`, a second implementation of the same
`IAurixVoiceClient` interface that drives the browser-native [Web SDK](web.md) through a small
JavaScript bridge (`Runtime/Plugins/WebGL/AurixWebGL.jslib` ↔ `AurixWebSdk.AurixBridge`):
WebSocket control plane, **WebRTC** media, the browser's Opus/AEC/NS/AGC and audio output.

```
C#  AurixWebGLVoiceClient ──JSON──▶ AurixWebGL.jslib ──▶ window.AurixWebSdk.AurixBridge ──▶ AurixClient (Web SDK)
     ▲ Update() drains events  ◀──JSON──                ◀── ordered, bounded event queue
```

1. `cd sdk/web && npm ci && npm run build` → `sdk/web/dist/aurix-web-sdk.js`, a dependency-free
   script that defines `window.AurixWebSdk`. Put it in `Assets/StreamingAssets/` (the default
   `SdkUrl`, `StreamingAssets/aurix-web-sdk.js`, is relative to the player's `index.html`) or load it
   from your WebGL template with a `<script>` tag — an already-present SDK is reused.
2. The `.jslib` ships in the package and is linked into WebGL players only; the native
   transport/audio classes are compiled out of WebGL players, so no native library is involved.
3. Add `AurixWebGLVoiceBehaviour` (API URL, `wss://` URL, token, channel ids, browser microphone
   processing, gain/volume/mute, `UseTurn`) or construct `AurixWebGLVoiceClient` and call
   `Update()` every frame. `TokenRefresher` / `JoinTokenProvider` work as in the native client — the
   browser client asks C# for tokens through the bridge.
4. Same server-side requirements as the Web SDK: page origin in `AURIX__SERVER__CORS_ORIGINS`,
   `https://` for `getUserMedia`, `media.external_ip` or TURN reachable from the browser.

Differences from the native client, all inherent to the browser: remote voices play through the
browser — the server mix in a hidden `<audio>` element plus up to `WebGLClientOptions.ParticipantStreams`
[per-participant WebRTC tracks](../features/channels.md#per-participant-tracks-for-browsers)
(capped by the node's `webrtc_participant_streams`) that the Web SDK spatializes itself with Web
Audio HRTF (`SpatialAudio = Hrtf | EqualPower | None`) from the positions you send with
`UpdatePositionAsync` — so no `AudioSource`, mixer, spatializer plugin or per-participant PCM
(`AurixParticipantAudioSource` is native-only). `SetPinnedParticipantsAsync(ids)` keeps chosen
users on their own track, `OnParticipantStreams` / `GetParticipantStreamsAsync()` report the
`mid → UserId` layout (`Live` = a track is attached), `GetParticipantStreamCapAsync()` the node's
cap and `IsParticipantSpatialized(id)` whether a voice currently goes through the HRTF panner;
speakers beyond the tracks stay in the mix. Capture processing is the browser's, `IOpusCodec` / DSP /
`MediaPathPolicy` / PCMU / downlink-mix settings do not apply, and audio starts only after a user
gesture — `OnRemoteAudio(playing: false, reason)` reports the block and `ResumeAudioAsync()` from a UI
click retries both the `<audio>` element and the Web Audio graph. Results are tasks completed from `Update()` on the main thread; browser event queues
are bounded and report drops through `OnEventsDropped`. In the Editor and on other platforms the
jslib is not linked (`NativeWebGLBridge` throws) — use `AurixVoiceBehaviour` there or inject a test
`IWebGLBridge`. Verified here: the jslib and the sample's WebGL template against the real bundle
under an Emscripten stand-in (Node and Chromium, the latter also joining a live node in CI), the
C# client against a scripted bridge, the Unity compile check with `UNITY_WEBGL`; a Unity-built
WebGL player was not run in this repository. Details and the full option list:
`sdk/unity/README.md` ("Unity WebGL").
