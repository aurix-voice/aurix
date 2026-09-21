# Godot SDK (GDExtension)

`sdk/godot` is a Godot **4.3+** GDExtension over the [native core](native.md): the C++ classes
call `aurix_client.hpp` (the header-only wrapper of the stable C ABI) and translate between
Godot types and the C structs. No protocol, codec, crypto or networking code lives in the
extension — AURX v2 over QUIC/UDP/tunnel, the WebSocket control plane, Opus, jitter buffers, VAD, the
DSP chain, reconnect/resume and cross-node failover are the same Rust library that Unreal and
the C samples use. Full reference: `sdk/godot/README.md`.

```
 Godot scene tree (main thread)                     aurix-client (own threads)
 ┌──────────────────────────────┐   _process()      ┌──────────────────────────┐
 │ AurixVoiceClient : Node      │──── push PCM ────►│ capture → DSP → Opus     │──► QUIC / UDP / WS tunnel
 │   AudioStreamMicrophone      │◄─── mix_output ───│ jitter → decode → mixer  │◄── QUIC / UDP / WS tunnel
 │   → AurixCapture bus         │◄─── poll_event ───│ event queue              │◄── WebSocket (wss)
 │   AudioStreamGenerator out   │      → signals    └──────────────────────────┘
 │ AurixParticipantPlayer ×N    │◄── pull_participant (claimed users leave the mix)
 │   : AudioStreamPlayer3D      │
 │ AurixRegions : RefCounted    │      GET /v1/me/regions → RTT probe → rank → best_ws_url()
 └──────────────────────────────┘
```

## Classes

| Class | Base | Role |
|---|---|---|
| `AurixVoiceClient` | `Node` | one per player: `connect_to_server(ws_url, token)`, `join_channel`, capture/playback, receiver controls, chat, moderation, speech, stats; every native event is a signal emitted from `_process` |
| `AurixParticipantPlayer` | `AudioStreamPlayer3D` | one per remote speaker you want Godot to spatialise (attenuation, doppler, reverb buses); claims the user on the client and pulls that user's PCM |
| `AurixRegions` | `RefCounted` | region discovery response parser and ranker |

Results are the C ABI's `AurixResult` (`RESULT_OK`, `RESULT_NOT_CONNECTED`, …), states
`STATE_DISCONNECTED … STATE_FAILED`, media path `MEDIA_NONE | MEDIA_UDP | MEDIA_TUNNEL | MEDIA_QUIC`; all
enums from the header are bound as constants on `AurixVoiceClient`.

```gdscript
@onready var voice: AurixVoiceClient = $AurixVoiceClient

func _ready() -> void:
	voice.session_ready.connect(func(s): print("user ", s.user_id))
	voice.channel_joined.connect(func(_rid, channel_id, participants, info): print("joined ", channel_id))
	voice.participant_speaking.connect(func(_ch, user_id, speaking): _talking(user_id, speaking))
	voice.connect_to_server("wss://voice.example.com/ws", token_from_your_backend)

func join(channel_id: String) -> void:
	voice.join_channel(channel_id)
```

## Audio in Godot terms

* **Capture:** `auto_capture` (default) creates a muted `AurixCapture` bus with an
  `AudioEffectCapture`, plays an `AudioStreamMicrophone` into it and pushes the captured frames
  to the core every frame; needs `audio/driver/enable_input`. Or push your own PCM with
  `push_capture` / `push_capture_mono` at any sample rate.
* **Mixed playback:** `auto_playback` owns a stereo `AudioStreamPlayer` fed from `mix_output()`
  on `playback_bus`. Per-participant volume/mute, server gain, blocks and directional panning
  are applied by the core's mixer.
* **Engine spatialisation:** `playback_mode = PLAYBACK_PER_PARTICIPANT` (claimed speakers leave
  the mix) or `PLAYBACK_PER_PARTICIPANT_ONLY`, then one `AurixParticipantPlayer` per speaker
  under their avatar. Claims survive SSRC changes, reconnect, failover and native client
  recreation. See [Channels and audio routing](../features/channels.md) for the semantics shared
  with Unity/Unreal.
* **Server-side directional audio:** `update_transforms(channel_id, {user_id: Transform3D})`
  (Godot's `-Z` forward is converted).
* **AEC reference:** `push_render(PackedVector2Array)` when you mix game audio yourself.
* **Media link:** `media_path_policy` (`MEDIA_PATH_AUTO | UDP_ONLY | TUNNEL_ONLY | QUIC_ONLY`),
  the `quic` property (`Auto` tries QUIC before UDP when the node offers it), `get_media_path()`,
  the `media_path_changed(path, reason)` signal and `network_changed()` — call it from your
  platform's connectivity notification so a QUIC session migrates to the new address instead
  of timing out; `session_ready`'s dictionary carries `media_quic` / `media_tunnel`. Semantics in
  [QUIC](native.md#quic-0-rtt-resume-and-connection-migration) and
  [the tunnel](native.md#when-udp-is-blocked-the-websocket-tunnel).
* **Packet loss:** the core rebuilds lost frames from FEC / DRED and conceals the rest with
  libopus' neural PLC; `get_encoder_settings()["dred_duration_ms"]`,
  `set_decoder_settings({"complexity": 5, "osce_bwe": false})` / `get_decoder_settings()`,
  `set_loss_adaptation(LOSS_ADAPTATION_AUTO | LOSS_ADAPTATION_FIXED_LOW | …)`,
  `get_loss_adaptation()`, `get_loss_profile()` (`LOSS_PROFILE_LOW | MODERATE | HIGH`), the
  `loss_profile_changed(profile, uplink_loss_percent)` signal and the static
  `AurixVoiceClient.is_dred_supported()` — semantics in
  [Packet loss](native.md#packet-loss-fec-dred-and-the-neural-plc).
* **End-to-end encryption:** the core announces the capability and seals/opens frames of
  [`e2ee` channels](../features/e2ee.md) on its own, so a Godot client joins them like any other
  channel; the fingerprint / identity-persistence API and the `E2EE_*` events are not bound to
  GDScript yet (a fresh identity per session).

## Threading, memory, lifetime

The core runs its own tokio runtime (`worker_threads`, 1 by default); the extension touches it
only from the main thread — do not call the node from `AudioStreamGenerator` threads. Every
value crossing the ABI is copied into `String`/`Dictionary`/`Array`/packed arrays; nothing
retains a pointer into an `AurixEvent`. Freeing the node disconnects and destroys the client;
players release their claims in `_exit_tree`.

## Build, packaging, tests

```bash
git clone --depth 1 --branch godot-4.3-stable https://github.com/godotengine/godot-cpp sdk/godot/godot-cpp
sdk/godot/scripts/build_native.sh [--release] [--target <triple>]      # cargo → stage → scons
godot --headless --path sdk/godot --import
godot --headless --path sdk/godot -s tests/smoke.gd                    # 100+ API checks, no server
AURIX_E2E_API=… AURIX_E2E_WS=… AURIX_E2E_API_KEY=… sdk/godot/tests/live.sh   # two clients, real audio
```

`addons/aurix_voice/aurix_voice.gdextension` maps Linux x86_64/arm64, Windows x86_64 and
macOS universal, Android arm64/arm32/x86_64 and iOS arm64 to `bin/<platform>.<arch>/` and
declares `aurix_client` as a `[dependencies]` entry so export templates ship both libraries.
Web exports cannot load the extension (no UDP, no raw sockets in a browser); they use the
separate [`AurixWebVoiceClient`](#web-export-aurixwebvoiceclient) instead.
Consoles: [Porting to consoles](consoles.md). Flutter / React Native:
[Mobile app frameworks](mobile-frameworks.md).

`demo/main.tscn` is a lobby scene (connect, join, mute, chat, roster with speaking/mute flags,
network quality, a "3D per speaker" toggle that spawns `AurixParticipantPlayer`s) that reads
`AURIX_GODOT_WS` / `AURIX_GODOT_TOKEN_A` / `AURIX_GODOT_CHANNEL` when set.

### Android and iOS

`scripts/build_native.sh --target <triple>` stages the mobile slices with the same layout as
desktop; the `.gdextension` already lists them, so a Godot Android/iOS export picks them up once
the files exist:

| Target | Prerequisites | Staged |
|---|---|---|
| `aarch64-linux-android`, `armv7-linux-androideabi`, `x86_64-linux-android` | `cargo install cargo-ndk`, `ANDROID_NDK_HOME` (or `ANDROID_HOME` with `ndk/`), API 21+ (`ANDROID_API_LEVEL`) | `bin/android.<arm64|arm32|x86_64>/libaurix_client.so` + `libaurix_voice.android.<target>.<arch>.so` (scons `platform=android`) |
| `aarch64-apple-ios` (device + `aarch64-apple-ios-sim`) | macOS with Xcode (`xcodebuild`, cmake) | `bin/ios.arm64/libaurix_client.xcframework` (static, device + simulator) + `libaurix_voice.ios.<target>.xcframework` built from two static archives (`ios_simulator=no|yes`) |

cargo-ndk points the bundled libopus build (cmake) at the NDK toolchain; on iOS both
`aurix_client` and the extension are static archives, linked by the Xcode project Godot
generates, so nothing is `dlopen`ed at runtime. The Android export needs the `RECORD_AUDIO`
permission and, for media over UDP/QUIC, `INTERNET`; iOS needs `NSMicrophoneUsageDescription`.
Background audio, CallKit/AudioSession routing and Android audio focus are the game's
responsibility — the extension only pushes and pulls PCM.

**Not verified here:** this repository's CI has no Android NDK or Xcode, so the mobile slices
are neither built nor exported nor run on a device by it — the recipe above is staged and
documented, and the first `build_native.sh --target aarch64-linux-android` on a machine with the
NDK is the verification step ([Limitations](../limitations.md)).

## Web export: `AurixWebVoiceClient`

A Godot **Web** export cannot run the GDExtension, but it can run the browser: the addon ships
`addons/aurix_voice/web/aurix_web_voice_client.gd`, a plain GDScript node that drives the
[Web SDK](web.md) through `JavaScriptBridge`. The bundle `aurix-web-sdk.js` is loaded next to
`index.html` (or from `sdk_url`), `AurixWebSdk.AurixBridge` (the same handle-based façade the
Unity WebGL client uses) is created per node, commands are `invoke()`d as JSON, and events are
drained every frame in `_process` and re-emitted as Godot signals with the **same names and
dictionary shapes as the native `AurixVoiceClient`** (`state_changed`, `session_ready`,
`channel_joined`, `participant_joined/left/speaking/mute_changed`, `chat_message`,
`transcript`, `network_quality`, `recovering/recovered`, …) plus browser-only ones
(`sdk_status_changed`, `token_requested`, `remote_audio`, `participant_streams_changed`,
`participant_visemes`, `e2ee_peer_key`, `devices_changed`, `stats`). Methods return the native
`RESULT_*` codes; asynchronous calls (`join_channel`, `send_chat`, `channel_history`, …) return
a positive request id and finish through the corresponding signal or `request_failed`.

```
 Godot Web export (wasm, main thread)                      browser
 ┌──────────────────────────────────┐  JavaScriptBridge   ┌────────────────────────────────┐
 │ AurixWebVoiceClient : Node       │──── invoke(json) ──►│ AurixGodot glue → AurixBridge  │
 │   _process(): drain() → signals  │◄─── drain() ────────│ AurixClient (Web SDK)          │──► wss control
 │   same signals as the native node│                     │ getUserMedia → WebRTC ◄── mixed + per-participant tracks
 └──────────────────────────────────┘                     │ HTML audio / Web Audio / HRTF  │
                                                           └────────────────────────────────┘
```

What differs from the native node is **audio ownership**: the browser captures the microphone
(`getUserMedia`) and plays remote audio (HTML audio, or Web Audio with HRTF when
`spatial_audio` is on and per-participant tracks are enabled) — `AudioStreamGenerator`,
`AurixParticipantPlayer`, `push_capture`/`pull_participant`, engine-side 3D and the native
DSP/codec/transport knobs simply do not exist on this node (no emulation; `network_changed()`
maps to a WebRTC renegotiation and `get_media_path()` reports `MEDIA_WEBRTC`). Positions are
still sent (`update_positions`/`update_transforms`) and the browser spatializes with the
channel's `positional` policy. Autoplay policy applies: call
`resume_audio()` from a user gesture when `remote_audio(false, "autoplay")` fires. E2EE,
visemes, voice effects, transcripts/translation, chat/history, TTS, devices, quality/stats and
token refresh (`token_requested` → `provide_token`, or `set_token` for a static token) follow
the Web SDK.

```gdscript
const WebClient := preload("res://addons/aurix_voice/web/aurix_web_voice_client.gd")

func _ready() -> void:
    if not WebClient.is_supported():   # false outside a Web export
        return
    var voice := WebClient.new()       # exported vars: participant_streams, spatial_audio, e2ee, visemes, sdk_url…
    add_child(voice)
    voice.session_ready.connect(func(s): voice.join_channel(CHANNEL))
    voice.remote_audio.connect(func(playing, reason): if not playing: $EnableAudio.show())
    voice.connect_to_server("wss://voice.example.com/ws", token)
```

`project.godot` sets `run/main_scene.web = res://demo/web_main.tscn`: the Web lobby
(`demo/web_main.gd`) mirrors the native demo (connect/join/mute/chat/roster/quality) and reads
`?ws=&api=&token=&channel=` from the page URL. `scripts/build_web.sh` exports the `Web` preset
(`export_presets.cfg`, threads off so no COOP/COEP headers are needed) and copies
`sdk/web/dist/aurix-web-sdk.js` next to `index.html`; serve the folder over HTTP(S). The export
still contains the desktop `.gdextension`, so the browser console shows one harmless
`No GDExtension library found for current OS and architecture (web.wasm32)` line at boot —
the native node is intentionally absent on the Web.

Tests: `tests/web_smoke.gd` (headless, no browser — unsupported-platform paths, helper
conversions and every bridge event → signal mapping) and `tests/web/godot_web_e2e.py`
(Playwright/Chromium: the real export boots, loads the SDK and fails a dead endpoint cleanly;
with `AURIX_API_URL`/`AURIX_WS_URL`/`AURIX_API_KEY` it joins a channel next to a plain Web SDK
peer and checks roster, per-participant stream layout, speaking, chat both ways, network
quality, stats, mute/volume/pin, leave and disconnect). Both run in the `godot` / `godot-web`
CI jobs. Firefox/Safari and real microphones are not exercised in CI.
