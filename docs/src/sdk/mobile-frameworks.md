# Flutter and React Native

Aurix has no first-party Flutter or React Native package. Both frameworks can nevertheless use
the full native feature set — AURX media over UDP with the WebSocket tunnel fallback, Opus,
DSP, per-participant audio, reconnect/resume, failover — by binding the same stable C ABI the
Godot, Unreal and Unity (mobile) SDKs use. This page says how to do that correctly and what
**not** to do.

**Do not embed the Godot GDExtension** (or the Unreal plugin) in a Flutter or React Native
app. Those wrappers are thin engine-specific layers: they depend on the engine's audio server,
main-loop `_process` pumping and object model. Nothing in them is reusable outside the engine,
and nothing in them is needed — the C ABI they call is the public contract.

```
 Dart / JS UI ──► framework native plugin ──► aurix_client.h (C ABI) ──► aurix-client
                  (Kotlin/Swift/C++)           libaurix_client.{so,a,dylib}
                  audio device I/O
                  event pump → UI thread
```

## What every binding has to provide

The core is push/pull and event-queue based (see [Native core](native.md)), so a binding is
four things, none of them protocol work:

1. **Library + header.** Build `aurix-client` for `aarch64-linux-android` / `armv7`, `x86_64`
   (Android) and `aarch64-apple-ios` (+ simulator) — the same targets and
   `cargo build -p aurix-client --release --target …` the Unity mobile package uses
   ([Unity SDK](unity.md#ios--android-notes)); ship a static library on iOS and a
   `.so` per ABI on Android. `include/aurix_client.h` is the ABI; it is drift-checked in CI.
2. **Audio device I/O in native code**, never in Dart/JS: AAudio/Oboe or `AudioRecord`/
   `AudioTrack` on Android, `AVAudioEngine`/`AudioUnit` (voice-processing IO) on iOS. The mic
   callback calls `aurix_client_push_capture_f32/i16`, the render callback
   `aurix_client_mix_output_f32/i16`. Any sample rate and mono/stereo are accepted; both calls
   are real-time safe. Never route PCM through the Dart or JS bridge — the copies and GC pauses
   destroy 20 ms timing.
3. **An event pump to the UI thread.** `aurix_client_poll_event` from a timer or, better,
   `aurix_client_set_wake_callback` → post to the UI thread → drain `poll_event`, converting each
   `AurixEvent` (or just `aurix_event_json`) into a framework event; then `aurix_event_free`.
   Copy everything you need out of the event before freeing it.
4. **Lifecycle glue**: app background/foreground, audio-session interruptions, route changes
   (headset/Bluetooth), permissions.

Tokens come from **your** backend (`POST /v1/tokens` with the app's API key, server-side only);
the app never sees an API key. Region choice: `aurix_regions_*` parses `GET /v1/me/regions`,
takes your RTT probes and ranks endpoints.

## Flutter

* **Package shape:** a federated plugin — a Dart API package plus `aurix_voice_android` /
  `aurix_voice_ios` platform packages. The Dart API exposes a `Stream<AurixEvent>` and async
  methods; the platform packages own the audio device and the client handle.
* **Calling the ABI:** `dart:ffi` with `ffigen` over `aurix_client.h` gives you every function
  and struct at zero marshaling cost; Flutter's `NativeCallable.listener` lets the wake callback
  post to the Dart isolate. Use FFI for control calls and event polling. Keep **audio** in the
  platform package (Kotlin/Swift or C++ via `Oboe`/`AudioUnit`) — Dart isolates are not
  real-time threads.
* **Memory:** `AurixEvent*` pointers are owned by the caller until `aurix_event_free`; wrap them
  in a `NativeFinalizer` *and* free eagerly after conversion. Strings returned by the ABI are
  borrowed: event strings (`aurix_event_message`, …) are owned by the event and die with it,
  `aurix_last_error` is per thread and valid until the next failing call — convert to Dart
  `String` immediately.
* **Lifecycle:** on `AppLifecycleState.paused` keep the client connected if the OS lets your
  audio session run (VoIP/background-audio entitlement on iOS, a foreground service with
  `microphone` type on Android); otherwise mute and stop the devices and let heartbeat-based
  link detection plus resume (server grace period, default 30 s) bring the session back on
  resume. `AVAudioSession` interruptions and route changes → stop/start the units and call
  `aurix_client_reset_capture` after a device switch.
* **Isolates / hot reload:** the native client survives hot reload; keep the handle in a
  singleton and re-attach the event stream rather than creating a second client (one live
  session per user per node is the server rule).

## React Native

* **Package shape:** a TurboModule (New Architecture) with a C++ core shared by Android and iOS,
  or a JSI host object. Bridge-based `NativeModules` work too but add a serialisation hop per
  event; avoid them for anything above a few events per second (speaking/energy notifications
  can be 10–50 Hz in a busy channel — coalesce them per frame, or subscribe only when the UI
  shows them).
* **Calling the ABI:** from the C++ TurboModule include `aurix_client.hpp` (the header-only
  C++11 wrapper) — you get RAII handles and `std::string` conversions for free and stay on one
  language for both platforms. Expose coarse methods (`connect`, `join`, `mute`, `setVolume`,
  …) and an `addListener` that delivers already-converted plain objects.
* **Event marshaling:** convert on the native side into a JS-safe shape (UUIDs as strings,
  numbers as `double`, no pointers, no `BigInt`), emit through `jsi::Function` calls scheduled
  on the JS thread via `CallInvoker`. Never hand `AurixEvent*` or raw buffers to JS.
* **Audio:** stays in native code exactly as for Flutter. If you need level meters in JS, poll
  `aurix_client_input_energy` from the TurboModule at ≤ 20 Hz rather than streaming PCM.
* **Tokens:** fetch from your backend over your existing authenticated HTTP client; pass the
  string to `connect`. Refresh with `aurix_client_set_token` before it expires (the JWT's `exp`
  is visible client-side) so reconnects after background use a valid token.
* **Lifecycle:** `AppState` `background`/`active` mirrors the Flutter guidance; Android needs a
  foreground service to keep capturing, iOS needs the `audio`/`voip` background mode.

## What you get without writing protocol code

Everything in the [native core](native.md) — encrypted AURX media with tunnel fallback,
resume/failover, per-participant PCM for custom spatialisation, DSP, VAD, stereo/music
uplinks, chat with history and read markers, transcripts/TTS/translation, moderation, network
quality — surfaces through the ~150 `aurix_*` functions. The binding decides how much of it to
expose. What stays out of reach is the browser path (WebRTC via the [Web SDK](web.md)) — that
is for web views, not for native apps.
