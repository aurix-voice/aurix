# Limitations and non-goals

Things Aurix deliberately does not do, and things that are not yet verified. Each item points to
the chapter that explains the boundary.

## Scope decisions

* **No social layer.** No friends/buddy lists, presence beyond "who has a live session / who is
  in this channel", no attachments or threads. Text chat is a channel/direct message with
  typing indicators; with `chat.persist` a deployment also gets paged history, directed
  messages queued for offline users, read markers, edits / tombstone deletions, reactions and
  PostgreSQL full-text search (exact words, no stemming), but replay is per user (read-marker
  driven, one live session per user and node), not an exactly-once per-device queue
  ([Text chat](features/chat.md)).
* **No console SDKs.** PlayStation/Xbox/Switch SDKs are under NDA and cannot ship in an
  open-source repository. The native core exposes a C ABI so a console port is an integration
  task, not a protocol one — the [porting guide](sdk/consoles.md) lists what the platform layer
  has to provide (sockets/TLS, threads, audio callbacks, certification hooks) and where the
  NDA boundary sits. Likewise no first-party Flutter / React Native packages: the same C ABI is
  bound from a native plugin ([Flutter and React Native](sdk/mobile-frameworks.md)).
* **Godot Web is a different node.** The GDExtension links the native core and ships for
  desktop (and, staged, mobile) exports; a Web export cannot carry the native transport, so it
  uses `AurixWebVoiceClient` — GDScript over `JavaScriptBridge` and the Web SDK — with the same
  signals and dictionaries but browser-owned audio: no `AudioStreamGenerator`/
  `AurixParticipantPlayer`, no engine-side 3D, no native DSP knobs, autoplay gating
  (`resume_audio()`), WebRTC only ([Godot SDK](sdk/godot.md#web-export-aurixwebvoiceclient)).
* **Godot Android/iOS are staged, not built.** `build_native.sh` knows the cargo-ndk / Xcode
  recipes and the `.gdextension` lists the slices, but this CI has no NDK or Xcode: nothing
  mobile is compiled, exported or run on a device here ([Godot SDK](sdk/godot.md#android-and-ios)).
* **The operator dashboard is a client of the API, not a control plane of its own.** It is a
  separate Caddy-served SPA (`dashboard/`, image `aurix-dashboard`) over the same admin routes:
  configuration is read-only (no runtime mutation API exists by design), drain / undrain is the
  only node action, administrators are fleet-global (application scope is a filter, not a
  tenancy boundary), and nobody can listen to audio from it. It is verified by Playwright
  against a live node and the built image in CI, not yet by an operator pilot; the supported
  pairing is dashboard and node of the same minor version
  ([Operator dashboard](operations/dashboard.md)).
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
* **Voice effects are microphone-only and pre-encode.** The effects chain (filters, formant,
  pitch, ring modulation, distortion, tremolo, static, reverb) runs on the sender's uplink in
  the native core (C ABI, Unreal, Godot, Unity native players) or in a browser `AudioWorklet`
  (Web SDK, Unity WebGL). Nothing is applied on the node or on the receiver: injected audio,
  TTS, translations and the downlink are untouched, everyone hears the same effected voice,
  and there is no per-listener "hear X as a robot". The two implementations share parameters
  and presets but are separate ports (Rust and TypeScript), tuned by ear, not by measurement
  ([Voice effects](features/speech.md#voice-effects)).
* **Lip-sync is a receiver-side heuristic, not phoneme recognition.** Visemes are derived from
  the spectrum of decoded audio (level, voiced/fricative split, two formants → nearest of five
  vowels), locally, one frame per 20 ms; there is no speech model, no language awareness, no
  network transport of mouth shapes and no server-side analysis. Browsers analyse only
  participants on a dedicated per-participant track (the mixed track cannot be split); Unity
  native players need the native library (`SupportsVisemes` / `SupportsVoiceEffects` are
  `false` without it) ([Visemes](features/speech.md#visemes-lip-sync)).
* **Priority-speaker ducking is one envelope per channel, driven by speech.** It engages on
  a priority member's audible frames (level byte, or speaking state for unlabelled frames)
  with the channel's attack / hold / release, for every receiver on the node alike: a
  receiver who muted or blocked the priority speaker is still ducked while they talk (the
  mute removes that voice, the duck attenuates the others), and a priority speaker on another
  node ducks through the cascaded frames. Browsers reproduce the envelope on dedicated tracks
  from `SpeakingStateChanged` — up to `media.speaking_timeout_ms` behind the node's own gain
  on the mix. Game-audio ducking is an event plus a helper (`AurixGameAudioDucker`), not an
  engine mixer integration
  ([Priority speakers and ducking](features/channels.md#priority-speakers-and-ducking)).

## Protocol and media

* **End-to-end encryption is per channel and costs the server-side features.** By default
  payloads are encrypted per hop with session keys the server knows — that is what allows
  mixing, recording, transcripts and translation. A channel with `e2ee: true` seals frames with
  sender keys the node never holds and therefore has no server mix, recording, live stream,
  STT/translation/safety, TTS injection or PCMU transcoding; every listener receives one stream
  per speaker (top-N caps still apply), browsers hear encrypted members only on dedicated
  per-participant tracks, and one WebRTC session cannot be in encrypted and plaintext channels
  at once. Identity keys are authenticated by the node, so protection against a *malicious
  operator* relies on comparing fingerprints out of band ([End-to-end
  encryption](features/e2ee.md), [Security](concepts/security.md)).
* **Browsers get a server-side mix plus a bounded number of per-participant tracks.** The
  Web SDK always receives one mixed (stereo) downlink per session; on top of it a node hands out
  at most `media.webrtc_participant_streams` (default 16, hard cap 64) dedicated tracks, each
  forwarding one speaker's Opus frames as-is, which the browser spatializes with Web Audio
  (`PannerNode`, HRTF by default). Speakers beyond that many, and everyone in ambient channels,
  stay in the mix (stereo panning only); the mapping changes hands with a short hold, so a
  speaker you first heard in the mix may move to a dedicated track (and back) mid-sentence.
  Web Audio needs a user gesture (autoplay policy) and a running `AudioContext`; without it the
  SDK falls back to the mixed track. Native AURX over UDP is not available in browsers
  ([Web SDK](sdk/web.md#per-participant-tracks-and-spatial-audio)).
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
  client's connection stalls. The [QUIC path](api/aurx.md#quic-aurx-datagrams-with-0-rtt-resume-and-connection-migration)
  is a UDP path too — it shares the media port and is blocked by the same firewalls, so it does
  not help there; there is no HTTP/3, no TURN for native clients, and the tunnel needs the
  WebSocket itself to be reachable (`wss://` on 443 is the usual answer).
* **QUIC is native-only and datagram-only.** Browsers keep WebRTC (no WebTransport), the Unity
  C# transport keeps UDP/tunnel (QUIC reaches Unity only through the native core), and QUIC
  streams are disabled — it moves the same AURX packets, nothing else. Connection migration
  needs the game to call `network_changed()` (or a NAT rebind); the core does not watch OS
  network interfaces itself. Resumption state lives in the node process: the first connection
  after a node restart or a cross-node failover is a full 1-RTT handshake, 0-RTT applies to
  reconnects to a node the client already talked to.
* **Browsers own their encoder.** The Web SDK can set the bitrate ceiling, FEC, DTX, maximum
  bandwidth and CBR through WebRTC (`fmtp` / `setParameters`); complexity, signal mode, VBR mode
  and expected loss are only controllable in the native, Unity and Unreal SDKs
  ([Web SDK](sdk/web.md#opus-in-the-browser)). Native mono encoders are capped at 300 kbit/s
  (libopus), channel configs at `media.max_bitrate`.
* **Loss repair is bounded by what the wire carries.** In-band FEC covers one frame back and
  only when the sender's encoder had it on; DRED covers as much history as libopus fits into
  the bitrate (none below ≈ 28 kbit/s, ≈ 100–150 ms at 28–40 kbit/s — a requested
  `dred_duration_ms` is a ceiling, not a guarantee), the receiver sees at most 52 frames back
  and the server mixers 4; everything else is neural PLC, which fades to silence over a long
  gap. Browsers get FEC and their own PLC only — no DRED, no loss profile
  (the browser owns its codec). The forwarded per-sender sequence keeps uplink gaps of up to
  50 frames when the packet sequence and the timestamp clock agree; a loss that coincides with a
  pause or with heartbeats is under-counted, never over-counted
  ([Packet loss](sdk/native.md#packet-loss-fec-dred-and-the-neural-plc)).
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
* **Voice quality is a network model, not a listening test.** MOS is the simplified E-model
  (RTT, jitter, loss) — it does not hear the audio, so clipping, a muted microphone, DSP
  artefacts or a starved encoder do not lower it, and codec differences (Opus vs PCMU) are not
  modelled. Ratings are per `media.quality_interval_ms`, so a session's history has that
  resolution and a node crash loses the samples since its last `quality.persist_interval_secs`
  checkpoint; the MOS alert state machine restarts on cross-node takeover (one duplicate alert
  possible). Prometheus carries distributions only — no per-session or per-user series; the
  fleet alert rules assume enough concurrent sessions to make percentiles meaningful and
  need tuning below a few dozen. Quality aggregates exist per application, not per channel
  ([Network quality](features/quality.md)).
* **Redis Cluster needs Redis 7 for sharded Pub/Sub.** `redis.cluster` works with Redis 7+
  (`SSUBSCRIBE`, RESP3); on Redis 6 clusters or RESP2-only proxies set
  `redis.sharded_pubsub = false` and the event bus falls back to classic Pub/Sub, which the
  cluster broadcasts to every node. Cluster mode is a compatibility feature, not a latency one;
  no benchmark claims a faster voice path with it.
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
* **Server SDKs are generated, unpublished, and REST-only.** The Node, Python, Go and C#
  clients are emitted from `api/openapi.json` and cannot drift from it, but they are not
  published to npm / PyPI / NuGet / pkg.go.dev from this repository (path dependencies or your
  own registry), and webhook / SSE payloads are typed only as the contract types them
  (`EventEnvelope { type, data }`). There are no Java / PHP / Ruby clients. The token servers
  are examples of the credential boundary — `/dev/login` is a development stand-in for your
  game's login, not an authentication system — and ship without TLS termination, client-side
  rate limits or metrics. The `aurix` CLI is a REST client with an embedded copy of the
  contract: it never joins a channel or sends audio, and against a node of another version
  `aurix diagnose` lists the operations the two sides disagree on
  ([Server SDKs and token servers](backend/server-sdks.md), [The aurix CLI](backend/cli.md)).

## Not verified in this repository's CI

* **Helm chart and Terraform example** are linted, rendered, schema-validated (`kubeconform`)
  and `terraform validate`d in CI, but not applied against a live cluster or AWS account.
* **Unreal plugin compile.** The `AurixVoice` plugin (and the `AurixVoiceSamples` Blueprint
  components) is checked against the C ABI header, for UHT/packaging conventions
  (`sdk/unreal/scripts/check_plugin.py`) and the native library builds on Linux, Windows and
  macOS, but Unreal Header Tool, a real engine compile and `RunUAT BuildPlugin` have not run —
  the CI job only runs `BuildPlugin` in Epic's container when an Epic-linked GHCR token is
  configured, and this repository has none. The first build in your project is the
  verification step ([Native core and Unreal](sdk/native.md)).
* **Godot editor and devices.** The extension is built and exercised headless in CI (API
  smoke test, project import, demo script parse) and against a live node in a two-client
  Godot E2E on Linux; the Web export runs in headless Chromium against a live node (boot, SDK
  glue, join/roster/speaking/chat/quality). Microphone capture through
  `AudioStreamMicrophone`, `AudioStreamPlayer3D` spatialization, the Windows/macOS extension
  binaries, real microphones/headphones in the Web export and Firefox/Safari are not run in CI
  ([Godot SDK](sdk/godot.md)).
* **Unity Editor and devices.** The Unity SDK, the Editor menu, the NUnit tests and the sample
  scenes are compiled against a UnityEngine stub and exercised through the .NET demo; the Unity
  Test Runner, package import through the Package Manager, permission dialogs, audio-route changes
  and background/foreground transitions on real iOS/Android hardware are not exercised in CI
  ([Unity SDK](sdk/unity.md)). `NativeOpusCodec` is tested against the Linux build of the native
  core; loading from `Plugins/` on other platforms follows Unity's P/Invoke rules and is not run here.
* **Unity WebGL player builds.** The WebGL path is verified in pieces — the C# client against a
  scripted bridge, the real `.jslib` and the sample's WebGL template against the real browser
  bundle under an Emscripten stand-in (Node, and Chromium against a live node in CI), the Unity
  compile check with `UNITY_WEBGL` — but the stand-in is not Unity's Emscripten runtime and no
  Unity-built WebGL player has been run in a browser from this repository
  ([Unity WebGL](sdk/unity.md#unity-webgl)).
* **Browser spatial audio is tested against a fake Web Audio graph.** The Web SDK's per-track
  graph (gain → `PannerNode` → master), listener/source placement and mute/block/focus gains are
  unit-tested on a scripted `AudioContext`, and the multi-track SDP negotiation, slot handout,
  pinning and mixed fallback against the real node with a `str0m` browser stand-in; no real
  browser rendered HRTF audio in CI.
* **Browser Opus negotiation.** The Web SDK's `fmtp` rewrite and `setParameters` path are unit-
  tested on SDP text and applied in the E2E browser runs; whether a given browser honours
  `useinbandfec`/`usedtx`/`maxplaybackrate` is up to that browser's WebRTC stack.
* **Browser E2EE outside Chromium.** The WebCrypto cipher and both encoded-frame transforms
  (`RTCRtpScriptTransform` worker, `createEncodedStreams()`) are unit-tested against a fake
  WebRTC stack and share vectors with the Rust and C# implementations; the `'streams'` path was
  verified live in Chrome against real nodes (browser ↔ browser, browser ↔ native). Firefox and
  Safari — the `RTCRtpScriptTransform` path in a real engine — have not been run from this
  repository ([End-to-end encryption](features/e2ee.md#browser-support)).
* **Windows/macOS native builds** of `aurix-client` are built and unit-tested in CI on
  `windows-latest` (x64 MSVC), `macos-14` (arm64) and `macos-15-intel` (x64) — including the C/C++
  samples linked against the freshly built library and the Unreal `ThirdParty` staging scripts —
  and uploaded as workflow artifacts. What CI does not do is load them from Unity `Plugins/` or
  compile the Unreal module on those hosts, and there is no 32-bit or ARM64 Windows build.
* **Loss repair is measured, not listened to.** FEC / DRED / PLC recovery is verified with
  synthetic speech-like signals (sample-domain correlation against the original, frame counts
  by method, stereo, reordering, partial DRED coverage, PCMU bypass) on Linux, in the
  server-mixer loss simulation and against a live node behind Linux netem (20 % downlink loss,
  12 % uplink loss with jitter and reordering: profile transitions, recovery share, MOS —
  [Development](operations/development.md#lossy-wan-and-network-migration)). That is an emulated
  link on loopback: no listening test, no real access network (Wi-Fi contention, bufferbloat,
  cellular schedulers) and no measurement of OSCE's effect on perceived quality has been run
  from this repository. `osce_bwe` depends on the libopus build and reads back `false` where it
  is not compiled in.
* **QUIC is exercised on one host.** Bind, authenticated media both ways, 0-RTT resume, stale
  connection refusal, early-data replay, wrong pin / wrong key, connection cap, the disabled-QUIC
  node, socket migration through `network_changed()`, fallback to the tunnel and back, IPv4 and
  IPv6 loopback are all tested in-process and against live nodes — on Linux loopback, the
  migration also under netem delay/jitter/loss. No real Wi-Fi ↔ cellular hand-over (a new radio
  path, NAT rebinding, a short UDP timeout), no measurement of head-of-line gains against the
  tunnel have been run from this repository
  ([QUIC](api/aurx.md#quic-aurx-datagrams-with-0-rtt-resume-and-connection-migration)).
* **Admin SSO against real identity providers.** The OIDC relying party is exercised end to end
  against the repository's mock provider (discovery, PKCE, nonce, JWKS rotation, userinfo,
  role mapping) and follows the OpenID Connect Core rules, but no Keycloak / Entra ID / Okta /
  Google tenant has been wired up in CI — claim names and group formats of your provider are
  the thing to verify first ([Administrator accounts and SSO](operations/admin-sso.md)).
* **Chaos runs on one host.** The `tools/chaos/` harness kills a node, fails Redis over through
  Sentinel (or kills a Redis Cluster shard master), stops and starts PostgreSQL and checks isolation — but everything (both nodes, the
  Sentinels, PostgreSQL) shares one machine and one loopback network. Network partitions between
  hosts, split-brain Sentinel quorums across data centres, PostgreSQL replica promotion and
  cross-region failover latency are not exercised ([High availability](operations/high-availability.md)).
* **Fuzzing is short in CI.** The committed corpus replays on every push and each libFuzzer
  target runs for 30 s with ASan on nightly; longer campaigns (minutes to hours per target, as
  in the runs that found the RTP and STUN regressions in `fuzz/corpus/`) are run out of band
  and not on a schedule ([Threat model](concepts/threat-model.md)).
* **The release workflow has not produced a public release yet.** `release.yml` is validated
  with `actionlint`, its version/changelog checks run locally, and each build step mirrors a
  CI job that does run — but no tag has been pushed through it, so the GHCR push, Sigstore
  signing, attestations and the GitHub release itself are exercised only on the first tag
  ([Releases](operations/releases.md)).

## Planned

Console SDKs (PlayStation / Xbox / Switch) beyond the porting guide; a pilot on real devices and
networks.
