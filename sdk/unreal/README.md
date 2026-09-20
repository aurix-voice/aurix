# Aurix Voice plugin for Unreal Engine

`AurixVoice` is a runtime plugin (UE 5.3+, Win64 / Linux / Mac) that wraps the native
[`aurix-client`](../../crates/aurix-client/README.md) library through its stable C ABI. The
plugin contains **no** protocol, crypto, networking or codec code of its own: AURX v2 over UDP,
the WebSocket control plane, Opus, jitter buffering, VAD, directional mixing and
reconnect/resume all live in the Rust library. The Unreal side is a `UGameInstanceSubsystem`
that pumps native events into Blueprint delegates on the game thread, feeds the microphone
(engine `AudioCapture`) into the client and plays the remote mix through a procedural
`USoundWave`.

```
sdk/unreal/
├── AurixVoice/
│   ├── AurixVoice.uplugin
│   └── Source/
│       ├── AurixVoice/                       runtime module
│       │   ├── Public/AurixVoiceTypes.h      Blueprint enums/structs mirroring the C ABI
│       │   ├── Public/AurixVoiceSubsystem.h  UAurixVoiceSubsystem: the whole API + events
│       │   ├── Public/AurixVoiceSoundWave.h  procedural 48 kHz stereo playback wave
│       │   └── Private/AurixAudioCapture.*   microphone → aurix_client_push_capture_f32
│       └── ThirdParty/AurixClientLibrary/    external module: headers + prebuilt library
│           ├── include/                      staged by the scripts (git-ignored)
│           └── lib/{Win64,Linux,Mac}/        staged by the scripts (git-ignored)
└── scripts/build_native.sh | .ps1            build aurix-client and stage it into the plugin
```

## 1. Build and stage the native library

Requires a Rust toolchain (1.88+) and a C compiler for the bundled Opus (MSVC on Windows,
clang/gcc elsewhere). Opus is linked **statically**, so the packaged game ships a single
`aurix_client` library and needs no system `libopus`.

```bash
# Linux / macOS host → stages lib/Linux or lib/Mac + include/
sdk/unreal/scripts/build_native.sh

# cross-target (needs the matching Rust target + linker installed)
sdk/unreal/scripts/build_native.sh --target aarch64-apple-darwin

# stage into a copy of the plugin living inside your game project
sdk/unreal/scripts/build_native.sh /path/to/MyGame/Plugins/AurixVoice
```

```powershell
# Windows (x64 Native Tools prompt or any shell with MSVC on PATH) → lib/Win64 + include/
sdk\unreal\scripts\build_native.ps1
sdk\unreal\scripts\build_native.ps1 -PluginDir C:\MyGame\Plugins\AurixVoice
```

The scripts fail loudly if an expected artifact is missing, and the `AurixClientLibrary`
module throws a `BuildException` naming the missing header/library at UBT time, so a plugin
without staged binaries never fails silently at runtime. The library is copied next to the
game binaries via `RuntimeDependencies` (Windows delay-loads the DLL; Linux/macOS use the
SONAME/`@rpath` install name the scripts set).

## 2. Install the plugin

Copy (or symlink) `sdk/unreal/AurixVoice` to `<Project>/Plugins/AurixVoice`, regenerate project
files and build. The plugin enables the engine's `AudioCapture` plugin as a dependency.
`CanContainContent` is false — everything is C++/Blueprint-callable, there are no assets.

## 3. Security model (identical to the Unity/Web SDKs)

* API keys never ship inside a build. Your game backend calls `POST /v1/tokens` (or
  `POST /v1/tokens/action` for one-time login/join tokens) and hands the per-user JWT to the
  client at runtime. `FAurixVoiceSettings::Token` is deliberately *not* an editable property, so
  it cannot be baked into a Blueprint asset.
* The WebSocket carries the token as a subprotocol; `SessionInitAck` gives the client a
  session-unique media key from which the AURX auth/encryption keys are derived. UDP media is
  bound with a signed `SessionBind` and every packet is AES-256-CTR sealed + HMAC authenticated;
  rejected packets show up as `FAurixStats::BadAuth` / `Replayed`.
* A dropped connection is resumed within the server's grace window with the same session,
  SSRC and key (`OnRecovering` → `OnRecovered(bResumed=true)`); after the window a fresh
  session is opened and channels are rejoined (`bResumed=false`, `OnRejoinFailed` per channel
  that needs a new join token). When the node is gone, attempts rotate through the failover
  nodes it advertised (`GetFailoverEndpoints`); the node that answers takes the session over —
  `OnEndpointChanged(Url)`, then `OnRecovered(bResumed=true, bMigrated=true)` with the same
  session/SSRC and a new media key and endpoint; `GetEndpoint` names the node in use.

## 4. Usage

### Blueprint

1. `Get Game Instance Subsystem → Aurix Voice Subsystem`.
2. Make `Aurix Voice Settings` (WebSocket URL from your config, Token from your backend call),
   leave `Auto Start Capture` / `Auto Start Playback` on, call **Connect**.
3. Bind `On Session Ready`, then **Join Channel** with the channel id (`Parse Uuid`) and the
   join token (empty unless the server runs `require_action_tokens`).
4. Bind `On Channel Joined`, `On Participant Joined/Left`, `On Participant Speaking`,
   `On Channel Energy` for the roster UI; `On Chat Message` / `Send Chat` for party text;
   `On Recovering` / `On Recovered` / `On Failed To Recover` for connection UI.
5. For a positional channel call **Update Own Position** every tick (or on movement) with the
   listener's location and rotation; `WorldToMeters` converts Unreal units (default 100 uu/m).
6. Optional, before **Connect**: **Discover Regions** with `Aurix Region Discovery Request`
   (Api Url, Token, optional Preferred Region / coordinates) and, in the completion delegate,
   take `Regions[0].WsUrl` as the WebSocket URL — see *Choosing a region* below.

### C++

```cpp
#include "AurixVoiceSubsystem.h"

void AMyPlayerController::StartVoice(const FString& Jwt)
{
	UAurixVoiceSubsystem* Voice = GetGameInstance()->GetSubsystem<UAurixVoiceSubsystem>();
	Voice->OnSessionReady.AddDynamic(this, &AMyPlayerController::OnVoiceReady);
	Voice->OnParticipantSpeaking.AddDynamic(this, &AMyPlayerController::OnSpeaking);
	Voice->OnFailedToRecover.AddDynamic(this, &AMyPlayerController::OnVoiceLost);

	FAurixVoiceSettings Settings;
	Settings.WebSocketUrl = TEXT("wss://voice.example.com/ws");
	Settings.Token = Jwt;                 // minted by your backend, never stored in assets
	Settings.bVadGate = true;             // send only detected speech
	Voice->Connect(Settings);
}

void AMyPlayerController::OnVoiceReady(const FAurixSessionInfo& Session)
{
	FGuid Team;
	UAurixVoiceSubsystem::ParseUuid(TeamChannelId, Team);
	int64 RequestId = 0;
	GetGameInstance()->GetSubsystem<UAurixVoiceSubsystem>()->JoinChannel(Team, TEXT(""), RequestId);
}

void AMyPlayerController::OnSpeaking(FGuid ChannelId, FGuid UserId, bool bSpeaking)
{
	// drive the speaking indicator next to the player's name
}

void AMyPlayerController::Tick(float DeltaSeconds)
{
	Super::Tick(DeltaSeconds);
	if (APawn* P = GetPawn())
	{
		GetGameInstance()->GetSubsystem<UAurixVoiceSubsystem>()
			->UpdateOwnPosition(ProximityChannel, P->GetActorLocation(), P->GetActorRotation());
	}
}
```

### Audio integration options

* **Default:** `bAutoStartCapture` opens the microphone through `Audio::FAudioCapture`
  (device from `GetCaptureDevices()` or the system default, hardware AEC requested) and
  `bAutoStartPlayback` spawns a 2D `UAudioComponent` playing a `UAurixVoiceSoundWave`
  (48 kHz stereo, directional panning already applied by the native mixer). Assign
  `PlaybackSoundClass` to route it through your game's mixer/ducking.
* **Custom capture:** leave `bAutoStartCapture` off and call `PushCaptureAudio` from your own
  capture path (any sample rate/channel count; the native side resamples to 48 kHz mono).
* **Custom playback:** leave `bAutoStartPlayback` off and call `MixOutputAudio` from your own
  procedural sound / submix; the native mixer adds the remote voices into your float buffer.

Both native entry points are audio-thread safe and stay valid until `Disconnect()`, which
stops capture, unbinds the sound wave and only then destroys the client.

* **Capture processing (DSP):** `FAurixVoiceSettings.Dsp` (`FAurixDspSettings`) configures the
  core's microphone chain — 80 Hz high-pass, acoustic echo cancellation (`EchoTailMs` 40–500,
  `StreamDelayMs`), RNNoise-derived noise suppression (`Off/Low/Moderate/High`) and a
  speech-gated AGC (`AgcTargetDbfs`, `AgcMaxGainDb`) — everything on by default. At runtime
  `SetDspSettings` / `GetDspSettings`; `GetDspStats` (`FAurixDspStats`: ERLE, estimated delay,
  converged, far-end active, speech probability, AGC gain, far-end underruns) for an overlay.
  The canceller's reference is whatever the core renders (the plugin's sound wave or your
  `MixOutputAudio` call); when the game plays other audio through the same speakers, pass it to
  `PushRenderAudio(InterleavedPcm, Channels)` (48 kHz float, 1–2 channels, playout order) from
  your submix so it is cancelled too. `bEchoCancellation = false` if you prefer the capture
  device's hardware AEC.

* **PCMU fallback:** `SetAudioCodec(EAurixAudioCodec::Pcmu)` negotiates G.711 μ-law for this
  session (8 kHz, no Opus CPU; the node transcodes at the edge, other participants keep Opus).
  `OnAudioCodecChanged` / `GetAudioCodec()` report what the server acknowledged; capture and
  playback switch codec inside the native core, nothing changes in the bridge. Refused with
  `CODEC_NOT_AVAILABLE` when the node runs `media.pcmu_fallback = false`.

* **Stereo uplink:** `FAurixEncoderSettings.bStereo` encodes the first two capture channels as
  L/R (a mono device is duplicated) for music / broadcast sources; honoured only while the joined
  channels' policy has `FAurixAudioPolicy.bStereo` (`ChannelConfig.stereo`), otherwise the core
  falls back to mono — `GetEncoderSettings().bStereo` shows the effective value. The capture DSP
  is bypassed for stereo frames; PCMU is always mono. Playback needs nothing: the mixer switches
  to a stereo decoder on the first stereo packet and downmixes before panning directional
  senders.

* **Blocked UDP:** `FAurixVoiceSettings.MediaPath` (`EAurixMediaPathPolicy::Auto` by default)
  binds media over UDP and falls back to the authenticated control WebSocket when the bind gets
  no answer or `UdpFallbackLostHeartbeats` heartbeats vanish mid-call; while tunnelled UDP is
  re-probed every `UdpReprobeIntervalMs` and taken back when it answers. `UdpOnly` /
  `TunnelOnly` pin a link. `GetMediaPath()`, `OnMediaPathChanged(Path, Reason)`,
  `FAurixSessionInfo.bMediaTunnel` (node advertises the tunnel) and `FAurixStats.MediaPath` /
  `HeartbeatsLostConsecutive` / `UplinkDropped`. The tunnel is TCP — expect latency bursts under
  loss; it keeps the player in the call, UDP remains the path to be on.

### Statistics and network quality bars

`GetStats(FAurixStats&)` returns the native snapshot (packets/bytes both ways, `BadAuth`,
`Replayed`, `HeartbeatsLost`, `FramesLost`/`FramesLate`/`Underruns`, RTT last/min/avg/max,
`JitterMs`, `LossPercent` over the last period, `RFactor`, `Mos`, `Bars` 1–5) and
`GetNetworkQuality(FAurixNetworkQuality&)` the last server-side rating (same fields plus the
uplink loss/jitter/bitrate the SFU measured); `OnNetworkQuality` fires when the server's bars
change. Bars use the same thresholds on every SDK and the server, so a HUD can show either.

### Choosing a region

Session resume is node-local, so a player should connect to the nearest node with capacity and
keep its direct URL. Either use the `endpoint.ws_url` your backend receives from
`POST /v1/tokens`, or let the client measure:

```cpp
FAurixRegionDiscoveryRequest Req;
Req.ApiUrl = TEXT("https://voice.example.com");   // any node / shared API hostname
Req.Token = Jwt;                                  // player JWT → GET /v1/me/regions
Req.PreferredRegion = PartyLeaderRegion;          // optional: ranks first when reachable
Req.bHasLocation = true; Req.Latitude = 48.9; Req.Longitude = 2.3;   // optional
// Req.bProbe = true (default): GET each region's probe URL ProbeSamples times, best RTT wins

FAurixRegionsDiscovered OnDone;
OnDone.BindDynamic(this, &AMyPlayerController::OnRegions);   // OnRegions must be a UFUNCTION()
Voice->DiscoverRegions(Req, OnDone);

void AMyPlayerController::OnRegions(bool bSuccess, const TArray<FAurixRegionEndpoint>& Regions, const FString& Error)
{
	if (!bSuccess || Regions.IsEmpty()) { /* fall back to the configured URL */ return; }
	Settings.WebSocketUrl = Regions[0].WsUrl;     // Regions[0].Region, RttMs, Nodes, LoadFactor for UI
	Voice->Connect(Settings);
}
```

Discovery uses the engine `HTTP` module (the plugin depends on it); parsing and ranking are done
by the native core, so the policy matches the Web and Unity SDKs: preferred region unless its
probe failed → RTT in `RttToleranceMs` buckets (ties keep the server's distance/load order) →
unprobed → `bProbeFailed`. `CancelRegionDiscovery()` aborts an in-flight discovery; starting a new
one cancels the previous; the subsystem cancels on shutdown. Only healthy nodes with a public
`wss://` URL are returned; an empty array means no node is configured for discovery.

### Events

Typed delegates cover connection state, session, media binding, channel/participant roster,
speaking/energy, transmission/focus, blocks, recording notices (`OnRecording` →
`RespondRecordingConsent`), bitrate adaptation, kicks, moderation acks, chat/typing,
transcripts, TTS status, request/server errors and the reconnect lifecycle. `OnRawEvent`
delivers every event as JSON for anything not typed (remote position updates, future fields).

`IsChannelTranscribed` / `IsChannelMonitored` (from `ChannelJoinAck`) tell whether a joined
channel is captioned or analysed by the server's content-safety classifier — show the latter
to the player where your policy requires a disclosure. `GetChannelScope` returns the presence /
text range of a positional channel (`RosterRadius` / `TextRadius`, 0 = whole channel): with a
roster radius, `OnParticipantJoined` / `OnParticipantLeft` also fire when someone walks into or
out of range, so a nameplate list driven by those events shows only nearby players.

All events are dispatched on the game thread from the subsystem's tick (up to 256 events per
tick; backlog is drained across ticks, nothing is dropped).

## 5. Verification status

What has been verified in this repository:

* `aurix-client` builds as a shared library for Linux with statically linked Opus via
  `build_native.sh` (`readelf -d` shows no `libopus` dependency and SONAME
  `libaurix_client.so`), headers and library land in the expected ThirdParty layout.
* `cargo test -p aurix-client --test c_abi` includes `unreal_plugin_uses_only_existing_abi`,
  which parses the `.uplugin`, checks the module/Build.cs layout and verifies that every
  `aurix_*` function, `AURIX_*` constant and `aurix::Client` / `aurix::Regions` method the
  plugin sources call is declared in the committed headers — ABI drift breaks CI, not the game
  build.

What has **not** been run here, because no Unreal Engine installation is available in the
development environment: Unreal Header Tool and the actual module compile on UE 5.3+, the
`AudioCaptureCore` stream callback signature (`Audio::FOnAudioCaptureFunction`,
`OpenAudioCaptureStream`) and `USoundWaveProcedural::GeneratePCMData` semantics against a live
engine, the `HTTP` module request/response API used by `AurixRegionDiscovery.cpp`, packaging on
Windows/macOS. Treat the first build in your project as a required
verification step; the plugin sources are small and any mismatch surfaces as a compile error
in one of the three bridge files (`AurixAudioCapture.cpp`, `AurixVoiceSoundWave.cpp`,
`AurixRegionDiscovery.cpp`).
