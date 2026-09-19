# Native core and Unreal SDK

`aurix-client` is the reusable native client: the AURX v2 media path and the WebSocket control
plane written once in Rust and exposed as a Rust crate, a stable **C ABI**
(`crates/aurix-client/include/aurix_client.h`, generated with cbindgen and drift-checked in
tests) and a header-only **C++11 RAII wrapper** (`aurix_client.hpp`). The Unreal plugin is a
thin layer over that wrapper; mobile wrappers or other engines can follow the same pattern.
Full reference: `crates/aurix-client/README.md` and `sdk/unreal/README.md`.

```
 your engine / app              aurix-client
 ┌──────────────────┐   PCM     ┌──────────┐  Opus + level  ┌───────┐  AURX v2 (AES-CTR+HMAC)
 │ mic callback     ├──────────►│ capture  ├───────────────►│ media ├────────────────────────► UDP
 │ speaker callback │◄──────────┤ mixer    │◄───────────────┤ (udp) │◄──────────────────────── UDP
 │ game thread      │ poll      ├──────────┤                └───────┘
 │  (events, joins) │◄─────────►│ client   ├─── typed ControlMessage ─── WebSocket (wss, bearer)
 └──────────────────┘           └──────────┘
```

## What the core does

* **Audio:** 48 kHz / 20 ms Opus with input gain, VAD gating and RFC 6464 level metadata,
  resampling from any device rate, per-sender jitter buffers with PLC, stereo mixer with
  per-participant gain and directional panning, output volume/mute. Opus is linked statically —
  no system `libopus` in the shipped game.
* **Media:** signed `SessionBind`, AES-256-CTR + HMAC on every packet, replay windows,
  heartbeats with RTT, quality reports.
* **Control:** `Authorization: Bearer` WebSocket, one-time resume tokens, typed
  `ControlMessage`s.
* **Client:** one voice session on a small private Tokio runtime; reconnect with backoff and
  session resume (same session / SSRC / media key / channels), automatic re-join after a fresh
  session (`REJOIN_FAILED` per channel that needs a new join token), receiver preferences that
  survive resume, chat, moderation with action tokens, transcripts, TTS, recording consent,
  positions, statistics — behind a synchronous, thread-safe API with a poll-based event queue.

## Rust

```rust
use aurix_client::{Client, ClientConfig, Event};
use std::time::Duration;

let client = Client::new(ClientConfig::new("wss://voice.example.com/ws", session_jwt))?;
client.connect()?;
loop {
    match client.wait_event(Duration::from_millis(100)) {
        Some(Event::SessionReady(_)) => client.join_channel(channel_id, None)?,
        Some(Event::ChannelJoined { participants, .. }) => println!("{} others", participants.len()),
        Some(Event::Disconnected { .. }) => break,
        _ => {}
    }
    // audio thread: client.push_capture_f32(mic_pcm, device_rate, channels);
    //               client.mix_output_f32(speaker_pcm, 2);
}
```

`push_capture_*` / `mix_output_*` are lock-light and meant for the audio callback; everything
else is game-thread API. `set_wake_hook` lets an engine signal its main loop instead of polling.

## C / C++

```bash
cargo build -p aurix-client --release              # libaurix_client.{so,dylib,dll,a}
cargo run -p aurix-client --example gen_header     # regenerate include/aurix_client.h
cc  -std=c99   -Icrates/aurix-client/include crates/aurix-client/examples/c/voice_loop.c     -Ltarget/release -laurix_client -lm -o voice_loop
c++ -std=c++11 -Icrates/aurix-client/include crates/aurix-client/examples/cpp/voice_loop.cpp -Ltarget/release -laurix_client -lm -o voice_loop_cpp
```

Both samples are complete integrations (create → connect → `AURIX_EVENT_SESSION_READY` → join →
push a tone / mix output → statistics → disconnect) and are compiled, linked and run by
`cargo test -p aurix-client --test c_abi`.

Rules that matter:

* `aurix_client_create` / `aurix_client_destroy` own the handle; never destroy from the wake
  callback. Events from `aurix_client_poll_event` / `wait_event` are freed with
  `aurix_event_free`; accessor pointers live until then.
* Input strings/arrays are borrowed for the call and copied when retained.
* `aurix_last_error` is thread-local.
* Audio functions may be called from a real-time thread concurrently with control calls.
* `aurix_client_disconnect` blocks up to ~3 s for the server acknowledgement; `connect` is
  asynchronous (`STATE_CHANGED` / `SESSION_READY` / `DISCONNECTED`).
* Every Rust enum has a `#[repr(C)]` mirror; `aurix_event_json` exposes the full serialised
  event for fields without a dedicated accessor.

Statistics: `aurix_client_stats` (`AurixStats`: media counters, `bad_auth`, `replayed`,
`heartbeats_lost`, RTT last/min/avg/max, jitter, `loss_percent`, `r_factor`, `mos`, `bars`),
`aurix_client_network_quality` and the `NETWORK_QUALITY` event — see
[Network quality](../features/quality.md).

## Region selection

The core has no HTTP client, so region discovery is split: the host performs the HTTP requests,
the core parses, stores probe results and ranks — identically to the Web and Unity SDKs.

1. `aurix_regions_discovery_url(api_url, preferred_region | NULL, has_location, lat, lon, buf, cap)`
   → `GET` it with `Authorization: Bearer <player JWT>` (`/v1/me/regions`).
2. `aurix_regions_parse(json)` → opaque `AurixRegionList` (`aurix_regions_len` / `aurix_regions_get`
   fill `AurixRegionEndpoint`: `region`, `node_id`, `ws_url`, `probe_url`, coordinates,
   `distance_km`, `nodes`, `load_factor`).
3. Probe each `probe_url` (a few GETs, discard the first, keep the minimum) and record it with
   `aurix_regions_set_rtt(list, i, rtt_ms)` — `-1.0` marks an unreachable node.
4. `aurix_regions_rank(list, preferred_region | NULL, tolerance_ms)`: preferred region first
   unless its probe failed → RTT in `tolerance_ms` buckets (ties keep the server's distance/load
   order) → unprobed → failed (`probe_failed`).
5. Use entry 0's `ws_url` as the WebSocket URL; `aurix_regions_free` when done.

Rust: `aurix_client::regions::{discovery_url, parse_regions, rank_regions, ProbedRegion}`;
C++: `aurix::Regions` (RAII: `discovery_url`, `parse`, `set_rtt`, `rank`, `all`). Server side:
[Regions](../operations/scaling.md#regions).

## Unreal plugin

`sdk/unreal/AurixVoice` is a runtime plugin for UE 5.3+ (Win64 / Linux / Mac). It contains no
protocol, crypto or codec code: `UAurixVoiceSubsystem` (a `UGameInstanceSubsystem`) pumps native
events into Blueprint delegates on the game thread, feeds the engine `AudioCapture` microphone
into `aurix_client_push_capture_f32` and plays the remote mix through a procedural 48 kHz stereo
`USoundWave`.

### Build and install

```bash
sdk/unreal/scripts/build_native.sh                              # Linux/macOS host → lib/Linux|Mac + include/
sdk/unreal/scripts/build_native.sh --target aarch64-apple-darwin
sdk/unreal/scripts/build_native.sh /path/to/MyGame/Plugins/AurixVoice
```

```powershell
sdk\unreal\scripts\build_native.ps1                             # Windows → lib/Win64 + include/
sdk\unreal\scripts\build_native.ps1 -PluginDir C:\MyGame\Plugins\AurixVoice
```

Then copy or symlink `sdk/unreal/AurixVoice` to `<Project>/Plugins/AurixVoice`, regenerate
project files and build. The `AurixClientLibrary` ThirdParty module fails at UBT time with a
message naming the missing header/library, so an unstaged plugin never fails silently at
runtime; the library is copied next to the game binaries through `RuntimeDependencies`.

### Usage

Blueprint: *Get Game Instance Subsystem → Aurix Voice Subsystem*, build *Aurix Voice Settings*
(WebSocket URL, token from your backend), **Connect**; on *On Session Ready* call **Join
Channel** (`Parse Uuid`, join token empty unless `require_action_tokens`); bind *On Channel
Joined*, *On Participant Joined/Left*, *On Participant Speaking*, *On Channel Energy*, *On Chat
Message*, *On Recovering/Recovered/Failed To Recover*; for positional channels call **Update Own
Position** from tick (`WorldToMeters` converts Unreal units).

```cpp
UAurixVoiceSubsystem* Voice = GetGameInstance()->GetSubsystem<UAurixVoiceSubsystem>();
Voice->OnSessionReady.AddDynamic(this, &AMyPC::OnVoiceReady);
Voice->OnParticipantSpeaking.AddDynamic(this, &AMyPC::OnSpeaking);

FAurixVoiceSettings Settings;
Settings.WebSocketUrl = TEXT("wss://voice.example.com/ws");
Settings.Token = Jwt;          // minted by your backend; deliberately not an editable property
Settings.bVadGate = true;
Voice->Connect(Settings);
```

Audio options: default engine capture + 2D `UAudioComponent` (`PlaybackSoundClass` routes it
through your mixer/ducking); or `PushCaptureAudio` from your own capture path and
`MixOutputAudio` from your own procedural sound/submix — both audio-thread safe until
`Disconnect()`.

`GetStats(FAurixStats&)`, `GetNetworkQuality(FAurixNetworkQuality&)` and `OnNetworkQuality`
expose the shared quality model; `OnRawEvent` delivers every event as JSON for anything without
a typed delegate. Events are dispatched from the subsystem tick, up to 256 per tick, nothing
dropped.

**Regions.** `DiscoverRegions(FAurixRegionDiscoveryRequest, OnComplete)` runs the whole flow
above with the engine's `HTTP` module (bearer `GET /v1/me/regions`, optional probes with
`ProbeSamples` / `ProbeTimeoutSeconds`, native ranking) and delivers a best-first
`TArray<FAurixRegionEndpoint>` to a Blueprint delegate; put `Regions[0].WsUrl` into
`FAurixVoiceSettings.WebSocketUrl`. `CancelRegionDiscovery()` drops an in-flight request; a new
`DiscoverRegions` cancels the previous one.

### Verification status

Verified in CI: the native library builds with statically linked Opus, and
`unreal_plugin_uses_only_existing_abi` parses the plugin sources and checks that every
`aurix_*` function, `AURIX_*` constant and `aurix::Client` method they use is declared in the
committed headers. **Not** verified: Unreal Header Tool and the module compile against a live
UE 5.3+ install (`AudioCaptureCore` callback signature, `USoundWaveProcedural::GeneratePCMData`)
and Windows/macOS packaging — treat the first build in your project as a required step; any
mismatch surfaces as a compile error in `AurixAudioCapture.cpp` or `AurixVoiceSoundWave.cpp`.
