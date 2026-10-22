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

## Region discovery

Static helpers mirror the C ABI: `AurixVoiceClient.regions_discovery_url(api_url, preferred_region, latitude, longitude)`
builds the `GET /v1/me/regions` URL (send it with `HTTPRequest` and the player's token), and
`AurixVoiceClient.rank_regions(body, rtt_ms_by_region, preferred_region, rtt_tolerance_ms)` returns the regions
best-first after you add the RTTs you measured against each entry's `probe_url`; connect to
`result[0].ws_url`. On Aurix Cloud the token server can do the choice instead
(`POST /v1/projects/{id}/tokens` returns `endpoint`).

## Inspector defaults

Everything a designer needs is on the `AurixVoiceClient` node: connection (`auto_reconnect`,
`media_path_policy`, `quic`, `tls_tunnel`), capture/playback (`auto_capture`, `auto_playback`,
`playback_bus`, `playback_mode`, `playback_buffer_seconds`) and the *Audio Defaults* group —
`preferred_codec` (Opus / PCMU / PCMA), `preferred_downlink_mode` (Streams / Mixed), `input_gain`,
`vad_threshold`, `vad_hangover_frames` — applied once the session is ready; the matching
`set_*` methods change them at runtime.

Mobile lifecycle is handled by the node: on `NOTIFICATION_APPLICATION_PAUSED` (and window focus
loss on desktop) it drops the microphone when `stop_microphone_in_background` is on (default), and
on resume restarts capture and, if the app was away for at least `probe_after_background_seconds`
(default 2), calls `probe_connection()` so a socket the OS silently killed is replaced at once
instead of after the ping timeout. `network_changed()` is still yours to call from platform
connectivity callbacks — Godot has no reachability API to poll.

Push-to-talk is a property group too: `push_to_talk_action` (an InputMap action — keyboard,
gamepad or touch, whatever the action binds), `push_to_talk_toggle` (press to start / press to
stop instead of hold) and `push_to_talk_release_delay` (seconds the mic stays open after release,
default 0.15). With an action set the node mutes itself until the action is pressed and calls
`set_muted` for you, so the roster and the server agree. `get_input_level()` (0..1 of the last
captured frame, independent of mute) and `get_input_energy()` feed a microphone meter from
`_process`.

`is_push_to_talk_transmitting()` and the `push_to_talk_changed(transmitting)` signal drive an
on-air indicator; `get_input_level()` (0..1) feeds a `ProgressBar`. The demo scene wires both
(`PttButton`, `InputLevel`).

## Audio injection

`inject_audio(pcm: PackedFloat32Array, sample_rate_hz, channels, loop = false, gain = 1.0,
mix_with_microphone = true)` plays a clip into your transmitting channels — soundboards, voice
lines, and the microphone test (join an echo channel, inject, listen). It works without a
microphone (frames are pushed from `_process`), `stop_audio_injection()` ends it and
`audio_injection_changed(active)` fires at start and when a non-looping clip finishes. Decode an
`AudioStreamWAV` with `data` → floats, or use `AudioStreamGenerator`-style PCM you already have.

## Diagnostics

- `participant_capture_changed(channel_id, user_id, available)` and the `capture_unavailable`
  key of every participant dictionary: the member has no working microphone (their SDK reported
  it); call `set_capture_available(false)` yourself when your own capture fails and `true` when
  it recovers — the built-in `start_capture` does it for you.
- `speaking_while_muted(speaking)`: the local VAD hears speech while muted — show "you are muted".
- `participant_removed` / `participant_left` carry a reason (`left`, `timeout`, `kicked`,
  `banned`, `out_of_range`, `channel_closed`, `replaced`).
- `get_transmission()`, `get_channel_focus()`, `is_priority(channel_id)`, `get_user_id()`,
  `is_capture_available()`, `get_input_gain()` read back what you set.
- `probe_connection(timeout_ms = 3000)`: ping now, reconnect only if no reply — free when the
  link is fine (after resuming, after a scene change). `reconnect_now()` drops unconditionally.
- `AurixVoiceClient.describe_error(code)` / `is_error_retryable(code)` /
  `is_error_player_facing(code)` (static): the cookbook's "what to do" for a wire error code.

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

Releases ship a prebuilt addon (`aurix-godot-addon-<version>.zip`: Linux x86_64/arm64, Windows
x86_64, macOS universal and Android arm64, debug + release templates, Rust core included) — unzip
into your project's `addons/` and enable the plugin; the `-source.zip` variant is for building the
other targets yourself with the script above.

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

**Verification status:** the release workflow builds the Android arm64 slice (`cargo-ndk`) and
ships it in the addon zip, so that path is exercised on every release; armv7/x86_64 Android and
the iOS slices are build targets of `build_native.sh` only — this repository has no Xcode runner
and ships no iOS artifacts, and none of the mobile slices is run on a device by CI
([Limitations](../limitations.md), [Support matrix](support-matrix.md)).

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
