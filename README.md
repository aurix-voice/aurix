<p align="center"><img src="assets/logo.svg" width="72" height="72" alt="Aurix"></p>
<h1 align="center">Aurix</h1>
<p align="center">
  <a href="https://github.com/aurix-voice/aurix/releases"><img alt="Release" src="https://img.shields.io/github/v/release/aurix-voice/aurix?label=release&color=0a0a0a"></a>
  <a href="https://github.com/aurix-voice/aurix/actions/workflows/ci.yml"><img alt="CI" src="https://img.shields.io/github/actions/workflow/status/aurix-voice/aurix/ci.yml?branch=main&label=ci"></a>
  <a href="https://www.npmjs.com/package/@aurix/web-sdk"><img alt="npm" src="https://img.shields.io/npm/v/%40aurix%2Fweb-sdk?label=web%20sdk&color=0a0a0a"></a>
  <a href="LICENSE"><img alt="License" src="https://img.shields.io/badge/license-AGPL--3.0%20%2B%20permissive%20SDKs-0a0a0a"></a>
</p>

Open-source, self-hosted voice chat for games and real-time apps — a drop-in alternative to
Vivox / Agora / Photon Voice that you run on your own infrastructure.

Don't want to run it yourself? **[Aurix Cloud](https://aurix.run)** is the managed version of
this server: free tier with no card, [docs](https://aurix.run/docs/) and a
[live demo](https://rooms.aurix.run). Your servers, our control plane — also possible.

* **Two media paths**: a native low-latency UDP protocol (**AURX**, for game engines) and
  **WebRTC** (for browsers / Unity WebGL), bridged by one SFU with per-participant volumes.
* **Multi-tenant**: apps, API keys with fine-grained permissions, per-app users, channels,
  recordings, bans and audit log are strictly isolated.
* **Positional / 3D audio** with directional panning, radius-scoped presence and text, ambient
  (cocktail-party) mixing; whisper, command & echo (mic test) channels, audio injection, server
  mute, kick, ban, reports, channel-wide mute-all / kick-all.
* **Player features**: auto-reconnect with session resume and cross-node failover, text chat lite, energy/VAD,
  receiver-local mute/volume/blocks, transmission modes & channel focus, network-quality bars
  with per-session MOS history, debounced MOS alerts and quality analytics,
  live transcripts and TTS, single-use action tokens, per-app webhooks and an SSE event stream.
* **Recording** (Ogg/Opus, consent-gated, optional AES-GCM at rest, S3 / local storage).
* **Built-in TURN/STUN** with time-limited HMAC credentials issued by the API.
* **Horizontal scale**: PostgreSQL + Redis control plane, media nodes register and heartbeat,
  channel events replicate across nodes, authenticated SFU-to-SFU cascade.
* **Ops**: administrator roles with OIDC SSO, Prometheus metrics, JSON logs, OpenTelemetry
  tracing, graceful drain, health/readiness, distroless-ish non-root container, CI with a live
  end-to-end test.

> **Status: 1.13** — production-hardened core and the full player feature set, verified in CI
> against a live node. What is deliberately out of scope and what is not yet verified is listed in
> [Limitations](docs/src/limitations.md).

**Documentation**: the full book lives in [`docs/`](docs/src/SUMMARY.md) (`mdbook serve docs`) —
[quick start](docs/src/getting-started/quick-start.md), [architecture](docs/src/concepts/architecture.md),
[protocols and REST API](docs/src/api/rest.md), [SDK guides](docs/src/sdk/cookbook.md),
[operations](docs/src/operations/deployment.md), [migration from Vivox / Agora / Photon Voice](docs/src/migration/README.md).
The REST contract is [`api/openapi.json`](api/openapi.json) (OpenAPI 3.1), also served by every node at `GET /openapi.json`.

---

## Quick start

```bash
docker run -d --name aurix-pg -e POSTGRES_USER=aurix -e POSTGRES_PASSWORD=aurix -e POSTGRES_DB=aurix -p 127.0.0.1:5432:5432 postgres:16-alpine
docker run -d --name aurix-redis -p 127.0.0.1:6379:6379 redis:7-alpine

cargo run --bin aurix-server                # uses configs/default.toml (development mode)
```

Bootstrap the first administrator and create an app (the response carries the app's first API key):

```bash
curl -X POST localhost:8080/admin/setup -H 'content-type: application/json' \
  -d '{"email":"root@example.com","password":"<strong password>","display_name":"Root"}'
ADMIN=$(curl -s -X POST localhost:8080/admin/login -H 'content-type: application/json' \
  -d '{"email":"root@example.com","password":"<strong password>"}' | jq -r .token)
curl -X POST localhost:8080/v1/apps -H "authorization: Bearer $ADMIN" -H 'content-type: application/json' -d '{"name":"MyGame"}'
```

Everything after that is done with the app's API key (`x-api-key`) from your game backend — a
channel and a short-lived player token are enough for the first call:

```bash
curl -X POST localhost:8080/v1/channels -H "x-api-key: $KEY" -H 'content-type: application/json' \
  -d '{"name":"lobby","config":{"channel_type":"positional","max_participants":64}}'
curl -X POST localhost:8080/v1/tokens -H "x-api-key: $KEY" -H 'content-type: application/json' \
  -d '{"external_id":"steam:7656119","display_name":"Alice",
       "channels":[{"channel_id":"<channel uuid>","speak":true,"receive":true}]}'
```

Hand the token to the client SDK and join. Ad-hoc channels, action tokens, moderation, webhooks and
the rest of the API are in the [client flow](docs/src/getting-started/client-flow.md) and the
[REST reference](docs/src/api/rest.md); production deployment (TLS, firewall, scaling out,
backup, observability) is covered chapter by chapter under [operations](docs/src/operations/deployment.md).

## Client SDKs

| SDK | Path | Media path | Verified by |
|-----|------|-----------|-------------|
| Web (TypeScript) | [`sdk/web`](sdk/web) | WebRTC/Opus via the SFU, WS control plane, demo page | two-browser smoke test (ICE/DTLS, RTP both ways, decoded audio) |
| Unity / .NET (C#) | [`sdk/unity`](sdk/unity) | native AURX v2 over UDP (signed SessionBind, AES-256-CTR + HMAC per packet, replay window), WS control plane; capture DSP via the native core (AEC/NS/AGC) with a managed high-pass + AGC fallback; Unity WebGL players get the same C# API over the Web SDK (browser WebRTC) through `AurixWebGL.jslib` | `dotnet test` (incl. a scripted WebGL bridge) + headless two-client E2E (`Aurix.Demo`, real Opus via Concentus); Unity compile check with `UNITY_WEBGL` |
| Native core (Rust + C ABI) | [`crates/aurix-client`](crates/aurix-client) | same native AURX v2 path in Rust: Opus/VAD/jitter/mixer, capture DSP (high-pass, acoustic echo cancellation, RNNoise-derived noise suppression, AGC) and a voice-effects chain (pitch shift, ring modulator, host callback) on the uplink, reconnect + resume, all control-plane features; `libaurix_client` + `include/aurix_client.h` for Unreal, mobile and custom engines | unit + fake-server tests, C sample compiled/linked/run in CI, live two-client E2E (`cargo test -p aurix-client --test e2e_live`) |
| Unreal Engine 5.3 – 5.8 (C++/Blueprint) | [`sdk/unreal`](sdk/unreal) | `AurixVoice` plugin over the `aurix-client` C ABI: `UAurixVoiceSubsystem` with typed Blueprint events, `AudioCapture` microphone bridge, procedural playback wave + per-participant sound waves for engine spatialization; `AurixVoiceSamples` module with Blueprint-spawnable lobby / proximity-voice components and a function library; Fab-ready layout (`FilterPlugin.ini`, icon, `Docs/`); static-Opus native library staged by `sdk/unreal/scripts/build_native.*` | native library builds (Linux/Win64/Mac) + ABI-reference test + engine-free descriptor/UHT-convention checks in CI; `RunUAT BuildPlugin` job gated on an Epic-linked GHCR token, so UHT/engine compile **not** run here (see the [README](sdk/unreal/README.md)) |
| Godot 4.3+ (GDExtension) | [`sdk/godot`](sdk/godot) | `aurix_voice` GDExtension over the same C ABI: `AurixVoiceClient` node (signals for every control-plane event, `AudioStreamMicrophone` capture through an `AudioEffectCapture` bus, mixed playback through `AudioStreamGenerator`), `AurixParticipantPlayer` (`AudioStreamPlayer3D` per speaker for engine spatialization), `AurixRegions` for RTT-ranked node discovery; static-Opus native library staged by `sdk/godot/scripts/build_native.sh` (Linux/Windows/macOS; Android/iOS recipes staged); Web export via `AurixWebVoiceClient` (GDScript over `JavaScriptBridge` + Web SDK, same signals, browser-owned audio, `scripts/build_web.sh`) | extension build + headless API smoke tests (native + Web) in CI; live two-client Godot E2E (`sdk/godot/tests/live.sh`) against a running node; Web export booted in headless Chromium against a live node (`tests/web/godot_web_e2e.py`); mobile slices not built in CI |

All SDKs authenticate with the per-user JWT from `POST /v1/tokens`; API keys stay on your backend.

## Game backend: server SDKs, token servers, CLI

| | Path | What |
|---|---|---|
| Server SDKs (Node, Python, Go, C#) | [`sdk/server`](sdk/server) | typed clients generated from `api/openapi.json` (`python3 tools/openapi-sdk/generate.py --check` guards drift) with hand-written transports: retries with `Retry-After`, structured errors, API-key / operator / bootstrap credentials, webhook signature verification, SSE iterator |
| Token servers | [`sdk/server/examples/token-server`](sdk/server/examples/token-server) | the same backend in all four languages: your game session → `POST /voice/token` → `POST /v1/tokens` with the API key → allowlisted `{token, user_id, expires_at, endpoint}` to the client; the key never reaches the client, identity is never taken from the request body |
| Rooms demo | [`examples/rooms`](examples/rooms) | a complete voice-conference app on one node — link-joinable rooms, a Lounge with a native bot playing music/readings/test signals, chat, speaking rings, live quality, phone layout; Node backend on the server SDK, Vite frontend on the Web SDK, Rust bot on `aurix-client`, `deploy/run.sh` for the whole stack |
| `aurix` CLI | [`crates/aurix-cli`](crates/aurix-cli) | profiles that reference (never store) secrets, `token issue`, channel/user/moderation/webhook/analytics/recording/admin commands, `events tail`, `diagnose`, and `aurix api <operationId>` for every operation of the embedded contract |

Docs: [Server SDKs and token servers](docs/src/backend/server-sdks.md), [The aurix CLI](docs/src/backend/cli.md).

## Security, releases and resilience testing

| | Where | What |
|---|---|---|
| Threat model + reporting | [`docs/src/concepts/threat-model.md`](docs/src/concepts/threat-model.md), [`SECURITY.md`](SECURITY.md) | what Aurix defends (tenant isolation, auth boundaries, E2EE confidentiality from the node, membership/rate limits) and what it explicitly does not (volumetric DDoS, a malicious operator in non-E2EE channels, cheating clients); private-report process, scope, supported versions |
| Fuzzing | [`fuzz/`](fuzz) | 12 libFuzzer targets for every network-facing parser (AURX, RTP, STUN/TURN, control JSON, E2EE frames, Opus, Ogg, WAV, live frames, webhook signatures, remote mixer, text parsers); the committed corpus (seeds + `regress_*`) replays on stable in `cargo test`, nightly + ASan smoke in CI |
| Releases | [`CHANGELOG.md`](CHANGELOG.md), [`docs/src/operations/releases.md`](docs/src/operations/releases.md), [`.github/workflows/release.yml`](.github/workflows/release.yml) | one SemVer number across server, SDKs and chart (`tools/release/check_versions.py`), tag-driven workflow: binaries per platform, SDK packages, container images, SHA-256 checksums, CycloneDX SBOMs, Sigstore keyless signatures and build-provenance attestations |
| Chaos / HA | [`tools/chaos/`](tools/chaos) | Docker Compose fleet (PostgreSQL, Redis master/replica + three Sentinels or a six-node Redis Cluster, two nodes): node SIGKILL with cross-node resume, Redis Sentinel failover / Cluster shard failover, PostgreSQL stop/start with readiness recovery, stale-node reaper, tenant/session isolation — reusing the live E2E suite; CI `chaos` job |
| Migration | [`docs/src/migration/`](docs/src/migration) | Vivox, Agora and Photon Voice → Aurix: credential boundary, channel/grant mapping, API-by-API tables, staged migration plan; key chapters also [in Russian](docs/src/ru/README.md) |

## Contributing and development

`cargo test --workspace` runs the unit and integration suites against the local PostgreSQL/Redis
above; `tools/` holds the load generator, chaos fleet and SDK generators, and the
[development chapter](docs/src/operations/development.md) describes the layout, the live
end-to-end test CI runs and how releases are cut. Contributions are accepted with a
[DCO](https://developercertificate.org/) sign-off (`git commit -s`), no CLA — see
[`CONTRIBUTING.md`](CONTRIBUTING.md) and [`SECURITY.md`](SECURITY.md) for reporting vulnerabilities.

## License

Two licences along one boundary — see [`LICENSING.md`](LICENSING.md) for the exact map and
what it means for a game, an operator or a fork:

* **AGPL-3.0-only** for everything an operator runs: the server node, dashboard, CLI, tooling,
  migrations and CI. Run it freely at any scale; if you modify the server and serve users over a
  network, offer them your modified source.
* **Apache-2.0** for everything a game links or ships: the native core and C ABI, `aurix-common`,
  `aurix-opus`, the Web / Unity / Unreal / Godot SDKs, the server SDKs, the OpenAPI contract, the
  documentation and the deployment examples. Closed-source and commercial games are unaffected by
  the AGPL.

`v1.4.0` is the first release under this layout; the withdrawn `v1.2.0` / `v1.3.0` releases were
Apache-2.0 throughout (see [`LICENSING.md`](LICENSING.md)). Contributions are accepted
with a [DCO](https://developercertificate.org/) sign-off (`git commit -s`), no CLA — see
[`CONTRIBUTING.md`](CONTRIBUTING.md).
