# Porting to consoles

Aurix ships no PlayStation, Xbox or Nintendo Switch SDK: those platform SDKs are under NDA and
nothing from them can live in an open repository. What Aurix *does* give you is a native core
that was designed so the platform-specific part of a console port is small, well delimited and
entirely yours. This chapter describes that boundary — what the core needs from the platform,
where each console-specific decision plugs in, and which certification topics map to which
Aurix feature — so a platform team can scope the work without reading the Rust.

> Everything below the [handoff boundary](#the-handoff-boundary) is written against your
> platform's SDK, by people with access to it. Aurix contributors cannot review, test or accept
> that code; keep it in your own repository or a private fork.

## What the core is, from a porting point of view

`aurix-client` (`crates/aurix-client`) is a Rust library exported as a **C ABI**
(`aurix_client.h`, ~150 functions) plus a header-only C++11 wrapper, built as a `cdylib`
*and* a `staticlib` — link it statically into the title, which is what console toolchains
expect. Inside:

| Concern | How the core does it | Console relevance |
|---|---|---|
| Audio devices | **Never opens one.** The host pushes microphone PCM (`aurix_client_push_capture_*`, any rate/channels) and pulls the mix / per-participant PCM (`aurix_client_mix_output_*`, `aurix_client_pull_participant_*`). | Use the platform audio API in the host; nothing to port inside the core. |
| Threads | One tokio runtime (`AurixClientConfig::worker_threads`, 1–8, default 1) for sockets, control plane and timers; capture/mix calls execute on **the caller's** thread. Events are queued and read with `aurix_client_poll_event` / `wait_event`; `aurix_client_set_wake_callback` fires on an internal thread. | Thread budget is `worker_threads` + the threads you already own. No thread is created per channel or participant. |
| Sockets | Standard BSD sockets through Rust `std` / tokio: one UDP socket for AURX media (v4 or v6, chosen by racing the advertised candidates), one TCP+TLS connection for the WebSocket control plane. With `media_path = tunnel` media rides the WebSocket too — **one outbound TCP connection total**. | Platform socket layers, NAT/privilege checks and port policies are handled at the boundary; see [Networking](#networking-and-tls). |
| TLS | rustls with the bundled webpki root store (Mozilla roots); no OS certificate store, no OpenSSL. | Platforms that mandate their own trust store or pinning need a fork point (below). |
| Randomness | `getrandom` (OS CSPRNG syscall) for SSRCs, nonces, request ids. | Must map to the platform's CSPRNG; part of the Rust target support. |
| Memory | Rust global allocator (system `malloc`), no custom hooks; jitter buffers and event queues are bounded. Opus (libopus, compiled from C), RNNoise-derived NS (`nnnoiseless`) and the FFTs are pure compute. | Route through the title's allocator with a `#[global_allocator]` in the console build; measure with the platform profiler. |
| Logging | `tracing` spans/events; no `printf`, no files. | Attach the platform logger in the build's `tracing` subscriber, or leave silent. |
| Time | `std::time::Instant` / tokio timers; wall clock only for display fields. | — |
| Filesystem | Not used. | — |

Godot, Unreal and Unity are thin layers on top of this and stay engine-only code — a console
port of a Godot or Unreal title reuses the engine's own console toolchain and only swaps the
native core artefact and the platform layer below it.

## Prerequisites you must obtain from the platform vendor

1. **A Rust toolchain for the console target.** Rust has no public tier for PlayStation or
   Nintendo targets; Xbox (GDK) titles build with MSVC for x86-64 and the standard Windows
   target is the usual starting point, subject to the GDK's API restrictions. Ask your platform
   account manager what is available — several vendors provide Rust support under NDA. This is
   the first gate; without it the port does not start.
2. **Confirmation of the socket, TLS and audio policies** (allowed ports, whether user-level
   network privileges must be checked before opening sockets, whether the platform TLS stack is
   mandatory, headset/mic routing and sample-rate constraints).
3. **The certification checklist** for online voice communication on that platform
   (communication-restriction checks, mute/block/report flows, suspend/resume behaviour,
   accessibility requirements such as chat transcription).

## Building the core for a console

```
cargo build -p aurix-client --release --target <console-target> \
    --no-default-features             # if your build adds features, see below
# artefacts: target/<console-target>/release/libaurix_client.a  (+ include/aurix_client.h)
```

* Prefer the **static library**. `crates/aurix-client/include/aurix_client.h` is the ABI; it
  is generated with cbindgen and drift-checked in CI, so a header from the same commit matches
  the library byte for byte.
* libopus is compiled from source by `audiopus_sys` with the target's C compiler — point
  `CC`/`CFLAGS` (or `CMAKE_TOOLCHAIN_FILE`) at the platform toolchain.
* `worker_threads`: start at 1. It is enough for a voice session; raise it only if the platform
  profiler shows the runtime thread saturating.
* Thread affinity and priorities are set by the host after `aurix_client_create` returns if
  the platform requires it (enumerate the runtime's threads via the platform debugger/thread
  list; they are named `tokio-runtime-worker`).
* Symbol visibility: only `aurix_*` symbols are exported; strip the rest.

## Networking and TLS

**Outbound only.** The client never listens; the server sees the client's public address
from its first packet. NAT traversal needs nothing from the platform beyond an outbound UDP
socket, and when UDP is blocked (or the platform forbids it for your title class) the core
falls back to the WebSocket tunnel automatically (`media_path = auto`) or is pinned to it
(`media_path = tunnel`). A tunnel-only build needs exactly one outbound TCP/TLS connection per
session — often the easiest thing to get past a network review.

**Ports.** The WebSocket is `wss://` on 443 (or whatever your ingress exposes); media UDP goes
to the port the server advertises in `SessionInitAck.media_addrs` (configurable server-side,
one port per node). Dual-stack and IPv6-only fleets are supported
([deployment](../operations/deployment.md#ipv6-and-dual-stack)).

**Where platform networking plugs in.** The C ABI has no socket-injection hook, so a platform
that mandates its own socket layer or TLS stack is a *fork point* rather than a host-side
adapter. The two files to know: `crates/aurix-client/src/media.rs` (UDP socket + tunnel link)
and `crates/aurix-client/src/control.rs` (WebSocket connect, one call into
`tokio_tungstenite::connect_async`). Options, from least to most invasive:

1. Standard sockets are allowed → nothing to do.
2. Platform trust store / pinning mandatory → keep rustls, replace the root store with the
   platform's certificates or a custom `ServerCertVerifier` in `control.rs` (rustls supports
   both; ~50 lines).
3. Platform socket API mandatory → implement `AsyncRead + AsyncWrite` (TCP) and an async
   `recv_from`/`send_to` pair (UDP) over the platform API and swap them in at those two sites.
   The protocol layers above are transport-agnostic.

**Privileges and parental controls.** Check the platform's communication permission for the
signed-in user *before* calling `aurix_client_connect`, and again when the platform notifies a
change; on denial, `aurix_client_disconnect` (or never connect) and hide the voice UI. Aurix's
server-side controls (per-app `voice` scope in tokens, channel `join_token`s, moderation) enforce
*your* policy; they do not know the platform's.

## Audio I/O and the audio thread

The core is push/pull and format-agnostic: hand it whatever the platform microphone delivers
(`f32` or `i16`, mono or the first two channels as L/R, any sample rate) and pull the mix at
the output device's rate. Everything else — resampling, DSP (high-pass, AEC, NS, AGC), VAD,
Opus, jitter buffers, PLC, mixing, panning — is inside.

Rules for the platform audio callback (the same rules Unity/Unreal/Godot follow):

* `aurix_client_push_capture_*`, `aurix_client_mix_output_*`,
  `aurix_client_pull_participant_*` and `aurix_client_push_render_*` are real-time safe
  in steady state (bounded work, no blocking I/O, lock-free or short `parking_lot` sections),
  and may be called from the audio callback. Everything else (`connect`, `join_channel`,
  event polling, setters) belongs on a game/worker thread.
* Do **not** call `aurix_client_destroy` while an audio callback may still be inside the client;
  stop the device first.
* Feed the final rendered game audio to `aurix_client_push_render_*` if you want the acoustic
  echo canceller to work with TV speakers; with a headset you can skip it and set
  `AurixDspConfig::echo_cancellation = false` (`aurix_dsp_config_bypass` disables the whole chain).
* Headset plug/unplug and mic permission changes are platform events: stop/start feeding, call
  `aurix_client_reset_capture` after a device switch so the encoder/VAD state does not carry
  over, and drive `set_muted` from the platform's hardware mute if there is one.
* If the platform provides its own hardware-accelerated Opus encoder, `aurix_client_send_opus`
  accepts already encoded 20 ms frames plus the RFC 6464 level byte; the core then skips its
  own DSP/encoder for that path.

## Suspend, resume and constrained modes

Consoles suspend titles (rest mode, quick resume, system overlay) with sockets torn down
underneath them. Map platform lifecycle events to the core as follows:

| Platform event | Do |
|---|---|
| Suspend / constrained (no network) | Stop the audio device (or keep pushing silence); leave the client alone — it detects the dead link via heartbeats. |
| Resume within the server's `session_resume_grace_secs` (default 30 s) | Nothing: `auto_reconnect` resumes the same session, channels, mute/volume/focus state; you get `Recovering` → `Recovered(resumed = true)`. Restart the audio device. |
| Resume later, or after a network change to another region | The core reconnects with a fresh session (`Recovered(resumed = false)`) and re-joins channels itself; failover endpoints from the last `SessionInitAck` are tried in order. If the bearer token expired meanwhile, `aurix_client_set_token` with a fresh one from your backend before the next attempt (listen for `RequestFailed`/`Failed` with an auth code). |
| User signs out / profile switch | `aurix_client_disconnect` + destroy; the next user gets a new client with their own token. |

`reconnect_max_attempts = 0` means never give up — keep it bounded (default 10) on consoles and
show a "voice unavailable" state on `FailedToRecover`.

## Certification topics → Aurix features

The platforms word these differently; the mapping is stable.

| Requirement (typical wording) | Aurix side | Your side |
|---|---|---|
| Respect communication restrictions (age, parental, privacy settings) | — | gate `connect` and joining voice channels on the platform check; disconnect on change |
| Players can mute/block other players, persistently | `aurix_client_set_participant_mute` (local, per channel or everywhere), `aurix_client_set_user_block` (server-side, persists across sessions and titles on the same app) | mirror the **platform** block list into `set_user_block` at connect; expose both in UI |
| Players can report abuse with evidence | `POST /v1/me/reports` (player token); [content safety](../features/safety.md) incidents with audio evidence clips; server-side recording with per-participant consent (`AURIX_EVENT_RECORDING` + `aurix_client_respond_recording_consent`) | the report UI; forward to the platform's reporting API where required |
| Visible indicator when a player's voice is transmitted / received | `LocalSpeaking`, `ParticipantSpeaking`, `is_speaking`, `input_energy` | the icon |
| Speech-to-text / text-to-speech accessibility for voice chat | `aurix_client_set_transcripts` + `Transcript` events; `aurix_client_speak` (TTS into the channel); live [translation](../features/speech.md) | transcription display; route platform-level TTS preferences |
| No voice capture without user awareness / hardware mute | the core only encodes what you push; `set_muted` stops the uplink instantly (encoder state kept) | wire the platform mic-mute state; stop pushing when the platform says the mic is unavailable |
| Handle network loss / suspend gracefully | heartbeat-based link detection, resume with grace period, failover endpoints | the lifecycle mapping above |
| Data handling / retention / deletion (privacy reviews) | tenant isolation, [retention policies](../features/moderation.md), `DELETE /v1/users/{id}` cascade + export, encrypted recordings | your privacy documentation; run deletion when the platform account is unlinked |
| Only platform-approved network stacks / TLS | fork points in `control.rs` / `media.rs` | the implementation, under NDA |

## The handoff boundary

Everything Aurix maintains is above this line; everything below is per-platform and yours.

```
 ┌─────────────────────────────────────────────────────────────────────┐
 │  Game code / engine layer (Godot, Unreal, Unity, custom)            │  open source, in this repo
 │  aurix-client C ABI + wrapper (protocol, crypto, codecs, DSP, mix)  │
 ├─────────────────────────────────────────────────────────────────────┤  ← handoff boundary
 │  Rust target + C toolchain for the console (vendor)                 │
 │  Platform layer (yours):                                            │  NDA, your repository
 │    identity   platform user → your backend → Aurix JWT              │
 │    privilege  communication permission checks, block-list mirror    │
 │    audio      mic/headset capture → push_capture; mix → device      │
 │    lifecycle  suspend/resume/sign-out → connect/disconnect/set_token│
 │    memory     #[global_allocator], thread affinity/priorities       │
 │    network    (only if mandated) socket/TLS swap in control.rs/media.rs │
 │    reporting  platform abuse-report API ↔ POST /v1/me/reports        │
 └─────────────────────────────────────────────────────────────────────┘
```

A realistic scope for a team that already ships the title on that console: the platform layer
is a few hundred lines per platform; the Rust build and any network fork are the long pole and
depend entirely on what the vendor provides. Nothing in the server, the protocol or the
other SDKs changes.

## Steam Deck and other PC-like targets

Steam Deck (Linux x86_64), Windows handhelds and macOS are ordinary PC targets: the CI-built
`aurix-client-<target>` artefacts and the Godot/Unreal/Unity packages run unchanged. Steam Input
/ SteamOS microphone routing is standard ALSA/PipeWire and works through the engine's audio
layer.
