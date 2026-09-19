# Unity / .NET SDK (`com.aurix.voice`)

Native client in C#: AURX v2 over UDP for media, WebSocket for control, no WebRTC stack. Runs in
Unity 2021.3+ (netstandard2.1; every platform except WebGL, IL2CPP-safe — no reflection, no
`unsafe`) and in plain .NET for dedicated servers, bots and tests. Full API reference:
`sdk/unity/README.md`.

```
sdk/unity/
├── package.json                     UPM package
├── Runtime/
│   ├── AurixVoiceClient.cs          the client: connect → bind media → join → audio/events
│   ├── Protocol/                    AURX v2 codec (AES-256-CTR + HMAC, replay window), control JSON
│   ├── Transport/                   ControlChannel (ClientWebSocket), MediaTransport (UDP)
│   ├── Audio/                       IOpusCodec, JitterBuffer, RemoteMixer, VAD, AudioInjector, OutputResampler
│   └── Unity/AurixVoiceBehaviour.cs MonoBehaviour: microphone → Opus → uplink, downlink → AudioSource
├── Samples~/Concentus/              IOpusCodec on top of Concentus (pure C# Opus)
├── Samples~/VoiceQuickstart/        sample scene (see below)
└── DotNet/                          solution: library, xunit tests, Unity compile check, headless E2E demo
```

## Install

1. *Window ▸ Package Manager ▸ + ▸ Add package from disk…* → `sdk/unity/package.json` (or a git
   URL pointing at that folder).
2. On the package page import the **Concentus Opus codec** sample and put the
   [Concentus](https://www.nuget.org/packages/Concentus) 2.x `netstandard2.0` assembly into
   `Assets/Plugins/`. Any other `IOpusCodec` (libopus/UnityOpus binding) works too.
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
   **Connect**. Start a second instance (or `dotnet run --project sdk/unity/DotNet/Aurix.Demo`)
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
(`DotNet/Aurix.Voice.UnityCheck`, against `UnityEngine` stubs, warnings as errors), but Unity
Editor itself is not part of CI — import it into your project once before relying on it.

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

## Feature map

| Feature | API |
|---|---|
| Reconnect / resume | `AutoReconnect`, `Reconnect` policy, `ForceReconnect()`, `OnRecovering(attempt, delay, cause)`, `OnRecovered(SessionInfo)`, `OnFailedToRecover`, `OnSessionClosed` |
| Action tokens | `TokenRefresher`, `JoinTokenProvider`, `JoinChannelAsync(id, joinToken)`, `ModerateAsync(channel, user, ModerationAction, token, reason)` |
| Local mute / volume / block | `SetParticipantMutedAsync(user, muted, channel?)`, `SetParticipantVolumeAsync(user, 0..2)`, `SetUserBlockedAsync`, `OnReceiverPreferences`, `OnUserBlockChanged` |
| Multiple channels | `SetTransmissionAsync(TransmissionMode)`, `TransmitToChannelAsync`, `SetChannelFocusAsync(Guid?)`, `OnTransmissionChanged`, `OnChannelFocusChanged`, `TransmitOpusFrame` (one frame to every allowed channel) |
| Positional / directional | `UpdatePositionAsync(channel, selfUserId, Position3D, Orientation3D)`, `OnPositions`; `RemoteMixer` pans by the per-packet direction — keep the `AudioSource` 2D |
| Energy / VAD | `VoiceActivityDetector` (`Speaking`, `Level`), `GateOnVad`, `OnChannelEnergy`, `participant.Energy` |
| Devices | `InputDevices`, `SetInputDevice()`, `SetInputGain()`, `SetOutputVolume()`, `SetOutputMuted()`; `OutputResampler` for non-48 kHz mixers |
| Echo test / injection | echo channel + `InjectClip(clip, loop, gain, mixWithMicrophone)`, `Injector` (live PCM), `StopInjection()` |
| Chat lite | `SendMessageAsync`, `SendDirectMessageAsync`, `SetTypingAsync`, `OnChatMessage`, `OnParticipantTyping` |
| Transcripts / TTS | `OnTranscript`, `SetTranscriptsAsync`, `SpeakAsync(text, channel?, TtsDestination, voice, clientRef)` → `SpeechRequest`, `OnTtsStatus`, `CancelSpeechAsync`, `IsSynthesizedSsrc` |
| Recording consent | `OnRecording(RecordingNotice)`, `RespondToRecordingAsync(id, RecordingConsent)` |
| Stats / quality | `GetStats()` → `VoiceStats`, `OnStats`, `OnNetworkQuality`, `LastNetworkQuality`, `QualityReportInterval`, `OnBitrateCommand` |
| Mobile | runtime microphone permission (`PermissionState`, `OnMicrophonePermissionDenied`, `RetryMicrophonePermission()`), background/foreground handling, `ProbeConnection()`, reconnect on Wi-Fi ↔ cellular |

## .NET: build, test, demo

```bash
cd sdk/unity/DotNet
dotnet build          # library + tests + demo + Unity compile check (warnings as errors)
dotnet test           # packet layout, wire vectors shared with the Rust tests, seal/open, replay, JSON, jitter buffer, resampler
AURIX_API_KEY=aurx_... dotnet run --project Aurix.Demo -- --api http://127.0.0.1:8080 --ws ws://127.0.0.1:8081/ws
```

The demo connects two headless clients over real UDP and asserts audio, events and counters;
`--scenario reconnect | prefs | chat | transmission | positional | echo` cover the other
features against a live server (see the README for what each checks).

## iOS / Android notes

* Fill *Microphone Usage Description* on iOS (`NSMicrophoneUsageDescription`), enable *Prepare
  iOS for Recording*; talking in the background needs `StopMicrophoneInBackground = false` and the
  *Audio* background mode.
* Android gets `RECORD_AUDIO` automatically; add `unityplayer.SkipPermissionsDialog` to defer the
  prompt to the moment the behaviour needs the microphone. A denied permission leaves the client
  connected as a listener.
* Bluetooth/headset route changes alter the output rate; the behaviour re-reads
  `AudioSettings` and restarts the `AudioSource` automatically.
