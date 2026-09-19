# Limitations and non-goals

Things Aurix deliberately does not do, and things that are not yet verified. Each item points to
the chapter that explains the boundary.

## Scope decisions

* **No social layer.** No friends/buddy lists, presence beyond "who has a live session / who is
  in this channel", no offline direct messages, conversations, read markers or attachments. Text
  chat is a real-time "lite" channel/direct message with typing indicators; history is opt-in
  per deployment ([Text chat](features/chat.md)).
* **No console SDKs.** PlayStation/Xbox/Switch SDKs are under NDA and cannot ship in an
  open-source repository. The native core exposes a C ABI so a console port is an integration
  task, not a protocol one ([Native core](sdk/native.md)).
* **No operator web dashboard.** Operations go through the REST API, the `aurix` CLI and the
  Grafana dashboards; a web panel is planned as a separate front end on top of the same API.
* **No SIP/PSTN gateway**, no server-side noise suppression or echo cancellation — that runs
  on the client: browsers via `getUserMedia` constraints, the native core / Unity / Unreal via
  the built-in capture DSP ([Native core](sdk/native.md#capture-dsp-echo-cancellation-noise-suppression-agc)).
  The DSP is a pure-Rust implementation (frequency-domain AEC, RNNoise-derived NS); it has unit
  and ABI coverage but no field tuning on a fleet of real devices yet.
* **No speech models ship with Aurix.** STT/TTS talk to OpenAI-compatible HTTP endpoints you
  host; transcripts are delivered live and never stored server-side ([Speech](features/speech.md)).

## Protocol and media

* **Native AURX is not end-to-end encrypted by default.** Payloads are encrypted per hop with
  session keys the server knows — that is what allows mixing, recording, transcripts and
  WebRTC interop. Clients may set the `E2ee` flag on frames they encrypt themselves; such frames
  are forwarded opaquely to native receivers only and never reach browsers, recordings, live
  streams or STT ([Native AURX media](api/aurx.md), [Security](concepts/security.md)).
* **Browsers get a server-side mix.** The Web SDK receives one mixed (stereo) downlink per
  session; per-participant tracks, insertable-stream encryption and native AURX over UDP are not
  available in browsers ([Web SDK](sdk/web.md)).
* **Opus inside; PCMU only as a per-session fallback** for native AURX clients (the node
  transcodes at the edge). No PCMA, no PCMU over WebRTC, no PCMU for `E2ee` frames, no video
  ([codecs](features/channels.md#codecs-opus-and-the-pcmu-fallback)).
* **Browsers own their encoder.** The Web SDK can set the bitrate ceiling, FEC, DTX, maximum
  bandwidth and CBR through WebRTC (`fmtp` / `setParameters`); complexity, signal mode, VBR mode
  and expected loss are only controllable in the native, Unity and Unreal SDKs
  ([Web SDK](sdk/web.md#opus-in-the-browser)). Native mono encoders are capped at 300 kbit/s
  (libopus), channel configs at `media.max_bitrate`.
* **Cascade is a one-hop mesh** between the nodes that host a channel — no relay trees; nodes
  must reach each other directly on `media.port + 1`/UDP ([Scaling](operations/scaling.md)).
* **Positional audio is server-side attenuation, panning and radius scoping** from
  client-reported positions; there is no occlusion, reverb or HRTF, and the ambient mix ranks by
  reported loudness only (no server-side voice-activity analysis of the payload). Directional panning applies to native and WebRTC
  downlinks; TTS/echo follow the same routing.

## Server behaviour

* **Node-local resources.** Session statistics, live audio streams and WebSocket resume state
  live on the node that hosts the session; the fleet-wide views are webhooks/SSE, the database
  and metrics ([Scaling](operations/scaling.md)).
* **Live audio streams are per participant**, not mixed, and are dropped (counted) when the
  consumer falls behind `recording.live.queue_frames`; no buffering across a consumer outage
  ([Recordings and live streams](features/recordings.md)).
* **Recordings are per participant** Ogg/Opus files; no server-side mixdown, no transcoding
  ([Recordings](features/recordings.md)).
* **Webhook and SSE delivery is at-least-once** with the same event id — consumers must be
  idempotent; SSE has no replay, only `lagged` + snapshot resync
  ([Webhooks and SSE](api/webhooks-sse.md)).
* **Rate limits and quotas are per node** except where backed by Redis (action tokens,
  bans, session registry); a client hopping between nodes can exceed a per-node limit.
* **TLS**: native rustls with PEM files, or terminate at your proxy; ACME/auto-renewal is left to
  the proxy ([Deployment](operations/deployment.md)).
* **Region discovery is per node, not per player location service**: nodes are placed by
  `server.region` / `server.location`, and the server orders by preference, distance to the
  coordinates the client sends, and load; the client-side RTT probe is what actually picks the
  nearest node. Discovery hands out a node's own `wss://` URL, so every node needs a public
  hostname and certificate ([Regions](operations/scaling.md#regions)).

## Not verified in this repository's CI

* **Helm chart and Terraform example** are linted, rendered, schema-validated (`kubeconform`)
  and `terraform validate`d in CI, but not applied against a live cluster or AWS account.
* **Unreal plugin compile.** The `AurixVoice` plugin is checked against the C ABI header and
  the native library builds on Linux, but Unreal Header Tool and a real engine compile have not
  run — the first build in your project is the verification step ([Native core and
  Unreal](sdk/native.md)).
* **Unity Editor and devices.** The Unity SDK and the sample scene are compiled against a
  UnityEngine stub and exercised through the .NET demo; permission dialogs, audio-route changes
  and background/foreground transitions on real iOS/Android hardware are not exercised in CI
  ([Unity SDK](sdk/unity.md)). `NativeOpusCodec` is tested against the Linux build of the native
  core; loading from `Plugins/` on other platforms follows Unity's P/Invoke rules and is not run here.
* **Browser Opus negotiation.** The Web SDK's `fmtp` rewrite and `setParameters` path are unit-
  tested on SDP text and applied in the E2E browser runs; whether a given browser honours
  `useinbandfec`/`usedtx`/`maxplaybackrate` is up to that browser's WebRTC stack.
* **Windows/macOS native builds** of `aurix-client` are scripted (`build_native.ps1`) but only
  the Linux build runs in CI.

## Planned

The operator web panel is tracked separately. A TCP fallback for native AURX media,
per-receiver stream caps / audience mode for very large channels and cross-node session
failover are the next backlog items.
