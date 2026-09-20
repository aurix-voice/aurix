# Godot SDK (GDExtension)

`sdk/godot` is a Godot **4.3+** GDExtension over the [native core](native.md): the C++ classes
call `aurix_client.hpp` (the header-only wrapper of the stable C ABI) and translate between
Godot types and the C structs. No protocol, codec, crypto or networking code lives in the
extension — AURX v2 over UDP/tunnel, the WebSocket control plane, Opus, jitter buffers, VAD, the
DSP chain, reconnect/resume and cross-node failover are the same Rust library that Unreal and
the C samples use. Full reference: `sdk/godot/README.md`.

```
 Godot scene tree (main thread)                     aurix-client (own threads)
 ┌──────────────────────────────┐   _process()      ┌──────────────────────────┐
 │ AurixVoiceClient : Node      │──── push PCM ────►│ capture → DSP → Opus     │──► UDP / WS tunnel
 │   AudioStreamMicrophone      │◄─── mix_output ───│ jitter → decode → mixer  │◄── UDP / WS tunnel
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
`STATE_DISCONNECTED … STATE_FAILED`, media path `MEDIA_NONE | MEDIA_UDP | MEDIA_TUNNEL`; all
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
macOS universal to `bin/<platform>.<arch>/` and declares `aurix_client` as a `[dependencies]`
entry so export templates ship both libraries. Android/iOS follow the same recipe with the
matching Rust target and godot-cpp platform; Web exports are not supported by this extension
(the native core needs UDP or a raw WebSocket — use the [Web SDK](web.md) from JavaScript).
Consoles: [Porting to consoles](consoles.md). Flutter / React Native:
[Mobile app frameworks](mobile-frameworks.md).

`demo/main.tscn` is a lobby scene (connect, join, mute, chat, roster with speaking/mute flags,
network quality, a "3D per speaker" toggle that spawns `AurixParticipantPlayer`s) that reads
`AURIX_GODOT_WS` / `AURIX_GODOT_TOKEN_A` / `AURIX_GODOT_CHANNEL` when set.
