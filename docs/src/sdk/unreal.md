# Unreal Engine SDK (Blueprint and C++)

`AurixVoice` is a runtime plugin for **Unreal Engine 5.3 – 5.8** (Win64, Linux x86_64, macOS, Android).
It wraps the native Aurix core (`aurix_client`, Rust with a C ABI, statically linked Opus) in a
`UGameInstanceSubsystem`, feeds the engine microphone into the core, plays the remote mix through a
procedural 48 kHz sound wave and raises Blueprint delegates on the game thread. The optional
`AurixVoiceSamples` module adds Blueprint-ready components (**Aurix Voice Lobby**, **Aurix Proximity
Voice**) and the **Aurix Voice Blueprint Library** so a party or proximity voice chat needs no C++.

The plugin contains no protocol, crypto or codec code of its own — everything the Web, Unity and
Godot SDKs do (QUIC with 0-RTT resume, WebSocket/TLS tunnels, FEC/DRED, E2EE, capture DSP,
reconnect and cross-node failover) is in the core and documented in
[Native core](native.md). This page covers only what is Unreal-specific.

## 1. Install

| How | When | Steps |
| --- | --- | --- |
| **Fab / prebuilt zip** | You just want voice in a project | Download `AurixVoice-unreal-<version>.zip` from the [release page](https://github.com/aurix-voice/aurix/releases) (plugin sources plus the native libraries for every platform), unzip into `<Project>/Plugins/AurixVoice`, open the project and accept the rebuild. The same plugin is submitted to Fab per engine version. |
| **From the repository** | You build the core yourself or want the latest commit | Clone `aurix-voice/aurix`, run `sdk/unreal/scripts/build_native.sh` (Linux/macOS) or `build_native.ps1` (Windows) — this builds `aurix-client` with cargo and stages header + library into `Source/ThirdParty/AurixClientLibrary/` — then copy `sdk/unreal/AurixVoice` to `<Project>/Plugins/AurixVoice`. |

Enable **Aurix Voice** under *Edit → Plugins → Audio* if it is not already enabled. The plugin
declares `AudioCapture` and `EnhancedInput` as dependencies and turns them on.

A plugin without the native library **fails at build time** with a message naming the missing
file (`AurixClientLibrary.Build.cs`), never silently at runtime; the library is copied next to the
game binaries through `RuntimeDependencies`, so packaged builds need nothing else.

### Platforms

| Platform | Library | Notes |
| --- | --- | --- |
| Win64 | `lib/Win64/aurix_client.dll` + `.lib` | MSVC build; delay-loaded, so the editor starts even if the DLL is missing and fails with a clear message on first use. |
| Linux x86_64 | `lib/Linux/libaurix_client.so` | Verified with `RunUAT BuildPlugin -Rocket` on Epic's Linux images, UE 5.3 – 5.8. |
| macOS | `lib/Mac/libaurix_client.dylib` | Universal (arm64 + x86_64) in releases. |
| Android | `lib/Android/<abi>/libaurix_client.so` | arm64-v8a, armeabi-v7a, x86_64, API 21+; `AurixClientLibrary_APL.xml` packs them and requests `RECORD_AUDIO`. |

## 2. Security model (same as every Aurix SDK)

Your **game backend** mints a short-lived per-user token with the app's API key
(`POST /v1/tokens`; one-time join tokens: `POST /v1/tokens/action`). The token is a runtime value
on every node below and deliberately *not* an editable property, so an API key can never end up in
a packaged build. See [Tenancy, credentials and permissions](../concepts/auth.md).

## 3. Blueprint quick start

### 3.1 Lobby / party voice (`Aurix Voice Lobby` component)

1. Open your **PlayerController** (or a GameInstance-owned actor that survives level changes) →
   *Add Component → Aurix Voice Lobby*.
2. Details: **Web Socket Url** (`wss://voice.example.com/ws`, or the best region from
   **Discover Regions**), optional **Channel Id** (UUID text; leave empty when channels come from
   your backend), **Push To Talk** (otherwise open mic gated by VAD).
3. Fetch the token from your backend (any HTTP node) → **Connect With Token**. When the session is
   ready the component joins **Channel Id** by itself; otherwise call **Join Channel**
   (**Parse Uuid** → `Channel`; `Join Token` empty unless the app has `require_action_tokens`).
4. Bind the component's events in your lobby widget:

| Event | Payload | Use |
| --- | --- | --- |
| **On Roster Changed** | array of *Aurix Lobby Entry*: `User Id`, `Display Name`, `Self`, `Speaking`, `Muted`, `Server Muted`, `Locally Muted`, `Priority`, `Energy` 0..1, `Volume` | rebuild the participant list; `Speaking` / `Energy` drive the talk indicator |
| **On Status Changed** | `State`, `In Channel`, `Microphone Muted`, `Media Path`, `Endpoint`, `Quality Bars` 1..5, `Mos`, `Rtt Ms` | status line — **Connection State To Text**, **Media Path To Text**, **Quality Bars To Text**, **Format Mos** make strings |
| **On Chat Line** | `Sender Name`, `Text`, `Direct` | chat log; **Send Chat** posts to the joined channel |
| **On Error** | `Code`, `Message` | toast; `KICKED` / `DISCONNECTED` also clear the roster, request codes are the server's (`CHANNEL_FULL`, `REJOIN_FAILED`, …) |

5. Input: **Set Push To Talk Pressed** (true on pressed / false on released) or
   **Toggle Microphone Muted**. Per-participant context menu: **Set Participant Muted Locally**,
   **Set Participant Volume Locally** (0..2) — listener-side only, the other player is not told.
6. **Disconnect** when leaving the party. The component unbinds on *End Play*; the subsystem (and
   the voice session) lives with the game instance, so a level change does not drop the call.

Reconnects, session resume and failover to another node are automatic; the roster is rebuilt from
the *On Channel Joined* snapshot after a fresh session and kept as-is after a resume.

### 3.2 Proximity voice (`Aurix Proximity Voice` component)

The channel must be positional (`ChannelConfig.positional` on the server: roll-off, max distance,
optional radius visibility — see [Channels and audio routing](../features/channels.md)).

1. Add **Aurix Proximity Voice** to the locally controlled **Pawn** (or the PlayerController).
   Optionally point **Pose Source** at the camera and set **Voice Attenuation** to a
   `SoundAttenuation` asset.
2. After `In Channel` (or your own *On Channel Joined*) call **Set Channel** with the positional
   channel id. The component sends location/rotation **Updates Per Second** times a second when the
   pawn moved ≥ **Min Move Cm** or turned ≥ **Min Turn Degrees** (`World To Meters` converts
   cm → m); the node applies roll-off and, with radius visibility, hides out-of-range players.
3. Hearing others, two ways:
   * **2D mix (default)** — nothing else to do; positional gain and stereo panning by your heading
     are already in the mix.
   * **Engine spatialization** — when a remote avatar spawns, call **Attach Participant Voice**
     (`User Id`, `Attach To` = the avatar's head/mesh component). The voice becomes an
     `AudioComponent` on that actor with your attenuation, occlusion, reverb and spatializer plugin
     and leaves the 2D mix. **Detach Participant Voice** when the avatar is destroyed (everything is
     detached on *End Play*). Attached talkers are mono.

Mapping `User Id` → avatar is your game's job; the usual pattern is a replicated `PlayerState`
property with the voice user id, resolved in the avatar's *Begin Play*.

### 3.3 Push-to-talk and the microphone meter

Push-to-talk is a state machine inside **Aurix Voice Subsystem**, so it works the same with the
lobby component or your own wiring:

1. Create an *Input Action* (bool), e.g. `IA_PushToTalk`, and bind it in your *Input Mapping
   Context* (key, gamepad button, touch region).
2. **Set Push To Talk Enabled** (`Enabled` true, `Toggle` false for hold / true for press-to-toggle,
   `Release Delay Seconds` 0.15 keeps the last syllable), then **Set Push To Talk Action**
   (`IA_PushToTalk`, `Local Player Index` 0). The subsystem polls the action every tick. Without an
   action: **Set Push To Talk Pressed** (true/false) from any event.
3. UI: **On Push To Talk Changed** (`Transmitting`) lights the talk icon; **Get Input Level**
   (0..1, independent of mute) or **Get Input Energy** (RMS) drives a Progress Bar.
4. Back to open mic: **Set Push To Talk Enabled** (false), then **Set Muted** (false) if the player
   should be heard right away.

### 3.4 Beyond the samples

Everything else is on **Aurix Voice Subsystem** (*Get Game Instance Subsystem* or the library's
**Get Aurix Voice**): several channels with **Set Transmission** / **Set Channel Focus**, moderation
with action tokens, per-participant sound waves (**Create Participant Sound**), capture DSP and Opus
settings (*Aurix Voice Settings*, also exposed as `Settings` on the lobby component), transcripts /
translation / TTS, group E2EE fingerprints, priority speakers and ducking, voice effects, visemes,
statistics and raw events. The node-by-node reference is `sdk/unreal/README.md` in the repository
and `Docs/QuickStart.md` inside the plugin.

### 3.5 FMOD Studio and Wwise

FMOD Studio for Unreal (and Wwise) bypass Unreal's audio device, so the plugin's 2D playback
component and `SpawnParticipantAudioComponent` are silent there. The subsystem's PCM boundary covers
this; all of it is BlueprintCallable except the DSP callbacks themselves, which FMOD/Wwise require in
native code:

| Need | `UAurixVoiceSubsystem` | Format |
|---|---|---|
| hear the team through the middleware | `StopPlayback()` (and clear `bAutoStartPlayback`), then `MixOutputAudio(Pcm, Channels)` from an FMOD DSP read callback / Wwise Audio Input | **adds** the mix into your buffer; 48 kHz interleaved float, 1–2 channels; returns active streams |
| one 3D voice per speaker | `SetParticipantClaimed(UserId, true)` + `PullParticipantAudio(UserId, Pcm, Channels)` from a programmer-sound PCM callback | overwrites the buffer with that participant; 48 kHz; the mix skips claimed speakers |
| middleware owns the microphone | `StopCapture()` then `PushCaptureAudio(Pcm, SampleRate, Channels)` | any rate / channel count; cut into 20 ms Opus frames by the core |
| echo cancellation | `PushRenderAudio(Pcm, Channels)` with the game mix the middleware rendered | 48 kHz; audio obtained from `MixOutputAudio` is referenced automatically — do not push it again |

Run FMOD at 48 kHz (*Project Settings → FMOD Studio → Sample Rate*); the plugin has no resampler on
this boundary. `Samples/FMOD/AurixFmodVoiceComponent.{h,cpp}` in the plugin folder is a complete
drop-in component for a game module depending on `AurixVoice` and `FMODStudio`: voice DSP on the
master group or a bus, AEC reference tap upstream of the voice DSP, optional capture through
`System::recordStart`, and `BindParticipantToEvent(UserId, UFMODAudioComponent*)` which streams one
participant into an event with a Programmer instrument via `UFMODAudioComponent::SetProgrammerSound`.
It is not compiled by this repository's CI (no FMOD licence there) — review it against your FMOD
version (written for the 2.02/2.03 integration) before shipping.

## 4. C++

```cpp
UAurixVoiceSubsystem* Voice = GetGameInstance()->GetSubsystem<UAurixVoiceSubsystem>();
Voice->OnSessionReady.AddDynamic(this, &AMyPC::OnVoiceReady);
Voice->OnParticipantSpeaking.AddDynamic(this, &AMyPC::OnSpeaking);

FAurixVoiceSettings Settings;
Settings.WebSocketUrl = TEXT("wss://voice.example.com/ws");
Settings.Token = Jwt;          // minted by your backend
Settings.bVadGate = true;
Voice->Connect(Settings);

// in OnVoiceReady:
int64 RequestId;
Voice->JoinChannel(ChannelId, /*JoinToken*/ FString(), RequestId);
Voice->StartCapture();   // -1 = default device; GetCaptureDevices() to pick one
// positional channels, from Tick (WorldToMeters = 100 by default):
Voice->UpdateOwnPosition(ChannelId, GetPawn()->GetActorLocation(), GetPawn()->GetActorRotation());
```

Audio options: the default engine capture + a 2D `UAudioComponent` (`PlaybackSoundClass` routes it
through your mixer/ducking); or `PushCaptureAudio` from your own capture path and `MixOutputAudio`
from your own procedural sound/submix — both audio-thread safe until `Disconnect()`. For engine
spatialization `CreateParticipantSound(UserId)` / `SpawnParticipantAudioComponent(UserId,
HeadComponent, Attenuation)` give each talker their own sound. Network quality (RTT, jitter,
loss, MOS, 1..5 bars) arrives through the statistics delegate and `GetDspStats()` reports the
capture DSP; `DiscoverRegions(Request, OnComplete)` probes the regions and picks the closest node.

## 5. Packaging and Fab

The plugin follows the code-plugin layout Epic's tooling expects (`Config/FilterPlugin.ini`,
`Resources/Icon128.png`, `Docs/`, docs/support URLs and `SupportedTargetPlatforms` in the
descriptor). `RunUAT BuildPlugin -Plugin=…/AurixVoice.uplugin -Package=… -TargetPlatforms=Linux
-Rocket` builds it the way an installed engine does; do **not** pass `-StrictIncludes` against an
installed Linux engine. Fab submissions are source zips per engine version (`EngineVersion` set in the descriptor) with
the prebuilt native libraries inside. Details and the
reviewer checklist: `sdk/unreal/AurixVoice/Docs/Packaging.md`.

## 6. Troubleshooting

| Symptom | Cause / fix |
| --- | --- |
| UBT error naming `aurix_client.h` or a library | The native library is not staged — unzip a release or run `build_native.*`. |
| Editor starts, first **Connect** logs `aurix_client.dll` not found | Win64 DLL missing or 32-bit; make sure `lib/Win64/aurix_client.dll` exists and is from the same release as the header. |
| `On Error` with `UNAUTHORIZED` | Token expired or minted for another app; tokens are short-lived — mint right before **Connect With Token**. |
| Voice works in PIE, silent in a packaged build | `RECORD_AUDIO` (Android) / microphone privacy prompt (macOS: `NSMicrophoneUsageDescription`) missing; check *Project Settings → Platforms*. |
| Everyone is at full volume in a positional channel | **Set Channel** was not called after the join, or the channel is not configured as positional on the server. |
| UDP blocked on the player's network | Nothing to do: the core falls back to the WebSocket tunnel and then the TLS tunnel on 443 automatically; **Media Path** shows which one. |

## 7. Verification status

Verified: the plugin builds warning-free with `RunUAT BuildPlugin -Rocket` on Epic's Linux images
for UE 5.3.2 (editor + runtime) and the runtime target on 5.5.4, 5.6.1, 5.7.4 and 5.8.4; CI checks
the sources against the committed C headers on every commit and packages the plugin on runners
with access to the Epic images. **Not** verified by us: packaging on Windows/macOS hosts and the
runtime behaviour of `AudioCaptureCore` / `USoundWaveProcedural` in a shipped game — treat the first
build in your project as a required step and report problems as issues.
