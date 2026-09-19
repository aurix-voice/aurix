# aurix-client — native voice client core and C ABI

`aurix-client` is the reusable client foundation for engines and native applications: the same
AURX v2 media path and WebSocket control plane the Unity SDK implements in C#, written once in
Rust and exposed both as a Rust crate and as a stable C ABI (`include/aurix_client.h`). The
Unreal plugin, mobile wrappers and any other native integration build on top of it.

```
 your engine / app              aurix-client
 ┌──────────────────┐   PCM     ┌──────────┐  Opus + level  ┌───────┐  AURX v2 (AES-CTR+HMAC)
 │ mic callback     ├──────────►│ capture  ├───────────────►│ media ├────────────────────────► UDP
 │ speaker callback │◄──────────┤ mixer    │◄───────────────┤ (udp) │◄──────────────────────── UDP
 │ game thread      │ poll      ├──────────┤                └───────┘
 │  (events, joins) │◄─────────►│ client   ├─── typed ControlMessage ─── WebSocket (wss, bearer)
 └──────────────────┘           └──────────┘
```

## What it does

* **Audio** (`audio`): 48 kHz / 20 ms Opus encoder with every libopus control
  (`EncoderSettings`: bitrate, complexity, max bandwidth, signal, VBR/CVBR, FEC, expected loss,
  DTX), input gain, VAD and RFC 6464 level metadata, resampling from any device rate, per-sender
  jitter buffers with PLC, stereo mixer with per-participant gain and directional panning, output
  volume/mute. The encoder follows the merged channel audio policy (`ChannelJoinAck.audio`,
  `ChannelAudioPolicy`) and the server's transient `BitrateCommand`; complexity can be pinned.
  `OpusEncoder` / `OpusDecoder` are also exported standalone (C: `aurix_opus_*`) for hosts that
  only want libopus — the Unity SDK's `NativeOpusCodec` uses them.
* **Capture DSP** (`dsp`): pure-Rust microphone processing between resampling and the input
  gain/VAD/encoder — 80 Hz high-pass, frequency-domain acoustic echo cancellation (40–500 ms
  tail, delay estimation, residual echo suppression) fed automatically by `mix_output_*`,
  RNNoise-derived neural noise suppression (`nnnoiseless`) and a speech-gated AGC with a soft
  limiter. `DspConfig` / `DspStats`; also exported standalone (C: `aurix_dsp_*`).
* **Media** (`media`): AURX v2 over UDP — signed `SessionBind`, AES-256-CTR + HMAC on every
  packet, per-sender replay windows, heartbeats with RTT, quality reports, mute state.
* **Control** (`control`): WebSocket with `Authorization: Bearer`, one-time resume tokens,
  typed `ControlMessage` send/receive, `SessionInitAck` → media key.
* **Client** (`client`): one voice session on its own small Tokio runtime; automatic reconnect
  with exponential backoff and session resume (same session id / SSRC / media key / channels),
  re-join of channels after a fresh session, receiver preferences (mute, volume, block, focus,
  transmission mode) that survive resume, chat/typing, moderation with action tokens,
  transcripts, TTS, recording consent, 3D positions, statistics — all behind a synchronous,
  thread-safe API with a poll-based event queue.
* **FFI** (`ffi`): 70+ `extern "C"` functions, `#[repr(C)]` structs/enums, opaque handles,
  explicit ownership, thread-local error strings, JSON escape hatch for every event.

## Rust usage

```rust
use aurix_client::{Client, ClientConfig, Event};
use std::time::Duration;

let client = Client::new(ClientConfig::new("wss://voice.example.com/ws", session_jwt))?;
client.connect()?;
loop {
    match client.wait_event(Duration::from_millis(100)) {
        Some(Event::SessionReady(_)) => { client.join_channel(channel_id, None)?; }
        Some(Event::ChannelJoined { participants, .. }) => println!("{} others", participants.len()),
        Some(Event::Disconnected { reason, .. }) => break,
        _ => {}
    }
    // audio thread: client.push_capture_f32(mic_pcm, device_rate, channels);
    //               client.mix_output_f32(speaker_pcm, 2);
}
```

`push_capture_*` and `mix_output_*` are lock-light and meant for the audio callback; everything
else can be called from the game thread. `poll_event`/`wait_event` return owned events; the
optional wake hook (`set_wake_hook`) lets an engine signal its main loop instead of polling.

### Capture DSP: high-pass, echo cancellation, noise suppression, AGC

```rust
use aurix_client::{DspConfig, NoiseSuppression};

let mut cfg = ClientConfig::new(ws_url, jwt);
cfg.dsp = DspConfig {
    high_pass: true,
    echo_cancellation: true, echo_tail_ms: 200, stream_delay_ms: 0,
    noise_suppression: NoiseSuppression::High,
    agc: true, agc_target_dbfs: -18.0, agc_max_gain_db: 24.0,
};                                        // == DspConfig::default(); DspConfig::BYPASS = off

client.set_dsp(cfg.dsp);                  // live changes, no glitch
let stats = client.dsp_stats();           // erle_db, echo_delay_ms, echo_converged,
                                          // far_end_active, speech_probability, agc_gain_db
// audio thread, only if the game plays sound the core did not render:
client.push_render_f32(speaker_pcm, channels);
```

The chain runs on 48 kHz mono after downmix/resampling and before input gain, VAD and the
encoder: high-pass → AEC → NS → AGC. Everything `mix_output_f32/i16` produces is the AEC's
far-end reference, so the plain "core plays remote voices" setup needs nothing else; when the
game routes voice through its own mixer or plays music through the same speakers, feed that
output to `push_render_*` in playout order (the delay estimator absorbs up to 500 ms of
buffering between render and capture; `stream_delay_ms` pre-loads a known offset). `DspStats`
is what a diagnostics overlay shows — `echo_converged`/`erle_db` tell whether the canceller
has locked, `far_end_underruns` counts render audio it needed but had not been given.

Values are clamped, not rejected (tail 40–500 ms rounded to 10 ms, AGC target −30…−6 dBFS,
max gain 0–40 dB). NS costs ≈ 1–2 % of a core at 48 kHz; AEC scales with the tail length.

### Statistics and network quality

`client.stats()` returns one `ClientStats` snapshot: transport counters (`media`: packets/bytes
both ways, audio frames, `bad_auth`, `replayed`, `heartbeats_lost`, RTT last/min/avg/max),
`transmit` (frames encoded / sent / gated by VAD), mixer totals (`frames_lost`, `frames_late`,
`underruns`), downlink `jitter_ms`, `loss_percent` over the last period (`0..=100`), the
rating (`r_factor` `0..=100`, `mos`, `bars` 1–5, same formula as the server), per-stream
`streams` and the last server-side `NetworkQuality` (`server`, also delivered as
`Event::NetworkQuality` whenever the bars change — it merges your downlink report with the
uplink loss/jitter the SFU measures). The client sends a quality report over the media
transport on every heartbeat (`ClientConfig::heartbeat_interval`, 5 s), which also drives the
server's adaptive downlink bitrate. In C: `aurix_client_stats`,
`aurix_client_network_quality`, `aurix_event_network_quality`.

### Region discovery

The core deliberately has no HTTP client — the host (engine, launcher) does the two HTTP steps
and the core does everything that must be identical across SDKs:

```rust
use aurix_client::regions::{discovery_url, parse_regions, rank_regions, ProbedRegion};

let url = discovery_url(api_url, preferred_region, player_location);   // GET with `Authorization: Bearer <jwt>`
let body = parse_regions(&http_get(&url, jwt)?)?;                      // GET /v1/me/regions
let probed = body.regions.into_iter()
    .map(|ep| { let rtt = ep.probe_url.as_deref().and_then(|u| min_rtt_ms(u, 3)); ProbedRegion::unprobed(ep).with_rtt(rtt) })
    .collect();
let ranked = rank_regions(probed, preferred_region, 15.0);
let ws_url = &ranked[0].endpoint.ws_url;                                // node-local: resume lands on the same node
```

`rank_regions`: preferred region first unless its probe failed → RTT in `tolerance_ms` buckets
(ties keep the server's distance/load order) → unprobed regions → regions whose probe failed
(`ProbedRegion::probe_failed`). Only healthy nodes with a public `wss://` URL are returned by
the server; an empty list means none is configured for discovery.

In C the same flow is `aurix_regions_discovery_url` → *host GET* → `aurix_regions_parse`
(opaque `AurixRegionList`, freed with `aurix_regions_free`) → *host probes each
`AurixRegionEndpoint.probe_url`* → `aurix_regions_set_rtt(list, i, rtt_ms | -1.0 for
unreachable)` → `aurix_regions_rank(list, preferred_or_NULL, tolerance_ms)` →
`aurix_regions_get(list, 0, &out)`; C++: `aurix::Regions` (RAII, `parse` / `set_rtt` / `rank` /
`all`). `examples/cpp/voice_loop.cpp` prints the ranking of a JSON body passed in
`AURIX_REGIONS_JSON`.

## C ABI

The header is generated with cbindgen and committed; `tests/c_abi.rs` fails if it drifts:

```bash
cargo run -p aurix-client --example gen_header     # regenerate include/aurix_client.h
cargo build -p aurix-client --release              # libaurix_client.{so,dylib,dll,a}
cc -std=c99 -Icrates/aurix-client/include crates/aurix-client/examples/c/voice_loop.c \
   -Ltarget/release -laurix_client -lm -o voice_loop
```

`examples/c/voice_loop.c` is a complete integration: create → connect → wait for
`AURIX_EVENT_SESSION_READY` → join → push a tone / mix output → print statistics → disconnect.

Capture DSP in C: `AurixDspConfig` in `AurixConfig.dsp` (`aurix_dsp_config_default` /
`aurix_dsp_config_bypass` presets), `aurix_client_set_dsp` / `aurix_client_dsp`,
`aurix_client_dsp_stats` (`AurixDspStats`), `aurix_client_push_render_f32/i16`. The processor is
also available **standalone** for hosts with their own transport: `aurix_dsp_create` /
`aurix_dsp_destroy`, `aurix_dsp_set_config` / `aurix_dsp_config` / `aurix_dsp_stats`,
`aurix_dsp_process_f32` (mono 48 kHz, in place, any whole number of 480-sample blocks —
`AURIX_INVALID_ARGUMENT` otherwise) and `aurix_dsp_push_render_f32`. None is variadic, so they
are safe P/Invoke targets — the Unity SDK's `NativeCaptureDsp` is built on them.

### C++ wrapper

`include/aurix_client.hpp` is a header-only C++11 layer over the C ABI: move-only RAII
`aurix::Client` / `aurix::Event`, `aurix::Uuid` (parse/format), `aurix::Config`, and
`std::string`/`std::vector` conveniences for every call. It adds no behaviour and no extra
library; `examples/cpp/voice_loop.cpp` mirrors the C sample and is compiled and run by the same
test. The Unreal plugin in [`sdk/unreal`](../../sdk/unreal) is built on this wrapper.

```bash
c++ -std=c++11 -Icrates/aurix-client/include crates/aurix-client/examples/cpp/voice_loop.cpp \
    -Ltarget/release -laurix_client -lm -o voice_loop_cpp
```

### Ownership and threading rules

* `aurix_client_create` returns a handle owned by the caller; `aurix_client_destroy` disconnects
  and frees it. Never destroy from inside the wake callback.
* `aurix_client_poll_event` / `aurix_client_wait_event` return events owned by the caller;
  release each with `aurix_event_free`. Pointers returned by `aurix_event_*` accessors stay valid
  until that free.
* Input strings and arrays are borrowed for the duration of the call and copied when retained
  (`token`, `ws_url`, join tokens, chat text).
* `aurix_last_error` is thread-local and valid until the next failing call on the same thread.
* The wake callback runs on an internal thread; do nothing but signal there. Events themselves
  are consumed from your own thread.
* Audio functions are safe to call from a real-time audio thread concurrently with control calls
  from the game thread.
* `aurix_client_disconnect` blocks for up to ~3 s to let the server acknowledge; `connect` is
  asynchronous and reports through `AURIX_EVENT_STATE_CHANGED` / `SESSION_READY` / `DISCONNECTED`.
* Reconnect: with `auto_reconnect` the client emits `RECOVERING` (per attempt), then either
  `RECOVERED` (with `resumed` = same session) or `FAILED_TO_RECOVER`; when the server issued a
  fresh session, channels are re-joined automatically and `REJOIN_FAILED` names the ones that
  did not come back.

Rust enums never cross the ABI as-is; every event, mode and role has a `#[repr(C)]` mirror, and
`aurix_event_json` exposes the full serialised event for fields that have no dedicated accessor.

## Tests

```bash
cargo test -p aurix-client                 # unit tests (audio, media with a fake server, ABI)
cargo test -p aurix-client --test c_abi    # header is up to date, C/C++ samples compile/link/run,
                                           # Unreal plugin references only declared ABI symbols
AURIX_E2E_API_KEY=aurx_... cargo test -p aurix-client --test e2e_live -- --nocapture
```

The live test drives two native clients against a running node: session + media bind, roster,
encrypted Opus tone Alice→Bob (RMS and gapless sequence asserted), speaking/energy events, chat
with request correlation, WebSocket drop through a TCP proxy → resume with the same session,
audio continues, leave and disconnect.
