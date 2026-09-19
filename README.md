# Aurix Voice Platform

Open-source, self-hosted voice chat for games and real-time apps — a drop-in alternative to
Vivox / Agora / Photon Voice that you run on your own infrastructure.

* **Two media paths**: a native low-latency UDP protocol (**AURX**, for game engines) and
  **WebRTC** (for browsers / Unity WebGL), bridged by one SFU with per-participant volumes.
* **Multi-tenant**: apps, API keys with fine-grained permissions, per-app users, channels,
  recordings, bans and audit log are strictly isolated.
* **Positional / 3D audio, whisper & command channels, server mute, kick, ban, reports.**
* **Recording** (Ogg/Opus, consent-gated, optional AES-GCM at rest, S3 / local storage).
* **Built-in TURN/STUN** with time-limited HMAC credentials issued by the API.
* **Horizontal scale**: PostgreSQL + Redis control plane, media nodes register and heartbeat,
  channel events replicate across nodes, authenticated SFU-to-SFU cascade.
* **Ops**: Prometheus metrics, JSON logs, OpenTelemetry tracing, graceful drain, health/readiness,
  distroless-ish non-root container, CI with a live end-to-end test.

> Status: 1.0 — production-hardened core (auth, tenant isolation, media auth, TURN, recording).
> See [Limitations](#limitations) before deploying at scale.

---

## Architecture

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

| crate | role |
|---|---|
| `aurix-common` | types, AURX wire protocol, crypto, config, errors, jitter buffer |
| `aurix-db` | SQLx models, queries, embedded migrations |
| `aurix-auth` | player JWTs, admin JWTs (Argon2), API keys, TURN credentials |
| `aurix-media` | SFU: sessions, channels, routing, Opus mixing, WebRTC (str0m), cascade |
| `aurix-turn` | RFC 5766/5389 TURN/STUN server (UDP + TCP, long-term credentials) |
| `aurix-control` | control plane: sessions, channels, nodes, events, rate limits, audit |
| `aurix-api` | REST API (axum) |
| `aurix-ws` | WebSocket signalling |
| `aurix-moderation` | bans, mutes, kicks, reports, STT-based content analysis hooks |
| `aurix-recording` | Ogg/Opus writer, consent, retention, encryption, S3 |
| `aurix-metrics` | Prometheus registry |
| `aurix-server` | the binary; wires everything together |
| `aurix-cli` | `aurix` admin CLI |

### Security model (short)

* Native media uses **AURX protocol v2**: every packet is encrypted (AES-256-CTR) and
  HMAC-SHA256-authenticated with keys derived from a **per-session media key** handed out over the
  authenticated WebSocket (`auth`/`enc`/`salt` sub-keys via HMAC labels; the per-packet IV is built
  from packet type, SSRC, sequence and timestamp, so a session's monotonic sequence counter makes
  IVs unique). The tag covers header + ciphertext, so headers cannot be altered either. A session
  must complete a signed `SessionBind` (the only unencrypted packet — the server needs the session
  id to pick the key; timestamp + nonce, replay-protected) before the server accepts media from
  its address; packets from any other address, with a wrong key, wrong SSRC, unencrypted, or
  outside the replay window are dropped. The server decrypts once per uplink packet and re-seals
  the downlink individually for every receiver with that receiver's keys, so no participant can
  read another participant's packets even on a shared network.
* WebRTC uses DTLS-SRTP; the browser's SSRCs are mapped to the authenticated session.
* Tenant identity always comes from the validated API key / JWT — never from request bodies.
* **One-time action tokens** (`POST /v1/tokens/action`): short-lived (90 s by default) JWTs with a
  unique `jti` that authorise exactly one `login`, `join`, `kick`, `mute` or `unmute`. The first
  presentation claims the `jti` atomically in Redis (`SET NX EX`, key scoped to the tenant); a
  replay — on any node — is refused with `TOKEN_REUSED`. Without Redis the claim registry is
  node-local. `AURIX__AUTH__REQUIRE_ACTION_TOKENS=true` makes them mandatory for opening a
  session and joining a channel, so a leaked player JWT can no longer be used to log in or join.
* TURN requires MESSAGE-INTEGRITY with long-term credentials derived from the API-issued
  time-limited username/password (HMAC-SHA1 shared secret, RFC 5389 §15.4); nonces, allocation
  ownership, permissions and channel bindings are enforced. No open relay.
* Public API errors never leak internal details; details are logged at `debug`.
* Production mode (`AURIX__SERVER__ENVIRONMENT=production`) refuses to start with wildcard CORS,
  placeholder/short secrets, dev DB credentials or a missing public media IP.

---

## Quick start (development)

```bash
docker run -d --name aurix-pg -e POSTGRES_USER=aurix -e POSTGRES_PASSWORD=aurix -e POSTGRES_DB=aurix -p 127.0.0.1:5432:5432 postgres:16-alpine
docker run -d --name aurix-redis -p 127.0.0.1:6379:6379 redis:7-alpine

cargo run --bin aurix-server                # uses configs/default.toml (development mode)
```

Bootstrap the first administrator (allowed only while no admin exists, or with the configured
`AURIX__AUTH__ADMIN_BOOTSTRAP_TOKEN` sent as `x-bootstrap-token`):

```bash
curl -X POST localhost:8080/admin/setup -H 'content-type: application/json' \
  -d '{"email":"root@example.com","password":"<strong password>","display_name":"Root"}'
ADMIN=$(curl -s -X POST localhost:8080/admin/login -H 'content-type: application/json' \
  -d '{"email":"root@example.com","password":"<strong password>"}' | jq -r .token)

# create an app; the response contains the app's first API key (shown once)
curl -X POST localhost:8080/v1/apps -H "authorization: Bearer $ADMIN" -H 'content-type: application/json' -d '{"name":"MyGame"}'
```

Everything after that is done with the app's API key (`x-api-key`) from your game backend:

```bash
# channel
curl -X POST localhost:8080/v1/channels -H "x-api-key: $KEY" -H 'content-type: application/json' \
  -d '{"name":"lobby","config":{"channel_type":"positional","max_participants":64}}'

# player token (short-lived JWT, scoped to channels + roles)
curl -X POST localhost:8080/v1/tokens -H "x-api-key: $KEY" -H 'content-type: application/json' \
  -d '{"external_id":"steam:7656119","display_name":"Alice",
       "channels":[{"channel_id":"<channel uuid>","speak":true,"receive":true}]}'

# one-time action token (login | join | kick | mute | unmute), single use, 90 s
curl -X POST localhost:8080/v1/tokens/action -H "x-api-key: $KEY" -H 'content-type: application/json' \
  -d '{"action":"join","external_id":"steam:7656119","channel_id":"<channel uuid>","speak":true}'
# kick/mute/unmute additionally need moderation:write, a target and the acting user:
#   {"action":"kick","user_id":"<moderator uuid>","channel_id":"<channel>","target_user_id":"<player>"}
```

### Client flow

1. Connect to `ws://host:8081/ws` with the player JWT (`Authorization: Bearer`, the
   `Sec-WebSocket-Protocol: aurix, bearer.<jwt>` sub-protocol for browsers, or `?token=` as last
   resort) or with a one-time `login` action token. The server replies
   `SessionInitAck { session_id, ssrc, media_addr, media_key, resume_token, resume_grace_ms }`.
2. **Native clients**: send an authenticated `SessionBind` datagram to `media_addr`
   (`AurixPacket::session_bind(...).encode_authenticated(media_key)`), wait for
   `SessionBindAck` / `MediaBound`, then `ChannelJoin { channel_id, token }` over WebSocket and
   start sending `Audio` packets (Opus, 20 ms, `encode_authenticated`). `token` is either the
   player JWT (must list the channel) or a one-time `join` action token for exactly that channel.
3. **Browsers**: `ChannelJoin`, then `WebRtcOffer { sdp }` → `WebRtcAnswer { sdp }`; ICE
   servers come from `GET /v1/me/turn-credentials`.
4. Positional channels: send `PositionUpdate` (own position only); `SpeakingState`,
   `ParticipantJoined/Left`, `RecordingNotification`, `Kick`, `SessionClose` arrive as events.
5. In-game moderation: `ModerateParticipant { channel_id, user_id, action, token }` with a
   `kick`/`mute`/`unmute` action token minted by your backend for the acting player →
   `ModerateParticipantAck` (or `Error`). The token binds actor, channel and target, so a
   client cannot redirect it.
5. **Reconnect**: if the WebSocket drops without a Close frame the session stays alive for
   `resume_grace_ms` (`AURIX__SERVER__SESSION_RESUME_GRACE_SECS`, default 30). Reconnect with the
   same JWT plus `X-Aurix-Resume: <session_id>.<resume_token>` (browsers: sub-protocol
   `resume.<session_id>.<resume_token>`) and the server answers `SessionInitAck { resumed: true }`
   with the same session, SSRC and media key, followed by one `ChannelJoinAck` per channel still
   joined — peers never see a leave. Tokens are one-time (rotated on every ack), bound to the
   user/app of the JWT and useless once the server closed the session (kick/ban/shutdown) or the
   grace period expired; then the same handshake simply yields a fresh session. Native clients
   re-send `SessionBind` from their (possibly new) UDP port. The SDKs do this automatically with
   exponential backoff and expose `Recovering` / `Recovered` / `FailedToRecover` events.
6. **Receiver-local mute / volume / block**: `SetParticipantMute { user_id, channel_id?, muted }`
   silences one player for *you only* (in one channel or, with `channel_id: null`, everywhere),
   `SetParticipantVolume { user_id, volume }` scales them for you (`0.0`–`2.0`, `1.0` = unity,
   multiplied with positional attenuation into the per-packet volume byte / WebRTC gain), and
   `SetUserBlock { user_id, blocked }` is a persistent, mutual cross-mute stored in `user_blocks`
   per application: neither side hears the other in any channel, on any node, in any future
   session. All three are enforced on the server before media is forwarded (native, WebRTC and
   cascade paths alike) and never reach the affected sender — no `MuteStateChanged` is
   broadcast, unlike the sender-side `SetMute` and the moderator mute. A fresh session starts
   with `ReceiverPreferences { blocked_users, local_mutes, volumes }` (blocks come from the
   database; mutes/volumes are replayed by the SDKs), the blocker gets `UserBlockChanged` acks.
7. **Text chat (lite)**: `ChatSend { channel_id, text, metadata?, client_ref? }` to a channel you
   are a member of, `ChatSendDirect { user_id, … }` to a user online in the same application
   (live-only: no offline delivery, no history), `ChatTyping { channel_id, typing }`. Everyone
   entitled to see the message — including the sender — receives one
   `ChatMessageReceived { message }` with a server-assigned `id`/`sent_at`; the sender's copy
   also carries the `client_ref`, nobody else's does. A rejected send comes back as
   `Error { code, message, client_ref }` so the client can fail exactly that optimistic message
   (`AUTH_DENIED`, `USER_MUTED`, `VALIDATION_ERROR`, `USER_OFFLINE`, `RATE_LIMIT_EXCEEDED`,
   `MESSAGE_BLOCKED`, `CHAT_DISABLED`). Typing is throttled per session+channel and never echoed
   to its origin. Persistent blocks, tenant boundaries and (by default) the moderator mute apply
   to text exactly as to voice; delivery crosses nodes through the same Redis event bus as
   presence events. Metadata is arbitrary JSON (`/`-commands, map pings, …) and counts toward
   `chat.max_message_bytes`.

The full message set is in `crates/aurix-common/src/protocol.rs` (`ControlMessage`).

---

## REST API overview

| Auth | Endpoint | Purpose |
|---|---|---|
| none | `GET /health`, `GET /ready` | liveness / readiness (DB + Redis) |
| bootstrap | `POST /admin/setup` | first admin (see above) |
| admin JWT | `POST /admin/login`, `GET /admin/me`, `POST /admin/admins`, `GET /admin/audit-log` | operators |
| admin JWT | `POST|GET /v1/apps`, `GET|DELETE /v1/apps/:id`, `POST /v1/apps/:id/rotate-key`, `GET /v1/nodes` | tenants & fleet |
| API key | `POST /v1/tokens` | issue player JWT |
| API key | `POST /v1/tokens/action` | one-time `login`/`join`/`kick`/`mute`/`unmute` token (moderation actions also need `moderation:write`) |
| API key | `POST /v1/turn/credentials` | TURN credentials for a user |
| API key | `POST|GET /v1/channels`, `GET|DELETE /v1/channels/:id`, `PUT …/config`, `GET …/participants` | channels |
| API key | `GET /v1/users`, `GET /v1/users/:id`, `POST /v1/users/:id/unban` | users |
| API key | `GET|POST /v1/users/:id/blocks`, `DELETE /v1/users/:id/blocks/:blocked_id` | persistent cross-mute (applied to live sessions on every node) |
| API key | `POST /v1/channels/:id/messages`, `POST /v1/users/:id/messages` | server/system text message into a channel or to one user's live sessions (`chat:write`; sender is the nil user id, bypasses the content filter; fire-and-forget — dropped if nobody is online unless persisted) |
| API key | `GET /v1/channels/:id/messages`, `GET /v1/users/:id/messages` | history, newest first, `?before=<rfc3339>&limit=1..200` — only when `chat.persist = true`, otherwise `404 NOT_FOUND` (`chat:read`) |
| API key | `POST /v1/moderation/{ban,mute,kick,report}`, `GET /v1/moderation/bans`, `POST …/bans/:id/revoke`, `GET /v1/moderation/events[/:id]`, `POST …/:id/resolve` | moderation |
| API key | `POST /v1/recordings/start`, `POST /v1/recordings/:id/stop`, `GET /v1/recordings[/:id]`, `GET …/:id/download`, `DELETE …/:id` | recording |
| API key | `POST|GET /v1/api-keys`, `DELETE /v1/api-keys/:id`, `GET /v1/audit-log`, `GET /v1/analytics` | account |
| player JWT | `GET /v1/me/turn-credentials`, `POST /v1/me/reports`, `POST /v1/me/recordings/:id/consent`, `POST /v1/webrtc/offer` | end users |

API-key permissions: `*`, `tokens:issue`, `turn:issue`, `channels:read|write`, `users:read|write`,
`moderation:read|write`, `recordings:read|write`, `chat:read|write`, `keys:manage`. A key can only mint keys with a
subset of its own permissions. Errors are `{"error":{"code":"…","message":"…"}}`.

---

## Production deployment

### Docker Compose (single node)

```bash
cp .env.example .env        # fill in secrets: openssl rand -base64 48
docker compose up -d --build
docker compose --profile observability up -d   # + Prometheus & Grafana on localhost
```

The image runs as uid 10001 with a read-only root filesystem, drops all capabilities and has a
`/ready` healthcheck. Secrets are only taken from the environment; nothing sensitive is baked in.

### Configuration

`configs/default.toml` holds **development** defaults. Override anything with environment
variables `AURIX__<SECTION>__<KEY>` (lists are comma-separated), or pass `--config path/to/file`.
Set `AURIX__SERVER__ENVIRONMENT=production` for strict validation. Key settings:

| variable | notes |
|---|---|
| `AURIX__DATABASE__URL`, `AURIX__REDIS__URL` | Redis is optional for a single node but required for multi-node events / distributed rate limits |
| `AURIX__AUTH__JWT_SECRET` | ≥ 32 random bytes; or `AURIX__AUTH__JWT_PUBLIC_KEY_PATH` for RS256 |
| `AURIX__AUTH__ADMIN_BOOTSTRAP_TOKEN` | allows `/admin/setup` after the first admin exists; unset after use |
| `AURIX__AUTH__ACTION_TOKEN_TTL_SECS`, `AURIX__AUTH__ACTION_TOKEN_MAX_TTL_SECS` | default (90) and maximum (600) lifetime of one-time action tokens |
| `AURIX__AUTH__REQUIRE_ACTION_TOKENS` | `true` — WebSocket login and `ChannelJoin` accept only one-time action tokens (player JWTs stay valid for REST) |
| `AURIX__MEDIA__EXTERNAL_IP` | public IP advertised to clients for UDP media |
| `AURIX__MEDIA__REQUIRE_PACKET_AUTH` | `true` (default) — drop unauthenticated media |
| `AURIX__MEDIA__RX_WORKERS` | concurrent UDP receive workers on the SFU socket; `0` (default) = CPU count clamped to 2–8 |
| `AURIX__MEDIA__CASCADE_SECRET`, `AURIX__MEDIA__CASCADE_PEERS` | shared secret + allow-list for SFU↔SFU relay |
| `AURIX__TURN__*` | `ENABLED`, `EXTERNAL_IP`, `REALM`, `AUTH_SECRET` (≥ 32 bytes), `MIN_PORT`/`MAX_PORT` relay range |
| `AURIX__SERVER__CORS_ORIGINS` | explicit origins; `*` is rejected in production |
| `AURIX__SERVER__TRUSTED_PROXIES` | CIDRs whose `X-Forwarded-For` is trusted for rate limiting / audit |
| `AURIX__SERVER__SESSION_RESUME_GRACE_SECS` | how long a dropped session waits for a resume (default 30, `0` disables; must be ≤ `AURIX__MEDIA__SESSION_TIMEOUT_SECS`) |
| `AURIX__SERVER__TLS_CERT_PATH`, `AURIX__SERVER__TLS_KEY_PATH` | native TLS for API + WebSocket (PEM). Otherwise terminate TLS on your proxy |
| `AURIX__RECORDING__*` | `ENABLED`, `STORAGE_PATH`, `RETENTION_DAYS`, `REQUIRE_CONSENT`, `ENCRYPTION_ENABLED` + `ENCRYPTION_KEY` (≥ 32 chars), S3 settings |
| `AURIX__RATE_LIMITING__*` | per-IP / per-key limits (Redis-backed when available) |
| `AURIX__CHAT__*` | `ENABLED` (default `true`), `MAX_MESSAGE_BYTES` (1024, text + metadata, ≤ 16384), `MESSAGES_PER_SECOND`/`MESSAGE_BURST` (2 / 10 per session), `TYPING_INTERVAL_MS` (1500), `SERVER_MUTE_BLOCKS_TEXT` (`true`), `FILTER_WEBHOOK` + `FILTER_TIMEOUT_MS` (1500) + `FILTER_FAIL_OPEN` (`false`), `PERSIST` (`false`) + `RETENTION_DAYS` (30) |

#### Chat content filter webhook

When `chat.filter_webhook` is set, every player message (not system messages) is `POST`ed there
as JSON before delivery:

```json
{"app_id":"…","channel_id":"…"|null,"from_user_id":"…","to_user_id":null|"…",
 "display_name":"alice","text":"…","metadata":{…}|null}
```

Reply `200` with `{"action":"allow"}`, `{"action":"replace","text":"…"}` (deliver the substitute)
or `{"action":"block","reason":"…"}` (sender gets `MESSAGE_BLOCKED` with that reason). A non-2xx
status, invalid body or timeout blocks the message unless `chat.filter_fail_open = true`. Storage
is off by default; with `chat.persist = true` messages land in `chat_messages` (post-filter text,
per application) and are swept hourly after `chat.retention_days`.

### Network / firewall

| port | proto | purpose |
|---|---|---|
| 8080 | TCP | REST API (behind TLS proxy or native TLS) |
| 8081 | TCP | WebSocket signalling (same TLS advice) |
| 10000 | UDP | native AURX media + WebRTC ICE/DTLS-SRTP |
| 3478 | UDP+TCP | TURN/STUN |
| `turn.min_port`–`turn.max_port` | UDP | TURN relay allocations — must be reachable from the internet |
| 4040 | TCP | Prometheus metrics — **internal only** |

For large TURN relay ranges run the container with `network_mode: host` instead of publishing
thousands of ports.

### TLS

Either terminate TLS at a reverse proxy (nginx / Caddy / cloud LB — set `trusted_proxies` so
client IPs are correct), or point `tls_cert_path`/`tls_key_path` at PEM files and expose 8080/8081
directly. Media (UDP) is protected by AURX v2 encryption+HMAC / DTLS-SRTP regardless of TLS.

### Scaling out

* Run N `aurix-server` instances against the same PostgreSQL + Redis. Each registers itself in
  `media_nodes` and heartbeats; `GET /v1/nodes` shows the fleet. Nodes silent for 30 s are marked
  unhealthy, and forgotten after 24 h.
* Channel events (join/leave/mute/ban/recording) are replicated through Redis pub/sub with an
  origin node id, so a node never re-applies its own events.
* Channels spanning nodes are relayed SFU-to-SFU ("cascade") **automatically**: set the same
  `media.cascade_secret` on every node and nothing else. Each node advertises its cascade UDP port
  (`media.port + 1`) in `media_nodes`; every `media.cascade_discovery_interval_ms` (default 3 s)
  and immediately after a remote join/leave event, a node reconciles the topology from the
  database — accepted peers are the healthy nodes, and each channel is forwarded only to the nodes
  that actually hold live memberships for it. Relayed packets travel inside a `Relay` envelope
  encrypted and authenticated with keys derived from `cascade_secret` (the client's plaintext is
  never on the wire between nodes) and pass a per-peer anti-replay window; unknown source
  addresses are dropped.
  `media.cascade_peers` remains as an optional static allow-list (e.g. for nodes not in the
  registry) and `media.cascade_discovery=false` returns to fully static full-mesh mode. Node
  addresses must be reachable between nodes on `media.port + 1`/UDP (the `media.external_ip` you
  register is what peers dial).
* Put the API/WS behind a load balancer; UDP media must reach the node the session was created on
  (`media_addr` in `SessionInitAck` already points there).

### Backup & restore

Everything durable lives in PostgreSQL and (optionally) the recording store.

```bash
docker compose exec db pg_dump -U "$POSTGRES_USER" -Fc aurix > aurix-$(date +%F).dump
docker compose exec -T db pg_restore -U "$POSTGRES_USER" -d aurix --clean < aurix-2025-01-01.dump
```

Recordings: back up the `recordings` volume (or use S3 with versioning). If recording encryption
is enabled, the key (`AURIX__RECORDING__ENCRYPTION_KEY`) **must** be backed up separately — files
are unreadable without it. Redis holds only ephemeral state and needs no backup.

Migrations are embedded in the binary and applied at start when `database.run_migrations = true`
(default); the server refuses to start if the schema is behind the compiled migrations.

### Observability

* `GET :4040/metrics` — `aurix_active_sessions`, `aurix_packets_*_total`, `aurix_bytes_*_total`,
  `aurix_api_requests_total{method,path,status}`, `aurix_turn_allocations`, `aurix_rate_limit_hits_total`,
  `aurix_ws_sessions_detached` / `aurix_ws_sessions_resumed_total` (reconnects), …
* Grafana dashboard: `deploy/grafana/dashboards/aurix-overview.json`.
* Logs: JSON (`AURIX__TRACING__LOG_FORMAT=json`), OTLP export via `AURIX__TRACING__OTLP_ENDPOINT`.
* Every privileged action (admin login, app/key changes, bans, kicks, recording access) is written
  to `audit_log` and readable via the API.

---

## Development

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace                                  # unit + in-process integration tests

# live end-to-end (needs a running server, see .github/workflows/ci.yml for the bootstrap steps)
AURIX_E2E_API_KEY=aurx_... cargo test -p aurix-server --test e2e_live -- --ignored --nocapture
```

The live E2E test drives two players through WebSocket + UDP: session bind, channel join,
authenticated audio Alice→Bob, forged-packet rejection, recording of real Opus packets, TURN
allocation with API-issued credentials, and clean-up on disconnect.

System dependencies for building: `pkg-config`, `libssl-dev`, `cmake`, `libopus-dev` (Debian names).

### Load testing

`aurix-loadtest` is a reproducible load generator that behaves like real native clients: it
creates channels and tokens through the REST API (API key required), opens one WebSocket per
session, performs an authenticated `SessionBind` over UDP, joins channels and then streams
sealed (encrypted + authenticated) AURX audio from the configured speakers while every member
opens the downlink with its own session keys (nothing bypasses production validation).

```bash
cargo build --release -p aurix-server -p aurix-loadtest
./target/release/aurix-loadtest \
  --api http://127.0.0.1:8080 --ws ws://127.0.0.1:8081 --metrics http://127.0.0.1:9090/metrics \
  --api-key aurx_...            # or AURIX_LOADTEST_API_KEY
  --sessions 1000 --channels 100 --speakers 2 --pps 50 --duration 30 [--json]
```

The report contains session setup success/latency, packets sent vs. delivered (`expected =
speakers × (members − 1) × frames`), bad-auth count, one-way latency percentiles (a timestamp is
embedded in every payload), control-plane message counts and a before/after diff of the server’s
Prometheus counters (`packets_*`, CPU seconds, RSS). Kernel-side drops show up in
`/proc/net/snmp` → `Udp: RcvbufErrors`; the loadgen host needs `ulimit -n` ≥ 2 × sessions.

Reference run (release build, 8 vCPU host shared with the load generator, PostgreSQL + Redis):

| scenario | in / out pps | delivered | one-way p50 / p99 | server CPU | RSS |
|---|---|---|---|---|---|
| 1000 sessions, 100 channels × 10, 2 speakers | 10k / 90k | 100 % | 2.5 / 4.8 ms | ~0.4 core | 48 MB |
| 2000 sessions, 200 channels × 10, 4 speakers | 40k / 360k | 99.8 % | 2.1 / 4.8 ms | ~1.4 cores | 70 MB |

The second scenario delivered only 79 % (p99 13.6 ms, 167k `RcvbufErrors`) before the SFU used
parallel receive workers and non-blocking sends; that is what `media.rx_workers` controls.

## Client SDKs

| SDK | Path | Media path | Verified by |
|-----|------|-----------|-------------|
| Web (TypeScript) | [`sdk/web`](sdk/web) | WebRTC/Opus via the SFU, WS control plane, demo page | two-browser smoke test (ICE/DTLS, RTP both ways, decoded audio) |
| Unity / .NET (C#) | [`sdk/unity`](sdk/unity) | native AURX v2 over UDP (signed SessionBind, AES-256-CTR + HMAC per packet, replay window), WS control plane | `dotnet test` + headless two-client E2E (`Aurix.Demo`, real Opus via Concentus) |

Both SDKs authenticate with the per-user JWT from `POST /v1/tokens`; API keys stay on your backend.

## Limitations

* Native TLS uses rustls with PEM files; ACME/auto-renewal is left to your proxy.
* No SIP/PSTN gateway, no server-side noise suppression (clients do that).
* Text chat is deliberately "lite": live channel/directed messages and typing only — no offline
  delivery, conversations, read markers or attachments; history is an opt-in per deployment.
* STT/content moderation is a pluggable pipeline; no provider is bundled.
* Cascade is a one-hop mesh between the nodes that host a channel (no hierarchical relay trees);
  it assumes nodes can reach each other directly on `media.port + 1`/UDP.
* No Unreal SDK yet; the protocol is documented in `crates/aurix-common/src/protocol.rs`, and the
  C# `MediaTransport`/`AurxPacket` sources are a compact reference implementation.

## License

Apache-2.0
