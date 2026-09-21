# Aurix Voice for Unreal — Blueprint quick start

Party voice in a lobby and proximity voice in the world, without writing C++. Everything below
uses the `AurixVoiceSamples` module (components + a function library) on top of the
`AurixVoice` runtime module; both ship in this plugin. C++ users can read
`Source/AurixVoiceSamples/Private/*.cpp` as the reference wiring of `UAurixVoiceSubsystem`.

## 0. Before the editor

1. Stage the native library once per platform (from the Aurix repository):
   `sdk/unreal/scripts/build_native.sh` (Linux/macOS) or `sdk/unreal/scripts/build_native.ps1`
   (Windows). It builds `aurix-client` with cargo and copies the header + library into
   `Source/ThirdParty/AurixClientLibrary/`. A plugin without them fails at build time with a
   message naming the missing file — never silently at runtime.
2. Copy the `AurixVoice` folder to `<Project>/Plugins/AurixVoice`, open the project, accept the
   rebuild, enable **Aurix Voice** under *Edit → Plugins → Audio* if it is not already on.
3. Your game backend must be able to mint per-user session tokens: `POST /v1/tokens` with the
   app's API key (one-time join/login tokens: `POST /v1/tokens/action`). **API keys never ship
   in a build** — the token is a runtime value in every Blueprint node below, deliberately not an
   editable property.

## 1. Lobby / party voice

1. Open your **PlayerController** Blueprint (or a GameInstance-owned actor that outlives level
   changes) and *Add Component → Aurix Voice Lobby*.
2. In the component's details set **Web Socket Url** (`wss://voice.example.com/ws`; the best
   region from *Discover Regions* if you use several) and, for a fixed lobby, **Channel Id**
   (UUID text). Leave it empty when channels come from your backend. Tick **Push To Talk** if
   you want it; otherwise the mic is open and gated by VAD.
3. Fetch the token from your backend (HTTP node of your choice) and call **Connect With Token**
   on the component. When the session is ready the component joins **Channel Id** by itself;
   otherwise call **Join Channel** (`Parse Uuid` → `Channel`, `Join Token` empty unless the app
   has `require_action_tokens`).
4. Bind the component's events in your lobby widget:
   * **On Roster Changed** (`Roster` = array of *Aurix Lobby Entry*: `User Id`, `Display Name`,
     `Self`, `Speaking`, `Muted`, `Server Muted`, `Locally Muted`, `Priority`, `Energy` 0..1,
     `Volume`) → rebuild the participant list; `Speaking` / `Energy` drive the talk indicator.
   * **On Status Changed** (`State`, `In Channel`, `Microphone Muted`, `Media Path`,
     `Endpoint`, `Quality Bars` 1..5, `Mos`, `Rtt Ms`) → status line; helpers
     *Connection State To Text*, *Media Path To Text*, *Quality Bars To Text*, *Format Mos* from
     the **Aurix Voice Blueprint Library** turn them into strings.
   * **On Chat Line** (`Sender Name`, `Text`, `Direct`) → chat log; **Send Chat** posts to the
     joined channel.
   * **On Error** (`Code`, `Message`) → toast. `KICKED` and `DISCONNECTED` also clear the
     roster; request codes are the server's (`CHANNEL_FULL`, `REJOIN_FAILED`, …).
5. Input: **Set Push To Talk Pressed** (true on key pressed, false on released) or **Toggle
   Microphone Muted**. Per-participant context menu: **Set Participant Muted Locally**,
   **Set Participant Volume Locally** (0..2) — listener-side, the other player is not told.
6. **Disconnect** when leaving the party. The component unbinds itself on *End Play*; the
   subsystem (and the voice session) lives with the game instance, so a level change does not
   drop the call.

Reconnects, session resume and failover to another node are automatic; the roster is rebuilt
from the *On Channel Joined* snapshot after a fresh session, and kept as-is after a resume.

## 2. Proximity voice in the world

The channel must be positional (`ChannelConfig.positional` on the server: roll-off, max distance,
optional radius visibility). Blueprint side:

1. Add **Aurix Proximity Voice** to the locally controlled **Pawn** (or the PlayerController).
   Optionally point **Pose Source** at the camera component and set **Voice Attenuation** to a
   `SoundAttenuation` asset for attached voices.
2. After the lobby component's **On Status Changed** reports `In Channel` (or your own
   *On Channel Joined*), call **Set Channel** with the positional channel id. The component now
   sends the pawn's location/rotation `Updates Per Second` times a second when it moved ≥
   `Min Move Cm` or turned ≥ `Min Turn Degrees` (`World To Meters` converts cm → m). The node
   applies distance roll-off and, with radius visibility, hides players out of range.
3. Two ways to hear others:
   * **2D mix (default):** nothing else to do; the node's positional gain and stereo panning by
     your heading are in the mix.
   * **Engine spatialization:** when a remote avatar spawns, call **Attach Participant Voice**
     (`User Id`, `Attach To` = the avatar's head / mesh component). The voice becomes an
     `AudioComponent` on that actor with your attenuation, occlusion, reverb and spatializer
     plugin, and leaves the 2D mix. Call **Detach Participant Voice** when the avatar is destroyed
     (the component detaches everything on *End Play*). Attached talkers are mono; unattached
     ones stay in the 2D mix.

Getting the remote avatar for a `User Id` is your game's mapping (PlayerState → voice user id).
A common pattern: put the voice user id in a replicated `PlayerState` property and resolve it
in the avatar's *Begin Play*.

## 3. Beyond the samples

Everything else is on **Aurix Voice Subsystem** (*Get Game Instance Subsystem* or the library's
**Get Aurix Voice**): multiple channels with **Set Transmission** / **Set Channel Focus**,
moderation with action tokens, per-participant sound waves (**Create Participant Sound**),
capture DSP and Opus settings (`Settings` on the lobby component exposes the same
*Aurix Voice Settings* struct), transcripts / translation / TTS, group E2EE fingerprints,
priority speakers and ducking, voice effects, visemes, statistics and raw events. The reference
is `sdk/unreal/README.md` and the "Unreal plugin" chapter of the docs site.

## Verification note

The plugin sources are checked in CI for ABI drift against the committed C headers and for
Marketplace/Fab packaging conventions; `RunUAT BuildPlugin` runs only on runners with access to
the Unreal Engine container images (see `.github/workflows/ci.yml`, job `unreal`). Treat the
first build inside your project as a required step and report compiler errors as issues.
