# Aurix Voice for Godot 4 (GDExtension)

`aurix_voice` is a Godot 4.3+ GDExtension that wraps the native
[`aurix-client`](../../crates/aurix-client/README.md) library through its stable C ABI
(`aurix_client.h` / the header-only `aurix_client.hpp`). Like the Unreal plugin it contains **no**
protocol, crypto, networking or codec code of its own: AURX v2 over UDP (or the WebSocket
tunnel), the control plane, Opus, jitter buffers, VAD, DSP, directional mixing, reconnect/resume
and cross-node failover all live in the Rust library. The Godot side is three classes that pump
native events into signals on the main thread and bridge Godot's audio server to the client.

```
sdk/godot/
├── addons/aurix_voice/
│   ├── aurix_voice.gdextension           library map + native dependency per platform
│   ├── bin/<platform>.<arch>/            staged by scripts/build_native.sh (git-ignored)
│   └── web/aurix_web_voice_client.gd     AurixWebVoiceClient — Web export over JavaScriptBridge + Web SDK
├── src/
│   ├── aurix_voice_client.{h,cpp}        AurixVoiceClient : Node — the whole API + signals
│   ├── aurix_participant_player.{h,cpp}  AurixParticipantPlayer : AudioStreamPlayer3D
│   ├── aurix_regions.{h,cpp}             AurixRegions : RefCounted — region discovery/ranking
│   ├── aurix_conversions.h               C ABI structs → Dictionary / String / Array
│   └── register_types.cpp                GDExtension entry point
├── SConstruct                            builds against godot-cpp + the staged native library
├── scripts/build_native.sh               cargo build aurix-client → stage → scons (desktop, Android via cargo-ndk, iOS xcframeworks)
├── scripts/build_web.sh                  Web export + aurix-web-sdk.js next to index.html
├── export_presets.cfg                    "Web" preset (threads off)
├── demo/main.tscn + main.gd              lobby UI: connect, join, mute, chat, 3D per speaker
├── demo/web_main.tscn + web_main.gd      the same lobby for the Web export (main scene on web)
├── tests/smoke.gd                        headless API smoke test (no server)
├── tests/web_smoke.gd                    headless AurixWebVoiceClient smoke test (no browser)
├── tests/web/godot_web_e2e.py            Playwright: exported lobby in Chromium (+ live node)
├── tests/live.gd + live.sh               headless two-client test against a running node
└── project.godot                         minimal project wrapping the addon
```

| Class | Base | Role |
|---|---|---|
| `AurixVoiceClient` | `Node` | one per player: connect, channels, capture, playback, receiver controls, chat, speech, stats; every native event is a signal |
| `AurixParticipantPlayer` | `AudioStreamPlayer3D` | one per remote speaker you want spatialised by Godot; pulls that participant's PCM from the client |
| `AurixRegions` | `RefCounted` | parses `GET /v1/me/regions`, stores RTT probes, ranks endpoints |
| `AurixWebVoiceClient` | `Node` (GDScript) | Web export only: same signals/dictionaries as `AurixVoiceClient`, media by the browser through the [Web SDK](../web/README.md) (`JavaScriptBridge`) |

Platforms: Linux x86_64/arm64, Windows x86_64, macOS (universal) — wherever `aurix-client`
builds and godot-cpp has a template. Android (arm64/arm32/x86_64, `cargo-ndk`) and iOS (arm64
device + simulator xcframeworks, Xcode) are staged by `build_native.sh --target …` and listed in
the `.gdextension`, but are not built in this repository's CI. Web (wasm) cannot load the
extension; `AurixWebVoiceClient` + `scripts/build_web.sh` cover it (browser owns capture and
playback; no `AurixParticipantPlayer`). Consoles: see
[Porting to consoles](../../docs/src/sdk/consoles.md). Details:
[docs/src/sdk/godot.md](../../docs/src/sdk/godot.md).

## 1. Build

Requirements: Rust 1.88+, a C/C++ compiler (the bundled Opus is compiled from source and linked
statically into `aurix_client`), Python 3 + [SCons](https://scons.org) 4.x, a checkout of
[godot-cpp](https://github.com/godotengine/godot-cpp) at `godot-4.3-stable`.

```bash
git clone --depth 1 --branch godot-4.3-stable https://github.com/godotengine/godot-cpp sdk/godot/godot-cpp
sdk/godot/scripts/build_native.sh                       # host target, template_debug
sdk/godot/scripts/build_native.sh --release             # template_release
sdk/godot/scripts/build_native.sh --target aarch64-unknown-linux-gnu
GODOT_CPP_PATH=~/godot-cpp sdk/godot/scripts/build_native.sh
ADDON_DIR=/path/to/MyGame/addons/aurix_voice sdk/godot/scripts/build_native.sh
```

The script runs `cargo build -p aurix-client`, copies `libaurix_client.so|.dylib` /
`aurix_client.dll` (+ import library) into `addons/aurix_voice/bin/<platform>.<arch>/`, then
runs `scons platform=… target=… arch=…` which links `libaurix_voice.<platform>.<target>.<arch>`
against it (rpath `$ORIGIN` / `@loader_path`, so both libraries ship side by side). Pass
`--no-extension` to stage only the native library and run SCons yourself with extra godot-cpp
options (`use_llvm=yes`, `dev_build=yes`, …).

Godot loads `.gdextension` files it discovers when the project is (re)imported; the first time,
open the project in the editor once or run `godot --headless --path sdk/godot --import`.

Copy `addons/aurix_voice/` into your own project to use it; the `.gdextension` maps every
platform to `res://addons/aurix_voice/bin/...` and lists `aurix_client` under
`[dependencies]`, so the export templates bundle it next to the extension.

## 2. Connect and join

```gdscript
extends Node

@onready var voice: AurixVoiceClient = $AurixVoiceClient

func _ready() -> void:
	voice.session_ready.connect(func(s): print("session ", s.session_id, " user ", s.user_id))
	voice.channel_joined.connect(func(_rid, channel_id, participants, info):
		print("joined ", channel_id, " as ", "listener" if info.role == AurixVoiceClient.ROLE_LISTENER else "speaker"))
	voice.participant_joined.connect(func(_ch, p): print(p.display_name, " joined"))
	voice.participant_speaking.connect(func(_ch, user_id, speaking): _set_talking_icon(user_id, speaking))
	voice.recovering.connect(func(attempt, delay_ms, cause): print("reconnecting #", attempt, ": ", cause))
	voice.request_failed.connect(func(_rid, code, message): push_warning(code + ": " + message))

	# The token comes from YOUR backend (POST /v1/tokens with the app's API key); never embed
	# API keys in the game.
	var r := voice.connect_to_server("wss://voice.example.com/ws", token)
	if r != AurixVoiceClient.RESULT_OK:
		push_error(voice.get_last_error())

func join(channel_id: String, join_token := "") -> void:
	voice.join_channel(channel_id, join_token)   # returns the request id (0 = not sent)
```

`connect_to_server` creates the native client from the node's exported properties
(`auto_reconnect`, `reconnect_max_attempts`, `request_timeout_ms`, `jitter_target_frames`,
`vad_gate`, `follow_channel_policy`, `media_path_policy`, `dsp_bypass`, …) and starts the
connection; `set_token` swaps the bearer token for the next reconnect. `disconnect_from_server`
tears everything down (also on `_exit_tree`). The node polls native events from `_process`
(`max_events_per_frame`, default 256), so every signal fires on the main thread.

Results are `AurixVoiceClient.RESULT_*` (the C ABI's `AurixResult`); connection states are
`STATE_DISCONNECTED … STATE_FAILED`; `get_media_path()` is `MEDIA_NONE / MEDIA_UDP /
MEDIA_TUNNEL`. `get_session()` / `get_endpoint()` / `get_failover_endpoints()` describe the live
session; `get_last_error()` is the native thread-local error string.

### Region discovery

```gdscript
var regions := AurixRegions.new()
var url := AurixRegions.discovery_url("https://voice.example.com", "eu_west")   # GET with the player's bearer token
# ... HTTPRequest ...
if regions.parse(body):
	for i in regions.size():
		regions.set_rtt(i, _probe(regions.get_endpoint(i).probe_url))   # optional
	regions.rank("eu_west")           # by RTT (then distance); the preferred region wins within rtt_tolerance_ms
	voice.connect_to_server(regions.best_ws_url(), token)
```

## 3. Audio

**Capture.** With `auto_capture = true` (default) the node creates a muted `AurixCapture` bus
with an `AudioEffectCapture`, plays an `AudioStreamMicrophone` into it and pushes the captured
frames into the native client every `_process` — the DSP chain (high-pass, AEC, noise
suppression, AGC), input gain, VAD gate and Opus encoder run inside `aurix-client`. Enable
`audio/driver/enable_input` in the project settings (the demo project does). To feed your own
PCM instead (custom device code, a different `AudioEffectCapture`, a headless bot), set
`auto_capture = false` and call `push_capture(PackedVector2Array, sample_rate_hz)` or
`push_capture_mono(PackedFloat32Array, sample_rate_hz)` at any rate — the core resamples.

Controls: `set_muted`, `set_input_gain`, `set_vad(threshold, hangover_frames)`, `set_vad_gate`,
`set_bitrate`, `set_complexity`, `set_encoder_settings(Dictionary)` (`bitrate_bps`, `complexity`,
`max_bandwidth`, `signal`, `vbr`, `fec`, `dtx`, `channels` for stereo/music uplinks…),
`set_dsp(Dictionary)` / `get_dsp_stats()`, `set_voice_effects(pitch_semitones, ring_mod_hz)`,
`get_input_energy()`, `is_speaking()` (+ the `local_speaking` signal), `get_audio_policy()`.

**Playback (mixed).** With `auto_playback = true` the node owns a stereo `AudioStreamPlayer`
(`playback_bus`, `playback_buffer_seconds`) fed from `mix_output()`; the remote mixer applies
per-participant volume, server gain, mute/block and directional panning. `set_output_volume`,
`set_output_muted`, `set_participant_volume`, `set_participant_mute(user, channel, muted)`
(`channel = ""` = everywhere), `set_user_block` (persistent, server-side).

**Playback (per participant / engine spatialisation).** Set `playback_mode` to
`PLAYBACK_PER_PARTICIPANT` (claimed speakers leave the mix) or
`PLAYBACK_PER_PARTICIPANT_ONLY` (only claimed speakers are audible) and add one
`AurixParticipantPlayer` per speaker:

```gdscript
var p := AurixParticipantPlayer.new()      # AudioStreamPlayer3D → attenuation, doppler, reverb buses…
p.user_id = participant.user_id
p.client_path = p.get_path_to(voice)       # or leave empty: the nearest AurixVoiceClient ancestor is used
avatar.add_child(p)
# p.is_active() → lip-sync; p.queue_free() releases the claim
```

The player claims the user on the client, pulls that user's PCM (microphone + TTS voice, stereo
preserved) every frame and pushes it through its own `AudioStreamGenerator`. Claims follow the
user across SSRC changes, reconnects and failover; a new native client (a later
`connect_to_server`) is re-claimed automatically. Lower-level: `pull_participant(user_id,
frames)`, `set_participant_claimed`, `get_participant_streams()`.

**Directional audio the server-side way.** `update_transforms(channel_id, {user_id:
Transform3D})` (or `update_positions` with `position/forward/up` dictionaries) feeds the
channel's positional audio; the server pans other participants for you when the channel is
positional and you keep `PLAYBACK_MIXED`. `push_render(PackedVector2Array)` gives the acoustic
echo canceller the final scene mix as reference when you route game audio yourself.

## 4. Receiver controls, chat, speech

`set_transmission(TRANSMIT_ALL | TRANSMIT_NONE | TRANSMIT_SINGLE, channel_id)`,
`set_channel_focus(channel_id)`, `set_audio_codec(CODEC_OPUS | CODEC_PCMU)`,
`set_downlink_mode(DOWNLINK_STREAMS | DOWNLINK_MIXED)`, `set_server_noise_suppression(enabled)` /
`get_server_noise_suppression()` (signal `server_noise_suppression_changed`; the node denoises
this session's uplink — `session_info.noise_suppression` says whether it offers that),
`set_transcripts(enabled)`,
`set_translation(language, spoken_language, speech)`, `respond_recording_consent(recording_id, accepted)`,
`send_control_json(json)` (forward-compatible escape hatch).

Chat: `send_chat`, `send_direct_chat`, `set_typing`, `channel_history` / `direct_history`
(keyset cursors), `mark_channel_read` / `mark_direct_read`, `channel_read_markers` /
`direct_read_markers`; signals `chat_message`, `participant_typing`, `chat_history`,
`chat_read_marker(s)`, `chat_inbox_synced`. Moderation: `moderate(channel, user, action,
action_token, reason)`. Speech: `speak(text, channel, destination, voice)`, `cancel_speech`,
signals `transcript` and `tts_status`.

Diagnostics: `get_stats()` (packets, bytes, loss, jitter, RTT, media path…),
`get_network_quality()` (+ the periodic `network_quality` signal with `bars` 0–5),
`get_native_version()`, and `raw_event(type, json)` for every event including ones this
wrapper has no dedicated signal for.

## 5. Threading and lifetime

* The native client runs its own network/media threads; the GDExtension only touches it from
  the main thread (`_process`, your calls). Do **not** call `AurixVoiceClient` methods from
  `AudioStreamGenerator` callbacks or other threads.
* Every value crossing the boundary is copied (`String`, `Dictionary`, `PackedVector2Array`);
  no pointer into an `AurixEvent` outlives `_process`.
* One `AurixVoiceClient` per player. Freeing the node disconnects and releases the native
  client; `AurixParticipantPlayer`s release their claims in `_exit_tree`.

## 6. Tests

```bash
godot --headless --path sdk/godot --import                    # once: register the extension
godot --headless --path sdk/godot -s tests/smoke.gd           # API surface, no server
AURIX_E2E_API=http://127.0.0.1:8080 AURIX_E2E_WS=ws://127.0.0.1:8081 AURIX_E2E_API_KEY=... \
  GODOT=godot sdk/godot/tests/live.sh                         # two clients through a real node
```

`live.gd` connects Alice and Bob, joins a channel, streams a 440 Hz tone from Alice's
`push_capture_mono` to Bob's `mix_output` (RMS ≈ 0.21), checks roster/speaking/chat signals,
claims Alice for per-participant pull (she leaves Bob's mix, `pull_participant` carries her),
leaves and disconnects. Exit code 0/1/2 = pass/fail/timeout.
