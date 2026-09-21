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
* CI runs the second node the native QUIC/tunnel E2E tests require.

## [1.2.0] - 2026-09-19

First version with a shared version number across server, SDKs and chart. Highlights of what
the repository contained at this point:

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
