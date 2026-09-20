# Limitations and non-goals

Things Aurix deliberately does not do, and things that are not yet verified. Each item points to
the chapter that explains the boundary.

## Scope decisions

* **No social layer.** No friends/buddy lists, presence beyond "who has a live session / who is
  in this channel", no attachments, threads or reactions. Text chat is a channel/direct message
  with typing indicators; with `chat.persist` a deployment also gets paged history, directed
  messages queued for offline users and read markers, but replay is per user (read-marker
  driven, one live session per user and node), not an exactly-once per-device queue
  ([Text chat](features/chat.md)).
* **No console SDKs.** PlayStation/Xbox/Switch SDKs are under NDA and cannot ship in an
  open-source repository. The native core exposes a C ABI so a console port is an integration
  task, not a protocol one — the [porting guide](sdk/consoles.md) lists what the platform layer
  has to provide (sockets/TLS, threads, audio callbacks, certification hooks) and where the
  NDA boundary sits. Likewise no first-party Flutter / React Native packages: the same C ABI is
  bound from a native plugin ([Flutter and React Native](sdk/mobile-frameworks.md)).
* **Godot: desktop exports only.** The GDExtension links the native core, so it ships for
  Linux/Windows/macOS exports; Godot's Web export cannot carry the native transport (no UDP, no
  raw sockets) and there is no Godot-side WebRTC client — a browser build would have to embed
  the Web SDK through `JavaScriptBridge`, which is not provided. Android/iOS builds of the
  extension are not staged by the scripts yet ([Godot SDK](sdk/godot.md)).
* **No operator web dashboard.** Operations go through the REST API, the `aurix` CLI and the
  Grafana dashboards; a web panel is planned as a separate front end on top of the same API.
* **No SIP/PSTN gateway**, no server-side noise suppression or echo cancellation — that runs
  on the client: browsers via `getUserMedia` constraints, the native core / Unity / Unreal via
  the built-in capture DSP ([Native core](sdk/native.md#capture-dsp-echo-cancellation-noise-suppression-agc)).
  The DSP is a pure-Rust implementation (frequency-domain AEC, RNNoise-derived NS); it has unit
  and ABI coverage but no field tuning on a fleet of real devices yet.
* **No speech or translation models ship with Aurix.** STT/TTS talk to OpenAI-compatible HTTP
  endpoints you host, live translation to a LibreTranslate- or OpenAI-chat-compatible server;
  transcripts and translations are delivered live and never stored server-side
  ([Speech](features/speech.md)). Translation is **caption-first**: the translated text follows
  the original by the STT segment plus the provider's latency (seconds, not milliseconds), the
  spoken translation is a synthesized voice on a channel-level translator SSRC — not the
  speaker's voice — and word timings are dropped from translated segments. Each node
  translates once per (segment, target language); there is no fleet-wide translation cache.
* **Voice effects are native-only.** The pitch shifter / ring modulator / host callback chain
  runs in the native core (Unreal, C ABI); Unity and the Web SDK use the engine's / browser's
  own audio graph for effects.

## Protocol and media

* **Native AURX is not end-to-end encrypted by default.** Payloads are encrypted per hop with
  session keys the server knows — that is what allows mixing, recording, transcripts and
  WebRTC interop. Clients may set the `E2ee` flag on frames they encrypt themselves; such frames
  are forwarded opaquely to native receivers only and never reach browsers, recordings, live
  streams or STT ([Native AURX media](api/aurx.md), [Security](concepts/security.md)).
* **Browsers get a server-side mix plus a bounded number of per-participant tracks.** The
  Web SDK always receives one mixed (stereo) downlink per session; on top of it a node hands out
  at most `media.webrtc_participant_streams` (default 16, hard cap 64) dedicated tracks, each
  forwarding one speaker's Opus frames as-is, which the browser spatializes with Web Audio
  (`PannerNode`, HRTF by default). Speakers beyond that many, and everyone in ambient channels,
  stay in the mix (stereo panning only); the mapping changes hands with a short hold, so a
  speaker you first heard in the mix may move to a dedicated track (and back) mid-sentence.
  Web Audio needs a user gesture (autoplay policy) and a running `AudioContext`; without it the
  SDK falls back to the mixed track. Insertable-stream encryption and native AURX over UDP are
  not available in browsers ([Web SDK](sdk/web.md#per-participant-tracks-and-spatial-audio)).
* **Unity WebGL is a browser client.** `AurixWebGLVoiceClient` reuses the Web SDK through a
  JavaScript bridge, so everything above applies: WebRTC media with a server-mixed stereo
  downlink plus bounded per-participant tracks, the browser's Opus/AEC/NS/AGC, playback through a
  hidden `<audio>` element and the browser's Web Audio HRTF rather than Unity's
  `AudioSource`/mixer/spatializer (no per-participant PCM, no `AurixParticipantAudioSource`;
  positions reach the browser renderer through `UpdatePositionAsync`),
  no PCMU, no `IOpusCodec`/DSP/media-path settings, and audio only after a user gesture (autoplay
  policy). The native `AurixVoiceClient` throws `PlatformNotSupportedException` in WebGL players
  ([Unity WebGL](sdk/unity.md#unity-webgl)).
* **Opus inside; PCMU only as a per-session fallback** for native AURX clients (the node
  transcodes at the edge). No PCMA, no PCMU over WebRTC, no PCMU for `E2ee` frames, no video
  ([codecs](features/channels.md#codecs-opus-and-the-pcmu-fallback)).
* **The TCP fallback for native media is the control WebSocket, not a second transport.** When
  UDP is blocked the SDKs carry AURX packets over the authenticated WebSocket
  ([tunnel](api/aurx.md#tunnel-aurx-over-the-control-websocket)); that inherits TCP head-of-line
  blocking (latency bursts under loss) and the node's per-session downlink queue drops when the
  client's connection stalls. There is no QUIC/HTTP/3 path, no TURN for native clients, and the
  tunnel needs the WebSocket itself to be reachable (`wss://` on 443 is the usual answer).
* **Browsers own their encoder.** The Web SDK can set the bitrate ceiling, FEC, DTX, maximum
  bandwidth and CBR through WebRTC (`fmtp` / `setParameters`); complexity, signal mode, VBR mode
  and expected loss are only controllable in the native, Unity and Unreal SDKs
  ([Web SDK](sdk/web.md#opus-in-the-browser)). Native mono encoders are capped at 300 kbit/s
  (libopus), channel configs at `media.max_bitrate`.
* **The native server mix is per hop, not end-to-end.** `E2ee` frames cannot enter a mix and
  keep arriving as separate streams even in `mixed` downlink mode; a mix costs the node one Opus
  decode per selected speaker plus one stereo encode per mixer, capped at `MAX_MIXERS` (8192)
  per node. `audience.max_speakers` is enforced when a speaker joins — nobody is demoted once
  admitted — and `max_streams` ranks by receiver gains and sender-reported level, not by
  server-side voice analysis ([Large channels](features/channels.md#large-channels-and-audiences)).
* **Cascade trees are region-deep only.** `region_tree` elects one hub per region per channel
  from registry metadata (region, health, address family, `relay_only`) — not from measured RTT
  or link cost — and caps a path at 3 hops (origin → hub → hub → node); there is no multi-level
  tree inside a region and no per-link bandwidth awareness. Hub loss drops cross-region audio
  for the affected channels until the next reconciliation pass (≤ `cascade_discovery_interval_ms`
  plus the health timeout) ([Scaling](operations/scaling.md#topology-mesh-or-region-tree)).
* **Positional audio is server-side attenuation, panning and radius scoping** from
  client-reported positions; the server does no occlusion, reverb or HRTF, and the ambient mix ranks by
  reported loudness only (no server-side voice-activity analysis of the payload). Directional panning applies to native and WebRTC
  downlinks; TTS/echo follow the same routing. Engine-side spatialization (HRTF, occlusion,
  reverb) is available to native / Unity / Unreal clients through per-participant PCM
  pulls — one unpanned decoded stream per talker — and to browsers through per-participant
  WebRTC tracks (Web Audio HRTF, distance/rolloff reproduced from `ChannelJoinAck.positional`;
  no occlusion/reverb, and `OcclusionUpdate` is not applied by the Web SDK) — not for a
  server-mixed downlink, which is one aggregate stream.

## Server behaviour

* **Node-local resources.** Session statistics, live audio streams and live media state live
  on the node that hosts the session; the fleet-wide views are webhooks/SSE, the database and
  metrics ([Scaling](operations/scaling.md)). Failover moves a *session* to another node (same
  id/SSRC, new media key) from its Redis mirror; a per-participant recording file or live
  stream on the old node is finalised at that point and continues on the new node only if
  that node records/streams the channel. Failover needs Redis — without it a resume elsewhere
  is a fresh session ([High availability](operations/high-availability.md)).
* **Usage metering is exact for CCU and minutes, eventual for counters.** CCU, session and
  participant minutes are derived from lifecycle intervals in PostgreSQL and survive node loss;
  media-byte, chat, TTS and STT counters are flushed per node every `usage.flush_interval_secs`
  and a crashed node loses its last interval. Series are 5-minute (application) / hourly
  (channel) buckets, UTC only, with no per-user breakdown; the monthly minute quota is admission
  control at channel join, not a kill switch for members already present
  ([Usage analytics and quotas](operations/usage-analytics.md)).
* **Redis Sentinel yes, Redis Cluster no.** The ownership claim is a multi-key Lua script
  without hash tags and the event bus is classic Pub/Sub; point the fleet at a Sentinel set or
  a managed endpoint with a stable address.
* **Live audio streams are per participant**, not mixed, and are dropped (counted) when the
  consumer falls behind `recording.live.queue_frames`; no buffering across a consumer outage
  ([Recordings and live streams](features/recordings.md)).
* **Recordings are captured per participant** as Ogg/Opus; a channel file is a post-hoc
  mixdown (Ogg/Opus or WAV only — no MP3/AAC/FLAC, no video containers) rendered on the node
  that receives the request, which must reach every source track (local disk or object
  storage). Post-hoc transcripts reuse the live `[stt]` provider and are stored in clear text
  in PostgreSQL ([Recordings](features/recordings.md)).
* **Webhook and SSE delivery is at-least-once** with the same event id — consumers must be
  idempotent; SSE has no replay, only `lagged` + snapshot resync
  ([Webhooks and SSE](api/webhooks-sse.md)).
* **Rate limits are fleet-wide only with Redis.** Without it (single node) or while Redis is down
  with `rate_limiting.fail_closed = false`, buckets are per node and a client hopping between
  nodes can exceed a limit. Chat flood control and TTS queue limits are per session by design
  (sessions never span nodes).
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
* **Godot editor and devices.** The extension is built and exercised headless in CI (API
  smoke test, project import, demo script parse) and against a live node in a two-client
  Godot E2E on Linux; microphone capture through `AudioStreamMicrophone`, `AudioStreamPlayer3D`
  spatialization and the Windows/macOS extension binaries are not run in CI
  ([Godot SDK](sdk/godot.md)).
* **Unity Editor and devices.** The Unity SDK and the sample scene are compiled against a
  UnityEngine stub and exercised through the .NET demo; permission dialogs, audio-route changes
  and background/foreground transitions on real iOS/Android hardware are not exercised in CI
  ([Unity SDK](sdk/unity.md)). `NativeOpusCodec` is tested against the Linux build of the native
  core; loading from `Plugins/` on other platforms follows Unity's P/Invoke rules and is not run here.
* **Unity WebGL player builds.** The WebGL path is verified in pieces — the C# client against a
  scripted bridge, the `.jslib` against the real browser bundle under an Emscripten-like harness,
  the Unity compile check with `UNITY_WEBGL` — but no Unity WebGL player has been built and run
  in a browser from this repository ([Unity WebGL](sdk/unity.md#unity-webgl)).
* **Browser spatial audio is tested against a fake Web Audio graph.** The Web SDK's per-track
  graph (gain → `PannerNode` → master), listener/source placement and mute/block/focus gains are
  unit-tested on a scripted `AudioContext`, and the multi-track SDP negotiation, slot handout,
  pinning and mixed fallback against the real node with a `str0m` browser stand-in; no real
  browser rendered HRTF audio in CI.
* **Browser Opus negotiation.** The Web SDK's `fmtp` rewrite and `setParameters` path are unit-
  tested on SDP text and applied in the E2E browser runs; whether a given browser honours
  `useinbandfec`/`usedtx`/`maxplaybackrate` is up to that browser's WebRTC stack.
* **Windows/macOS native builds** of `aurix-client` are built and unit-tested in CI on
  `windows-latest` (x64 MSVC), `macos-14` (arm64) and `macos-13` (x64) — including the C/C++
  samples linked against the freshly built library and the Unreal `ThirdParty` staging scripts —
  and uploaded as workflow artifacts. What CI does not do is load them from Unity `Plugins/` or
  compile the Unreal module on those hosts, and there is no 32-bit or ARM64 Windows build.
* **Admin SSO against real identity providers.** The OIDC relying party is exercised end to end
  against the repository's mock provider (discovery, PKCE, nonce, JWKS rotation, userinfo,
  role mapping) and follows the OpenID Connect Core rules, but no Keycloak / Entra ID / Okta /
  Google tenant has been wired up in CI — claim names and group formats of your provider are
  the thing to verify first ([Administrator accounts and SSO](operations/admin-sso.md)).

## Planned

The operator web panel is tracked separately.
