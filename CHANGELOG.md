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

[Unreleased]: https://github.com/aurix-voice/aurix/compare/v1.2.0...HEAD
[1.2.0]: https://github.com/aurix-voice/aurix/releases/tag/v1.2.0
