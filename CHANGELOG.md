# Changelog

All notable changes to Aurix are recorded here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/); versions follow
[Semantic Versioning](https://semver.org/) as described in
[docs/src/operations/releases.md](docs/src/operations/releases.md).

The server, the client SDKs (native core / C ABI, Web, Unity, Unreal, Godot), the server SDKs
(Node, Python, Go, C#), the CLI and the Helm chart `appVersion` share one version number and are
released together.

## [Unreleased]

### Added

* **`aurix doctor`** — local preflight on the node host, no credentials needed: loads the
  configuration exactly like the node (`configs/default` + file + `AURIX__*`), runs the node's
  validation plus a production-mode preview, checks every configured TCP/UDP listener (free
  before start, all bound while a node is running — detected via `/health`), parses the
  API/media/WebTransport certificate pairs (chain, key, permissions, expiry, SAN vs
  `quic_server_name`, ≤ 14-day WebTransport validity, SHA-256 pin), handshakes QUIC, the TLS
  tunnel and WebTransport against a running node and compares the *served* certificate with
  the configured one, opens Redis in the configured mode (direct / Sentinel / Cluster) and
  `PING`s, connects to PostgreSQL and reports migration state (applied, pending, checksum
  drift, failed rows, versions from a newer build) **without running anything**. Human or
  `-o json` output, exit `2` on failure, `--strict` for warnings, `--skip-remote` /
  `--skip-probes`; URL passwords and every secret configuration value are scrubbed from the
  report. Runs in CI against the E2E node and in the chaos harness (`doctor` scenario, Sentinel
  and Cluster) ([CLI](docs/src/backend/cli.md)).
* **Soak harness.** `tools/soak/run.sh` + the `aurix-soak` bot runner keep the two-node chaos
  topology under real native clients (`aurix-client`: SessionBind, join, Opus over QUIC/UDP/TLS,
  resume, failover, token refresh) for hours while node kills, Redis failover, PostgreSQL
  restarts, a UDP blackhole (→ TLS tunnel fallback and back) and a netem loss/delay/reorder
  window run in rotation. Hearing ratio,
  loss, replay/auth counters, recovery time per fault, node/harness RSS and fd growth and
  leftover sessions are asserted; JSONL report + summary. Nightly 90 min run in the `Soak`
  workflow, longer runs by dispatch or on a dedicated host
  ([Development and load testing](docs/src/operations/development.md)).

### Fixed

* Native TLS tunnel (`aurix-client`): the close reason is published before the closed flag, so a
  reader that observes the link closed never sees an empty reason.
* Web SDK WebTransport: `close()` records the reason and state before tearing the transport down,
  so `onClose`/state observers see a consistent pair.
* `aurix_active_participants` was registered but never updated; it now reports the number of
  channel memberships on the node.
* Unity native TLS tunnel: `TlsMediaTunnel.CloseReason` is now set before `IsClosed` flips, so an
  observer that sees the link closed always sees why (the `Closed` event still fires last).

## [1.5.0] - 2026-09-23

Feature release: the roadmap items behind the former "limitations" list — Redis Cluster,
RTT-measured multi-level cascade with TCP fallback, a TLS media tunnel and WebTransport for
strict firewalls, speaker demotion, server-side live-stream mixing with outage buffering,
opt-in server-side noise suppression, PCMA, stored transcripts and exactly-once per-device chat
delivery. Migrations `20240101000021` … `20240101000024` are applied on start; the protocol and
REST additions are backwards compatible with 1.4 clients and SDKs, but the Redis key layout
changed (restart every node on this version before relying on cross-node failover) and the
`aurix_noise_suppression_frames_total` path label for G.711 is `g711` (was `pcmu`).

### Added

* **Redis Cluster.** `redis.cluster = ["redis://…", …]` (env `AURIX__REDIS__CLUSTER`, exclusive
  with Sentinel) runs the control plane against a Redis Cluster: slot-aware client with
  `MOVED`/`ASK` handling and topology refresh, hash-tagged session (`session:{id}:*`) and user
  (`user:{id}:*`) keys so the ownership CAS scripts and multi-key `DEL`s stay same-slot, and
  cross-node events over sharded Pub/Sub (`SSUBSCRIBE`/`SPUBLISH`, Redis 7 / RESP3) with
  `redis.sharded_pubsub = false` as the classic fallback. Losing the master that owns the event
  slot detaches the subscriber (`/ready` → 503) until the replica is promoted and the node
  re-subscribes. Live tests (`AURIX_E2E_REDIS_CLUSTER`) and the chaos harness
  (`AURIX_CHAOS_REDIS=cluster`, six-node cluster, shard kill under load) cover it; the CI `chaos`
  job now runs both the Sentinel and the Cluster fleet. Key layout is compatible with existing
  standalone/Sentinel deployments only after all nodes restart on this version (session and
  user keys changed names; in-flight mirrors of older nodes are not read).
* **Cascade: measured links, deeper trees, TCP fallback.** Every node pings its cascade peers
  (`media.cascade_probe_interval_ms`, 1 s; sealed `Heartbeat` ping/pong on the cascade port),
  publishes the smoothed RTT and transport per peer to the new `media_node_links` table
  (migration `20240101000020`) and reads the fleet-wide link matrix back on every
  reconciliation pass. `region_tree` hubs are now ranked by it — candidates that reach every
  node they must talk to first, relay-only next, lowest RTT, then the channel hash — and the
  tree grows a level where links are blocked: a *core hub* joins two hubs that do not reach
  each other (or only much slower), and a region whose hosts do not reach each other becomes a
  star around its hub; the hop cap is `MAX_RELAY_HOPS` = 5. A peer that misses three UDP probes
  is reached over **TCP on the same cascade port** (`media.cascade_tcp_fallback`, default on):
  length-prefixed frames of the very same sealed envelopes, `Hello`-authenticated inbound
  connections, per-peer anti-replay, bounded queues (drops counted in
  `aurix_cascade_tcp_dropped_total`), back to UDP as soon as it answers. New
  `GET /v1/nodes/links` (`nodes:read`) / `aurix node links` and
  `aurix_cascade_links{transport}` expose the table. Nodes without link probes keep working in
  a mixed fleet (their links count as unmeasured, and they still drop envelopes deeper than
  their own 3-hop cap, so upgrade hubs first). Live E2E now blocks UDP between the hubs
  with `iptables` (`AURIX_E2E_SUDO_IPTABLES=1`) and checks audio, table and metrics through the
  fallback and back.
* **Dedicated TLS media tunnel (TCP/443) for native clients.** `media.tls_tunnel_port` (default
  `0` = off; run it on 443 or behind a TLS-passthrough Caddy/Traefik) opens a TLS 1.3 listener
  (ALPN `aurix-tunnel/1`, `tokio-rustls`) that carries the very same sealed AURX packets as
  `u16`-length-prefixed frames — AURX authentication, anti-replay, E2EE and the 1400-byte packet
  bound are unchanged; a zero-length, oversized or truncated frame closes the connection. The
  listener shares the QUIC certificate (`quic_cert_path`/`quic_key_path` or the generated
  self-signed one) and its `host:port` list plus the SHA-256 pin are advertised in
  `SessionInitAck.tls_tunnel` (`media.tls_tunnel_advertise` overrides the addresses when a proxy
  fronts the node). Bounded downlink queues (`tls_tunnel_queue_packets`), a connection cap
  (`tls_tunnel_max_connections`) and a bind timeout (`tls_tunnel_bind_timeout_ms`) protect the
  node; `aurix_tls_tunnel_{packets_total,handshakes_total,connections,sessions}` and
  `media_path = "tls"` in session stats expose it. **Native core** (`ClientConfig.tls_tunnel`,
  `MediaPathPolicy::TlsOnly`, `MediaPath::Tls`, `SessionInfo.media_tls`; C ABI
  `AURIX_MEDIA_TLS` = 5, `AURIX_MEDIA_PATH_TLS_ONLY`, C++/Unreal/Godot bindings) and the
  **Unity C# SDK** (`TlsMediaTunnel` on `SslStream`, `MediaPathPolicy.TlsOnly`,
  `MediaPath.Tls`, `AurixVoiceClient.TlsTunnel`) pin the advertised certificate, require the
  ALPN and slot the tunnel into `Auto` after QUIC and UDP and before the WebSocket tunnel: a
  blocked-UDP bind falls to TLS, a TLS session re-probes UDP periodically and moves back, a TLS
  connection the node closes falls to the WebSocket tunnel, and a resumed session re-binds over
  TLS. Covered by listener tests (framing, ALPN/pin refusal, bind timeout, queue overflow, cap),
  client↔SFU integration tests, Unity tests against a TLS 1.3 fake node and a live E2E that
  black-holes UDP with `iptables`. Certificate renewal stays the operator's job (no ACME).
* **AURX over WebTransport for browsers.** `media.webtransport_port` (default `0` = off; meant
  for UDP/443, separate from `media.port`) runs an HTTP/3 endpoint (`wtransport`) whose
  WebTransport session at `/aurix` carries sealed AURX packets one per QUIC datagram — the same
  bytes a native QUIC client sends, so `SessionBind` ownership, authentication, anti-replay, the
  1400-byte bound, E2EE frames, mixed downlinks and `BitrateCommand` are the AURX ones and a
  browser session is a native (`aurx`) session to the router: per-speaker streams, no SDP/ICE/
  TURN, no WebRTC. Certificates: an operator PEM pair (`webtransport_cert_path`/`key_path`, Web
  PKI, advertise the DNS name in `webtransport_advertise`) or — the default, fine for bare IPs —
  a node-generated short-lived ECDSA P-256 certificate (`webtransport_cert_days`, 1–14) that is
  rotated at half its validity; both the current and the next hash go to browsers in
  `SessionInitAck.webtransport { urls, cert_sha256 }` for `serverCertificateHashes`. Bounded
  downlink queues (`webtransport_queue_packets`), a connection cap
  (`webtransport_max_connections`), a bind timeout (`webtransport_bind_timeout_ms`) and the
  20 s idle timeout protect the node; `aurix_webtransport_{packets_total,handshakes_total,
  connections,sessions,cert_rotations_total}` and `transport = "webtransport"` in `MediaBound` /
  session stats expose it. **Web SDK**: `transport: 'auto' | 'webrtc' | 'webtransport'`
  (`auto`, the default, takes WebTransport when the node advertises it and the browser has
  WebTransport datagrams + WebCrypto + WebCodecs, otherwise — or when every advertised URL
  fails — negotiates WebRTC as before; `webtransport` fails `connect()` instead of falling
  back; `webrtc` never tries), `webTransport: { connectTimeoutMs, heartbeatIntervalMs,
  heartbeatLossLimit, opus, idleTimeoutMs }`, `client.mediaTransport`, the `mediaTransport`
  event and `ClientStats.transport`. On this path the browser runs Opus itself through WebCodecs
  (`AudioEncoder`/`AudioDecoder`, 20 ms AudioWorklet capture) with the full parameter set the
  native SDKs have — complexity, signal, application, expected loss, FEC, DTX, CBR, bitrate —
  merged from the channel policies exactly like the native core; downlink SSRCs become sources
  of the same spatial renderer (HRTF, per-participant gains, visemes, ducking) as WebRTC tracks,
  server-processed frames carry the receiver's gain/direction, E2EE frames are decrypted in the
  page, heartbeats give RTT/loss for `QualityReport`, a session that loses the path
  re-establishes media (WebRTC when `auto`) with a continuous sequence counter. Covered by 22
  focused SDK tests against a fake WebTransport node (capability blocker, hash filtering,
  `SessionBind`, URL fallback, capture routing plain/E2EE, playback, stats, mute, reconnect,
  packet-size drops, heartbeat RTT/loss, `SessionClose`, `BitrateCommand`, strict/auto/webrtc
  policies, sequence continuity, bind-retry cleanup, heartbeat opt-out), client↔SFU integration tests over a real
  `wtransport` client (bind, media both ways, browser↔native UDP peer, wrong path/pin/key/
  unbound refusals, session replacement, connection cap) and a Chromium E2E
  (`sdk/web/test/browser/webtransport_e2e.py`, in the `godot-web` CI job) with strict/auto/
  WebRTC tabs sharing a channel, cross-transport audio, mute and E2EE. Browsers missing any of
  WebTransport datagrams, `serverCertificateHashes` (for the node-generated certificate),
  WebCrypto or WebCodecs Opus stay on WebRTC under `auto` (Chromium-based browsers have the set).
  `getParticipantStreams()` / the `participantStreams` event list the rendered SSRC slots as
  `{ mid: 'wt:<ssrc>', userId, stream: undefined, live: true }` (there is no `MediaStream`;
  `AurixBridge` hosts — Unity WebGL, Godot Web — get the same layout and a `remoteAudio`
  report for the Web Audio graph), the datagram queues are tuned for a busy page (64 outgoing /
  256 incoming buffered datagrams, 500 ms max age) and WebCrypto backlogs after a main-thread
  stall are bounded and dropped (counted in `packetsDroppedLocally`) instead of piling up
  behind heartbeats.
* **Speaker admission at `audience.max_speakers`.** `audience.speaker_admission` decides what
  happens to a joiner with a speaking grant once every slot is held: `reject` (the previous
  behaviour, `channel_full`), `wait` (joins as an effective listener, `ChannelJoinAck.
  waiting_to_speak`, gets a slot when one frees — priority speakers and moderators first, then by
  waiting time) or `demote` (like `wait`, plus a plain speaker silent for
  `audience.demote_idle_ms`, default 3 s, yields its slot to a waiting member that is trying to
  speak — longest idle first, quietest on a tie; a joining priority speaker, moderator or
  administrator takes the least recently active plain speaker's slot at once). Priority
  speakers, moderators and administrators are never demoted; the persisted grant never changes,
  only the effective role, so a demoted member is a listener for the router (frames dropped),
  the roster and the mixer until it is admitted again. Remote speakers count against the cap
  across the cascade and role changes travel between nodes. New `RoleChanged` control message
  (`reason` = `speaker_demoted` / `speaker_admitted`; channels with `hide_listeners` show it as
  a leave/join to the members that see the user), `participant.role_changed` webhook/SSE
  event, `waiting_to_speak` on `GET /v1/channels/{id}/participants` memberships (migration 21),
  and `participantRoleChanged` / `waitingToSpeak` (Web, `AurixBridge` hosts),
  `OnParticipantRoleChanged` / `IsWaitingToSpeak` (Unity, Unreal), `participant_role_changed` /
  `is_waiting_to_speak` (Godot) and `AurixEventParticipantRoleChanged` /
  `waiting_to_speak` (C ABI) in the SDKs.
* **Live audio streams: server-side mix, outage buffering, fleet-wide access.** `mix=true`
  (pull query parameter / `CreateStreamRequest.mix`) turns a stream into one server-side mix of
  the selected, consenting, non-E2EE talkers — one 20 ms frame per tick with an all-zero user id
  and SSRC 0, Opus at `recording.live.mix_bitrate` (64 kbit/s) or PCM, soft-clipped, with limited
  concealment while a talker's packets are late; at most `recording.live.max_mix_streams` (16)
  mixers per node (`409` beyond, `0` disables). A stream now survives its consumer:
  `recording.live.outage_buffer_ms` (10 s, ≤ 5 min, `0` = off) of frames are kept in order while
  a pull consumer is away (`state: reconnecting`) or a push target is re-dialled, and replayed on
  reconnect after a fresh `hello` (`reconnects` bumped) and a `dropped` frame if the window
  overflowed; past the window the stream ends. Streams belong to the fleet: ownership and status
  live in the new `live_streams` table (migration 22, `node_id` on `LiveStream` and in `hello`),
  so `GET /v1/audio/streams`, `GET`/`DELETE …/audio/streams/{id}` (`202` when the stop is
  forwarded to the owner) and `pull?resume=<id>` (relayed to the owner with the caller's
  credentials) work on every node, a node that hosts none of the channel's participants may
  own its stream (it pins the channel so the cascade relays the audio to it), and rows of a
  vanished node are pruned with it. Tenant scoping applies to every remote view.
* **Server-side noise suppression (opt-in).** `[media.noise_suppression]` (`enabled = false`,
  `level = high | moderate | low`, `max_sessions = 256`, `bitrate = 32000`) lets a node denoise
  a session's uplink itself for clients that run no capture DSP: the RNNoise-class model of the
  native core (`nnnoiseless`, pure Rust, 48 kHz mono, 10 ms blocks) over the decoded Opus
  frame, re-encoded at `bitrate` with the same duration and in-band FEC, or inside the PCMU
  transcode where the samples are already PCM — before the frame reaches receivers, recording,
  transcription, the server mixes and the cascade. A session asks with the new
  `SetNoiseSuppression { enabled }` control message and gets `NoiseSuppressionChanged`
  (`SessionInitAck.noise_suppression` advertises the option, `NOISE_SUPPRESSION_UNAVAILABLE`
  / 503 when it is off or `max_sessions` are busy; kept in `ReceiverPreferences.noise_suppression`
  across resume/failover), or a channel sets `ChannelConfig.noise_suppression = true` to clean
  every uplink into it — rejected together with `stereo` or `e2ee`, and when the node cannot
  honour it the frames pass uncleaned. E2EE frames and frames into stereo channels are never
  decoded; a frame a native client sends to several channels is cleaned once and the result
  reused. Metrics `aurix_noise_suppression_sessions`,
  `aurix_noise_suppression_frames_total{path,outcome}`. SDKs: `setServerNoiseSuppression` /
  `serverNoiseSuppression` / `serverNoiseSuppressionChanged` + `sessionInfo.noiseSuppression`
  (Web, `AurixBridge` hosts), `SetServerNoiseSuppressionAsync` / `ServerNoiseSuppression` /
  `OnServerNoiseSuppressionChanged` (Unity native + WebGL), `set_server_noise_suppression` /
  `server_noise_suppression` + `Event::NoiseSuppressionChanged` (native core;
  `aurix_client_set_server_noise_suppression`, `AURIX_EVENT_SERVER_NOISE_SUPPRESSION_CHANGED`,
  `AurixSessionInfo.noise_suppression` in the C ABI / C++), `SetServerNoiseSuppression` /
  `IsServerNoiseSuppressionEnabled` / `OnServerNoiseSuppressionChanged` (Unreal),
  `set_server_noise_suppression` / `server_noise_suppression_changed` (Godot native + Web).
* **PCMA (G.711 A-law) and end-to-end encrypted G.711.** `SetAudioCodec { codec: "pcma" }`
  joins `"pcmu"` as a per-session fallback for native AURX clients: the same 8 kHz / 64 kbit/s
  frames, A-law companded, flagged with the new `PacketFlags::Pcma` (`0x0010`, the never-used
  `KeyFrame` bit; `Pcmu` stays `0x2000`). In plaintext channels the node transcodes each law at
  the edge exactly as it did PCMU — a PCMA uplink becomes narrowband Opus, an Opus/PCMU speaker
  reaches a PCMA listener re-encoded in A-law — and frames in the wrong law for the session are
  dropped. In **E2EE channels G.711 now works**: a `Pcmu | E2ee` / `Pcma | E2ee` frame is
  relayed sealed with its codec flag, byte for byte and only to capable members, like E2EE Opus
  (the node never decodes, transcodes, mixes, records, transcribes or denoises it); every SDK
  picks the decoder from the flag after opening the frame, browsers over WebTransport included.
  The negotiated law survives resume and cross-node failover (`ReceiverPreferences.codec`).
  Metrics are now per law: `aurix_g711_sessions{codec}` and
  `aurix_g711_frames_total{codec,direction,outcome}` replace `aurix_pcmu_sessions` /
  `aurix_pcmu_frames_total` (update dashboards and alert rules). SDKs: `AudioCodec.Pcma`
  everywhere — `G711Codec(AudioCodec)` (`PcmuCodec` kept as the μ-law alias), `AurxPacket.CodecFlag`
  / `CodecOf`, E2EE uplink for any codec (Unity); `Law::A` in `aurix_common::g711`,
  `AURIX_CODEC_PCMA` in the C ABI / C++ / Unreal `EAurixAudioCodec::Pcma` / Godot `CODEC_PCMA`;
  `g711.ts` A-law/μ-law decoding for E2EE frames in the Web SDK.

* **Stored transcripts (`stt.persist`).** With `stt.persist = true` every live transcript a node
  delivers (`Transcript` / `channel.transcript`) is written to the new `transcripts` table
  (migration `20240101000023`) under the event's own `id`, and each machine translation the
  fleet pushes to listeners hangs off it in `transcript_translations` (one row per target
  language, first node to translate wins) — E2EE channels are still never transcribed, so
  nothing of them is stored. Storage never gates delivery: a failed or slow (> 2 s) insert is
  logged and the caption still goes out. New REST: `GET /v1/channels/{id}/transcripts?user_id=&before=&after=&limit=`
  and `GET /v1/users/{id}/transcripts` (keyset pages, newest first, translations inlined;
  `transcripts:read`), `DELETE /v1/channels/{id}/transcripts` and `DELETE /v1/transcripts/{id}`
  (`transcripts:write`, audited as `transcripts_deleted`); moderators get `transcripts:read`,
  admins `transcripts:write` through `X-Aurix-App`. Rows are swept after `stt.retention_days`
  (30; 0 = keep), erased with the user (`UserErasureCounts.transcripts`) and included in the
  user data export (`UserExport.transcripts`). Server SDKs and the CLI gain the operations.

* **Exactly-once per-device delivery of directed chat messages.** A client that identifies its
  installation on the WebSocket handshake (`X-Aurix-Device: <id>`, sub-protocol `device.<id>` or
  `?device=`; `[A-Za-z0-9._~-]{1,128}`, `400` otherwise; kept across resume and cross-node
  takeover) gets its own delivery cursor in the new `chat_device_cursors` table (migration
  `20240101000024`, keyed by application, user and device). The device confirms delivery with
  `ChatAck { message_id }`; the server takes `sent_at` from the stored row (never from the
  client), accepts only stored directed messages addressed to that user (`NOT_FOUND` otherwise)
  and moves the cursor forward only. On connect a device with a cursor is replayed exactly the
  directed messages newer than it, from any node; `ChatInboxSynced` gains `per_device` (`true`
  for a cursor-driven replay, `false` for the read-marker replay that anonymous connections and
  first-time devices still get). Cursors idle for `chat.device_cursor_max_age_days` (90; `0` =
  keep) are swept, listed / forgotten over `GET /v1/users/{id}/chat-devices` and `DELETE
  /v1/users/{id}/chat-devices/{device_id}` (`chat:read` / `chat:write`, `ChatDeviceCursor`),
  exported with the user (`UserExport.chat_devices`) and erased with them
  (`UserErasureCounts.chat_device_cursors`). Every SDK takes the id (`deviceId` / `DeviceId` /
  `device_id`) and acknowledges automatically — the replay once by its newest message after
  `ChatInboxSynced`, live directed messages one by one — after the message reached your code.

### Fixed

* **Web SDK `getStats()` with per-participant tracks.** The WebRTC snapshot took whichever
  `inbound-rtp` entry `getStats()` listed last, so with dedicated downlink tracks
  (`participantStreams`) `packetsReceived`/`packetsLost`/`bytesReceived`/`concealedSamples`/
  `packetsDiscarded` described one arbitrary track — often an idle one. The counters now add up
  across all audio tracks; `jitterMs` is the worst track that carried packets and
  `jitterBufferDelayMs` the emitted-weighted mean.
* **A speaker's first frames arriving out of order muted them for the whole session.** The
  forwarded per-sender audio sequence gave a frame that overtook its predecessor on a jittery
  uplink the slot "before" the stream's first number, i.e. it wrapped to `u32::MAX`; native
  receivers moved their anti-replay window there and rejected every following frame as a
  replay (`replayed` climbing, no audio). Such a frame now takes the next fresh number.

## [1.4.0] - 2026-09-22

Licence change plus one media fix — no protocol, API or database changes. Nodes of 1.3 and 1.4
may share a fleet.

### Changed

* **Licensing.** The server side — `aurix-server` and the media, TURN, control, database, auth,
  moderation, recording and metrics crates, the operator dashboard, the `aurix` CLI, load tester,
  fuzzing / chaos / netem harnesses, release tooling, CI and migrations — is now licensed under
  **AGPL-3.0-only**. Everything a game links or ships stays **Apache-2.0**: `aurix-client` (native
  core and C ABI), `aurix-common`, `aurix-opus`, the Web / Unity / Unreal / Godot SDKs, the server
  SDKs and token-server examples, the OpenAPI contract, documentation and deployment examples.
  The map is `REUSE.toml` (checked by `reuse lint` in CI) and explained in `LICENSING.md`; every
  package carries its own `LICENSE`, the client core ships `NOTICE`, container images are labelled
  `AGPL-3.0-only`. The Apache-2.0-era releases `v1.2.0` and `v1.3.0` were withdrawn (release pages,
  tags and container images); 1.4.0 is the first release under the new layout.
* **Contributions** require a Developer Certificate of Origin sign-off (`git commit -s`); pull
  requests are checked by `tools/release/check_dco.py`. No CLA. See `CONTRIBUTING.md`.
* **Dashboard** shows a "Source" link to the repository the build came from (`AURIX_SOURCE_URL` at
  build time, default `https://github.com/aurix-voice/aurix`) — the convenient place for an AGPL
  §13 source offer when a modified dashboard is deployed.

### Fixed

* **`ChannelEnergy` could skip a short burst.** A participant who spoke for less than
  `media.energy_interval_ms` and went quiet before the reporter tick (which can run late on a loaded
  node) was never reported above silence, so listeners saw `SpeakingStateChanged` without a matching
  level. Levels measured since the last report are now reported once before decaying.

## [1.3.0] - 2026-09-19

Operator dashboard, richer stored chat, node drain, admin delegation and lossy-link protection
for listeners. Database migrations 18 and 19 are additive; nodes of 1.2 and 1.3 may share a fleet
during a rolling upgrade.

### Added

* **Operator dashboard** (`dashboard/`): a separate Vite + React + TypeScript SPA over the admin
  API — overview (CCU, minutes, MOS, fleet health, quality alerts), nodes with drain / undrain,
  applications / API keys / limits / webhooks (deliveries, test, resync, rotation), live channels
  and sessions with moderation actions and per-session statistics, moderation (reports, safety
  incidents with evidence, bans and blocks, users, stored chat with search), recordings
  (mixdown, transcripts, downloads), analytics (series, CSV, worst sessions), administrators /
  roles / OIDC / audit log, and the read-only effective configuration. RU/EN, light / dark /
  system theme; the UI gates on `AdminPermission` from the node. Shipped as its own Caddy image
  `ghcr.io/aurix-voice/aurix-dashboard` (same-origin proxy of `/v1`, `/admin`, `/health`,
  `/ready`, `/openapi.json`, SSE unbuffered, security headers, non-root, no capabilities), a
  `dashboard` Compose service, and Helm `dashboard.*` (Deployment, Service, PDB, standard
  `Ingress` or Traefik `IngressRoute`, NetworkPolicy egress to the node). CI job `dashboard`
  runs typecheck / lint / unit tests / build and a Playwright suite against a live node through
  the freshly built image; the release workflow publishes, signs and attests the image.
  Every paginated table (live channels, applications, webhook deliveries, moderation events /
  incidents / bans / users, stored chat, recordings, audit log) has a rows-per-page selector
  (10 / 25 / 50 / 100, default 25) remembered per table in the browser.
* Stored text chat grows edits, deletions, reactions and search (migration 18): `ChatEdit` /
  `ChatDelete` (author within `chat.edit_window_secs`, channel moderators may delete) fan out
  `ChatMessageUpdated` — a deletion is a tombstone that keeps the message id and position and
  disappears from search, offline replay and unread counts; `ChatReact` keeps exact per-message
  tallies (`message.reactions[]`, `chat.reactions_per_message` distinct, idempotent duplicates)
  and fans out `ChatReactionChanged`; `ChatSearch` runs PostgreSQL full-text search over one
  conversation with the history cursor (`chat.search`, `chat.searches_per_minute`). REST:
  `GET|PATCH|DELETE /v1/messages/{id}`, `PUT|DELETE /v1/messages/{id}/reactions/{reaction}`,
  `GET /v1/channels/{id}/messages/search`, `GET /v1/users/{id}/messages/search`; webhook / SSE
  events `chat.message_updated` and `chat.reaction`. Web, Unity (native + WebGL), native core /
  C ABI / C++, Unreal, Godot (native + Web) and the generated Node / Python / Go / C# server
  SDKs expose the new calls and events.
* Operator **node drain** (migration 19): `POST /v1/nodes/{id}/drain` / `undrain` (admin
  permission `nodes:drain`, optional reason, audited as `node_drained` / `node_undrained`).
  A draining node stays healthy but is skipped by node selection, region discovery and the
  failover list, and its `/ws` answers `503` to fresh sessions and cross-node takeovers;
  sessions already there stay and resume locally. The state is persisted in `media_nodes`
  (`MediaNode.drain {reason, since, by}`), survives heartbeats and restarts, and is applied by
  the node within one heartbeat. `aurix node drain|undrain`.
* `GET /admin/config` (`config:read`): the node's effective, merged configuration with every
  secret-bearing field (`*secret*`, `*password*`, `*_key`, `api_key`, `*_token`) and URL
  credential masked as `***`, plus node id, version, region and environment. Read-only — there
  is no mutation route by design. `aurix node config`.
* Administrators can act on one application without its API key: admin JWT +
  `X-Aurix-App: <app_id>` scopes any tenant route to that application, with the role mapped to
  tenant permissions (viewer → reads, moderator → moderation/chat/audit, admin → writes,
  superadmin → `*`). An API key in the request still takes precedence; unknown or deactivated
  applications are `404`. Audit and moderation actors record the administrator. This is the
  access path for the operator dashboard so keys never reach a browser.
* `NetworkQuality.receivers_loss_percent` — the worst downlink loss any local receiver of a
  session's audio reported over its last interval. The node pushes a `NetworkQuality` as soon as
  the loss the sender has to protect against crosses the 3 % / 10 % tiers, and the native, Unity,
  Unreal and Godot clients pick their FEC/DRED loss profile from the higher of the uplink loss
  and this value, so a listener on a lossy link gets redundancy from the talker. Old nodes and
  payloads without the field read back `0`.
* `tools/netem/shape.sh` and `crates/aurix-client/tests/netem_live.rs`: a repeatable lossy-WAN
  and network-migration E2E on Linux netem (loss, jitter, reordering per direction; QUIC
  migration under delay and loss) with quantitative MOS / recovery / profile assertions, run in
  CI.

### Fixed

* Live `ChatMessageReceived.sent_at` is now truncated to microseconds — the precision
  PostgreSQL stores — so a cursor built from a live message matches the stored row.
* `GET /v1/channels?active_only=true` returned `total` for *all* channels of the app, so
  clients paginated over pages that did not exist; `total` now counts active channels.
* `GET /v1/nodes` reports each node's `version` and `registered_at`; the node registers with its
  package version so fleet views can show version skew.
* `GET /v1/channels/{id}/participants` memberships now carry `display_name`, `media_node_id`,
  `is_priority` and the membership `id`, so participants hosted on another node are no longer
  anonymous to operators; `GET /v1/analytics/usage` totals now include the raw quality
  counters the OpenAPI schema already declared.

## [1.2.0] - 2026-09-19

First tagged release. The whole tree was versioned `1.2.0` while the features below were being
built, so this section lists everything that shipped under the number; the *Fixed* items are
defects found and repaired before the tag (CI on real runners, fuzzing, chaos runs).

### Added

* Per-participant WebRTC downlink tracks (`media.webrtc_participant_streams`) with browser-side
  HRTF / equal-power spatialization in the Web SDK and Unity WebGL; `SetParticipantStreams`
  pinning, `ParticipantStreams` layout events.
* Group end-to-end encryption for channels (`config.e2ee`): X25519 identity keys, generation-based
  sender keys, rotation on join/leave, replay protection; native core, browser (Insertable Streams),
  Unity and Unreal/Godot parity; `E2EE_REQUIRED` for incapable sessions.
* Priority speakers with server-side ducking (`config.ducking`, `priority` grant,
  `POST /v1/moderation/priority`, `ParticipantPriorityChanged`); game-audio ducking events.
* Local viseme / lip-sync analysis and a voice-effects library (filters, formant, pitch, ring mod,
  distortion, tremolo, reverb, presets) in every client SDK.
* Per-session MOS (ITU-T E-model) quality summaries, MOS alerts with debounce/hysteresis
  (`quality.alert` / `quality.recovered`), `GET /v1/analytics/sessions`, Prometheus quality
  histograms, alert rules and a Grafana dashboard.
* `aurix-opus`: bundled static libopus 1.6 with DRED and OSCE; adaptive loss profile (FEC / DRED /
  deep PLC tiers) in the native core; gap-aware server mixer.
* QUIC media transport for native clients (0-RTT session bind, connection migration on
  `network_changed()`, pinned node certificate), multiplexed on the media UDP port.
* Godot Web export over the Web SDK, Android/iOS staging for the GDExtension.
* Unity UPM package layout, WebGL browser CI against a live node.
* Unreal: Fab-ready plugin layout, `AurixVoiceSamples` Blueprint module, engine-free plugin checks,
  gated `BuildPlugin` CI.
* Server SDKs for Node, Python, Go and C# generated from `api/openapi.json`, token-server examples
  in all four languages, reworked `aurix` CLI (profiles, curated commands, `aurix api`, `diagnose`).
* Security: libFuzzer harnesses for every network-facing parser (`fuzz/`), deterministic corpus
  replay on stable, CI fuzz smoke job, threat model, `SECURITY.md`.
* Release engineering: `CHANGELOG.md`, `tools/release/check_versions.py`, tag-driven release
  workflow producing binaries, checksums, CycloneDX SBOMs, container images and (when enabled)
  Sigstore signatures.
* Chaos/HA harness: node kill with cross-node resume, Redis Sentinel failover, PostgreSQL restart,
  stale-node and isolation checks (`tools/chaos/`, CI job on Docker Compose).
* Human documentation: five-minute quickstart, migration guides from Vivox, Agora and Photon Voice,
  Russian translations of the key chapters.

### Changed

* Native clients default to `Auto` media transport ordering QUIC → UDP → tunnel.
* Forwarded RTP sequence numbers keep short uplink gaps visible to receivers (concealment on the
  receiving side), collapsing only pauses and long jumps.
* System `libopus-dev` is no longer required to build; `cmake` is.
* The release workflow builds each container-image architecture on a runner of that
  architecture and merges them into one manifest list instead of compiling under QEMU.

### Fixed

* `RtpHeader::parse` could panic on a truncated header extension; `StunMessage` re-encoding kept
  stale `MESSAGE-INTEGRITY` / `FINGERPRINT` attributes and failed its own verification. Both found
  by the new fuzz targets; regression seeds live in `fuzz/corpus/`.
* Concurrent session destroy and replacement could run the SFU teardown twice and underflow
  `aurix_active_sessions`.
* `aurix` profiles that point `api_key_file` at a not-yet-created file no longer fail every
  command before `aurix app create` has written it.
* Race in E2E firewall tests (shared state now behind an `RwLock`).
* An app's default API key was created with the never-enforced `100` requests/minute budget and
  throttled as soon as `rate_limiting.per_key` applied; it now starts at `6000` like keys from
  `POST /v1/api-keys`.
* `POST /v1/recordings/{id}/transcribe` checked the node's `[stt]` configuration before the
  recording's tenant, so a foreign tenant got `400` instead of `404`.
* The Docker image build did not copy `api/openapi.json`, which `aurix-api` embeds.
* The Unity Concentus sample let Concentus pick a host `libopus` when one was present, making
  its behaviour machine-dependent; it now always runs the managed port.
* `/ready` reported ready while the Redis Pub/Sub subscriber was still reconnecting after a
  Sentinel failover, so a balancer could send players to a node that missed other nodes'
  roster/chat/moderation events for a few seconds. Readiness now requires the subscriber to be
  attached, and a master switch wakes the subscriber immediately instead of waiting out its
  5-second backoff.
* The Godot Web export preset shipped the desktop demo scene, whose script needs the native
  GDExtension that the Web build does not include.
* The native core counted an E2EE frame as undecryptable when it overtook its sender key at a
  rotation (media over UDP/QUIC, key over the control plane); such frames now wait up to
  500 ms for the key and are decrypted when it arrives.
* CI runs the second node the native QUIC/tunnel E2E tests require, with `server.external_url`
  set on both nodes (failover advertisement must match the URL clients dial) and the same chat
  persistence, channel limit and rate-limiting settings on the peer.
* The media node publishes `SessionBound` before answering the bind, so observers never learn of
  a bind after the client already treats the session as bound.

### Baseline

What the repository already contained when the shared version number was introduced:

* AURX v2 authenticated encryption for native media, signed `SessionBind`, session resume with
  one-time resume tokens, one-time action tokens.
* Channel, positional (radius / directional / ambient) and echo channels, listener role,
  per-receiver stream caps, native server mix, TCP/WS media tunnel fallback, PCMU fallback.
* Cross-node session failover, Redis Sentinel, cascade relay trees, IPv6 dual-stack, fleet-wide
  rate limits.
* Text chat with history, offline delivery and read markers; moderation; content safety
  (STT → classifier, lexicon filter, evidence export); webhooks and SSE.
* Recording, mixdown, post-hoc STT, live audio taps; transcripts, TTS, translation.
* Admin OIDC SSO and roles; usage analytics; retention and user erasure.
* Client SDKs: native core with C ABI, Web, Unity (native + WebGL), Unreal, Godot; client DSP
  (high-pass, AEC, neural noise suppression, AGC); per-participant PCM for engine spatialization.
* OpenAPI contract, mdBook documentation, Helm chart, Terraform example, load-test tool.

Earlier development was not tagged; `git log` before `3fe205e` is the only record.

[Unreleased]: https://github.com/aurix-voice/aurix/compare/v1.5.0...HEAD
[1.5.0]: https://github.com/aurix-voice/aurix/compare/v1.4.0...v1.5.0
[1.4.0]: https://github.com/aurix-voice/aurix/compare/872f918...v1.4.0
[1.3.0]: https://github.com/aurix-voice/aurix/compare/31eb7a5...872f918
[1.2.0]: https://github.com/aurix-voice/aurix/tree/31eb7a5
