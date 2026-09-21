# Architecture

```
 game client ──AURX/UDP──┐                     ┌── PostgreSQL (apps, users, channels, sessions,
 browser  ──WebRTC/DTLS──┤   ┌─────────────┐   │              memberships, bans, recordings, audit)
                         ├──▶│  aurix-server│◀──┤
 signalling ──WebSocket──┤   │  API :8080   │   └── Redis (cross-node events, rate limits)
 backend ────REST/API key┘   │  WS  :8081   │
                             │  media:10000 │──▶ other media nodes (authenticated cascade)
 NAT traversal ──TURN:3478──▶│  turn :3478  │
                             │  metrics:4040│──▶ Prometheus / Grafana
                             └─────────────┘
```

One binary, `aurix-server`, hosts every plane. Run one for a single-node deployment or N against
the same PostgreSQL + Redis for a fleet ([Scaling out](../operations/scaling.md)).

| crate | role |
|---|---|
| `aurix-common` | types, AURX wire protocol, crypto, config, errors, jitter buffer, quality model |
| `aurix-db` | SQLx models, queries, embedded migrations |
| `aurix-auth` | player JWTs, admin JWTs (Argon2), API keys, action tokens, TURN credentials |
| `aurix-media` | SFU: sessions, channels, routing, Opus mixing, WebRTC (str0m), cascade, TTS injection |
| `aurix-turn` | RFC 5766/5389 TURN/STUN server (UDP + TCP, long-term credentials) |
| `aurix-control` | control plane: sessions, channels, nodes, events, rate limits, audit, webhooks, retention, user lifecycle |
| `aurix-api` | REST API (axum) — the contract is `api/openapi.json` |
| `aurix-ws` | WebSocket signalling |
| `aurix-moderation` | bans, mutes, kicks, reports, STT-based content analysis hooks |
| `aurix-recording` | Ogg/Opus writer, consent, retention, encryption, S3, live audio streams |
| `aurix-metrics` | Prometheus registry |
| `aurix-server` | the binary; wires everything together |
| `aurix-cli` | `aurix` CLI: operator, backend and diagnostic commands over the embedded OpenAPI contract ([The aurix CLI](../backend/cli.md)) |
| `aurix-client` | native client core (Rust + C ABI) used by the Unreal plugin |
| `aurix-loadtest` | reproducible load generator behaving like real native clients |

## Planes

**Control plane** (REST + WebSocket). REST is for your backend and operators: tenants, keys,
channels, tokens, moderation, recordings, webhooks, analytics. The WebSocket carries one JSON
session per player: session init, channel join/leave, presence-in-channel events, receiver
preferences, chat, moderation with action tokens, quality, recording consent, WebRTC signalling.

**Media plane** (UDP). Native clients speak AURX v2 — encrypted, authenticated 20 ms Opus frames
with optional level/direction metadata — and browsers speak WebRTC (ICE/DTLS-SRTP) on the same
UDP port. The SFU routes per channel and per receiver (positional attenuation, receiver-local
mutes/volumes/blocks, transmission mode, focus), re-sealing every downlink packet with the
receiver's keys. WebRTC downlinks are mixed on the server (stereo for directional channels), plus
a bounded set of per-participant tracks — the speaker's own Opus frames forwarded as-is — that the
browser spatializes itself with Web Audio HRTF ([channels](../features/channels.md#per-participant-tracks-for-browsers)).

**Event plane.** Everything that happens is published as a `ServerEvent`: to other nodes over
Redis pub/sub (with the origin node id so a node never re-applies its own events), to game
servers as signed [webhooks or SSE](../api/webhooks-sse.md), and to the audit log for
privileged actions.

## State

* **PostgreSQL** is the source of truth: apps, API keys, admins, users, channels, sessions,
  memberships, bans, moderation events, blocks, recordings, webhook subscriptions and
  deliveries, audit log, analytics, tombstones, optional chat history.
* **Redis** holds ephemeral state: the cross-node event bus, distributed rate limits, action-token
  claims, session presence. It is optional for a single node (features degrade to node-local) and
  required for multi-node deployments. It needs no backup.
* **The SFU** holds live sessions, channels and routing tables in memory on the node that owns
  the session (`media_addr` in `SessionInitAck` points there). Recordings, live streams and
  session statistics are therefore *node-local* resources.
