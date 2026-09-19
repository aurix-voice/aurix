# Aurix Voice Platform

Open-source, self-hosted voice chat for games and real-time apps — a drop-in alternative to
Vivox / Agora / Photon Voice that you run on your own infrastructure.

* **Two media paths**: a native low-latency UDP protocol (**AURX**, for game engines) and
  **WebRTC** (for browsers / Unity WebGL), bridged by one SFU with per-participant volumes.
* **Multi-tenant**: apps, API keys with fine-grained permissions, per-app users, channels,
  recordings, bans and audit log are strictly isolated.
* **Positional / 3D audio**, whisper, command and echo (mic test) channels, audio injection,
  server mute, kick, ban, reports, channel-wide mute-all / kick-all.
* **Recording** (Ogg/Opus, consent-gated, optional AES-GCM at rest, S3 / local storage) and
  **live audio streams** to your own services.
* **Built-in TURN/STUN** with time-limited HMAC credentials issued by the API.
* **Horizontal scale**: PostgreSQL + Redis control plane, media nodes register and heartbeat,
  channel events replicate across nodes, authenticated SFU-to-SFU cascade.
* **Ops**: Prometheus metrics, JSON logs, OpenTelemetry tracing, graceful drain, health/readiness,
  non-root container, CI with a live end-to-end test.

> Status: 1.2 — production-hardened core (auth, tenant isolation, media auth, TURN, recording) plus
> the full player feature set: reconnect/resume, chat, energy/VAD, positional/directional/ambient
> audio with radius-scoped presence, action tokens, webhooks/SSE, transcripts/TTS, content safety,
> PCMU fallback, and Web / Unity / native (C ABI) / Unreal SDKs.
> Read [Limitations](limitations.md) before deploying at scale.

## How to read this book

| You want to… | Start here |
| --- | --- |
| run a node and hear two players talk | [Quick start](getting-started/quick-start.md), then [Client flow](getting-started/client-flow.md) |
| integrate a game backend | [Tenancy, credentials and permissions](concepts/auth.md), [REST API and OpenAPI](api/rest.md), [Webhooks and the event stream](api/webhooks-sse.md) |
| integrate a game client | [Client SDKs](sdk/overview.md) — Web, Unity, native / Unreal |
| implement your own client | [WebSocket control plane](api/websocket.md), [Native AURX media](api/aurx.md) |
| operate it in production | [Deployment and configuration](operations/deployment.md), [Scaling out](operations/scaling.md), [Backups and observability](operations/observability.md) |

The machine-readable REST contract is `api/openapi.json` in the repository, also served by every
node at `GET /openapi.json` and browsable [here](api/reference.html). The full WebSocket message
set is the `ControlMessage` enum in `crates/aurix-common/src/protocol.rs`.

## Building the book

```bash
cargo install mdbook          # 0.5.x
mdbook build docs             # -> docs/book/
mdbook serve docs --open      # live-reloading preview
```

`docs/src/api/openapi.json` is a symlink to the repository's `api/openapi.json`, so the built
site always ships the same specification the server embeds. CI lints the specification with
Redocly and builds the book on every push.
