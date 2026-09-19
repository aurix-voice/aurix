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
* **No SIP/PSTN gateway**, no server-side noise suppression or echo cancellation (clients do
  that: browsers via `getUserMedia` constraints, Unity/Unreal via their audio stacks).
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
* **Opus only.** No PCMU/PCMA fallback, no video.
* **Cascade is a one-hop mesh** between the nodes that host a channel — no relay trees; nodes
  must reach each other directly on `media.port + 1`/UDP ([Scaling](operations/scaling.md)).
* **Positional audio is server-side attenuation and panning** from client-reported positions;
  there is no occlusion, reverb or HRTF. Directional panning applies to native and WebRTC
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

## Not verified in this repository's CI

* **Unreal plugin compile.** The `AurixVoice` plugin is checked against the C ABI header and
  the native library builds on Linux, but Unreal Header Tool and a real engine compile have not
  run — the first build in your project is the verification step ([Native core and
  Unreal](sdk/native.md)).
* **Unity Editor and devices.** The Unity SDK and the sample scene are compiled against a
  UnityEngine stub and exercised through the .NET demo; permission dialogs, audio-route changes
  and background/foreground transitions on real iOS/Android hardware are not exercised in CI
  ([Unity SDK](sdk/unity.md)).
* **Windows/macOS native builds** of `aurix-client` are scripted (`build_native.ps1`) but only
  the Linux build runs in CI.

## Planned

Helm chart and Terraform example, geographic node selection for players, Opus
complexity/bandwidth controls with an optional native libopus binding, safety adapters
(STT → toxicity, evidence export), PCMU fallback, and ambient/radius visibility. The operator web
panel is tracked separately.
