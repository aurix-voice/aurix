# Aurix Voice Platform

Open-source, self-hosted voice chat for games and real-time apps — a drop-in alternative to
Vivox / Agora / Photon Voice that you run on your own infrastructure.

* **Two media paths**: a native low-latency UDP protocol (**AURX**, for game engines) and
  **WebRTC** (for browsers / Unity WebGL), bridged by one SFU with per-participant volumes.
* **Multi-tenant**: apps, API keys with fine-grained permissions, per-app users, channels,
  recordings, bans and audit log are strictly isolated.
* **Positional / 3D audio, whisper, command & echo (mic test) channels, audio injection, server mute, kick, ban, reports.**
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

# ad-hoc channel: no POST /v1/channels needed - the grant names the channel and it is created
# on the first join (and removed when the last participant leaves). The id is derived from the
# name, so the response tells you the channel_id up front and every token for the same name
# lands in the same channel. Works in both /v1/tokens and /v1/tokens/action (join).
curl -X POST localhost:8080/v1/tokens -H "x-api-key: $KEY" -H 'content-type: application/json' \
  -d '{"external_id":"steam:7656119","display_name":"Alice",
       "channels":[{"ad_hoc":{"name":"match-8f3a","channel_type":"team","max_participants":10}}]}'

# channel-wide moderation: everyone currently present except the listed users
curl -X POST localhost:8080/v1/moderation/mute-all -H "x-api-key: $KEY" -H 'content-type: application/json' \
  -d '{"channel_id":"<channel uuid>","muted":true,"except":["<game master uuid>"]}'
curl -X POST localhost:8080/v1/moderation/kick-all -H "x-api-key: $KEY" -H 'content-type: application/json' \
  -d '{"channel_id":"<channel uuid>","reason":"round over"}'
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
4. Positional channels: send `PositionUpdate` (own position + orientation; moderators may move
   anyone) — see *Positional / directional audio* below. `SpeakingState`,
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
8. **Audio energy / voice activity**: clients measure their own microphone level and label each
   audio frame with it — native AURX sets `PacketFlags::Energy` and prefixes the payload with one
   RFC 6464-style byte (`0` = full scale … `127` = silence, otherwise `-dBov`;
   `AurixPacket::audio_with_level`), browsers rely on the standard `ssrc-audio-level` RTP header
   extension that WebRTC sends anyway. The server strips the byte before forwarding (listeners
   receive plain Opus), turns labelled frames into `SpeakingStateChanged` only when the level
   reaches `media.speaking_energy_threshold` (unlabelled frames keep counting as speech by
   arrival) and every `media.energy_interval_ms` broadcasts `ChannelEnergy { channel_id, levels:
   [{ user_id, energy }] }` to the channel with the *changed* levels (≥ 3 dB step or a transition
   to/from silence; `energy` is linear `0..1`, a stale level decays to `0`; when a member joins,
   the current levels of everyone already talking on that node are re-sent once so the newcomer
   gets a baseline). Both events travel
   across nodes with the other presence events and are never persisted. The SDKs expose a local
   VAD (`localSpeaking`/`localEnergy` in the Web SDK, `VoiceActivityDetector` + optional
   `GateOnVad` transmit gating in Unity) and the remote `energy` / `OnChannelEnergy` events.
9. **Devices, input gain, speaker mute** are client-side only (nothing on the wire): the Web SDK
   enumerates/selects microphones and speakers (`enumerateAudioDevices`, `setInputDevice` via
   `replaceTrack`, `setOutputDevice` via `setSinkId` where supported), applies `0..4` software
   input gain through Web Audio and mutes/attenuates the remote mix on attached `<audio>`
   elements; the Unity SDK does the same with `Microphone.devices` (hot-swap with fallback to the
   default device), `AudioLevel.ApplyGain` before VAD/Opus and `RemoteMixer.OutputVolume` /
   `OutputMuted` (decoding continues while muted so jitter buffers stay in sync).
10. **Positional / directional audio**: a `positional` channel routes each frame per listener
    from the poses last reported with `PositionUpdate { channel_id, positions: [{ user_id,
    position, orientation }] }` — nothing is forwarded until both the speaker's and the
    listener's pose are known, nothing beyond `positional_config.max_radius`, and the distance
    gain (`near_distance`/`far_distance`/`rolloff`, multiplied with the receiver's participant
    volume and channel focus) travels in the volume byte / WebRTC gain like everywhere else.
    With `positional_config.directional: true` the server also resolves *where the speaker is
    relative to the listener's orientation*: azimuth (`0` ahead, `+π/2` right, `±π` behind) and
    elevation (`+π/2` above), computed from the listener's `forward`/`up` vectors in the game's
    own coordinates — `coordinate_system: left_handed` (default; Unity `X` right/`Y` up/`Z`
    forward, Unreal) or `right_handed` (OpenGL/Three.js/Godot, mirrors left and right).
    Co-located or degenerate poses resolve to "ahead". Native AURX downlink frames then carry
    `PacketFlags::Directional` and two signed bytes (azimuth `-127..127` ≙ `-π..π`, elevation ≙
    `-π/2..π/2`) after the optional volume byte and before the Opus payload
    (`AurixPacket::take_downlink_meta`); the Unity mixer pans each stream with constant-power
    gains. The WebRTC downlink is mixed in stereo on the server (Opus `sprop-stereo=1`, the Web
    SDK offers `stereo=1` so browsers decode both channels; uplinks stay mono). End-to-end
    encrypted native frames are forwarded untouched (no server-side metadata).
11. **Echo channel & audio injection**: a channel with `channel_type: echo` is a microphone
    test — every participant hears *only their own* audio, looped back through the real uplink
    → server → downlink path (encrypted/authenticated native frames or the WebRTC mix, with
    the receiver's own local volume/mute applied), never anybody else's, and echo frames are
    never relayed to other nodes. Put a player alone in one from the settings screen; sharing
    one is harmless (rosters and speaking states are still visible, audio is not). Both SDKs
    can also *inject* audio into the uplink — a test clip in the echo channel, a bot voice, an
    in-game radio — mixed over the microphone or replacing it, looped or one-shot, with its own
    gain (`injectAudio` / `AudioInjector`); the microphone mute silences injected audio too,
    and the server sees ordinary frames (VAD, transmission mode, focus and channel limits apply
    unchanged).

The full message set is in `crates/aurix-common/src/protocol.rs` (`ControlMessage`).

---

## REST API overview

| Auth | Endpoint | Purpose |
|---|---|---|
| none | `GET /health`, `GET /ready` | liveness / readiness (DB + Redis) |
| bootstrap | `POST /admin/setup` | first admin (see above) |
| admin JWT | `POST /admin/login`, `GET /admin/me`, `POST /admin/admins`, `GET /admin/audit-log`, `POST /admin/retention/sweep` | operators (`sweep` runs one retention pass now, superadmin; `409` while another node holds the sweep lock) |
| admin JWT | `POST|GET /v1/apps`, `GET|DELETE /v1/apps/:id`, `POST /v1/apps/:id/rotate-key`, `GET /v1/nodes` | tenants & fleet |
| API key | `POST /v1/tokens` | issue player JWT; a channel grant is `{"channel_id":…}` or `{"ad_hoc":{"name":…,"channel_type":…,"max_participants":…}}` (created on first join, dropped when empty; `max_participants` is clamped to the app limit, creation counts against the app's channel quota) |
| API key | `POST /v1/tokens/action` | one-time `login`/`join`/`kick`/`mute`/`unmute` token (moderation actions also need `moderation:write`; `join` accepts `ad_hoc` too) |
| API key | `POST /v1/turn/credentials` | TURN credentials for a user |
| API key | `POST|GET /v1/channels`, `GET|DELETE /v1/channels/:id`, `PUT …/config`, `GET …/participants` | channels |
| API key | `GET /v1/users`, `GET /v1/users/:id`, `POST /v1/users/:id/unban` | users |
| API key | `DELETE /v1/users/:id[?purge_moderation=true]`, `GET /v1/users/:id/export` | erase a user and everything they own / export it as JSON (`users:erase`, `users:export`; see [User erasure, export and retention](#user-erasure-export-and-retention)) |
| API key | `GET|POST /v1/users/:id/blocks`, `DELETE /v1/users/:id/blocks/:blocked_id` | persistent cross-mute (applied to live sessions on every node) |
| API key | `POST /v1/channels/:id/messages`, `POST /v1/users/:id/messages` | server/system text message into a channel or to one user's live sessions (`chat:write`; sender is the nil user id, bypasses the content filter; fire-and-forget — dropped if nobody is online unless persisted) |
| API key | `GET /v1/channels/:id/messages`, `GET /v1/users/:id/messages` | history, newest first, `?before=<rfc3339>&limit=1..200` — only when `chat.persist = true`, otherwise `404 NOT_FOUND` (`chat:read`) |
| API key | `POST /v1/moderation/{ban,mute,kick,report}`, `GET /v1/moderation/bans`, `POST …/bans/:id/revoke`, `GET /v1/moderation/events[/:id]`, `POST …/:id/resolve` | moderation |
| API key | `POST /v1/moderation/{mute-all,kick-all}` | channel-wide server mute / kick of everyone currently present minus `except: [user ids]`; response lists `affected`, `skipped`, `failed`; every target still gets its own `user.muted`/`user.kicked` event and audit entry plus one `channel_mute_all`/`channel_kick_all` summary |
| API key | `POST /v1/channels/:id/tts`, `GET /v1/tts/voices` | speak a server announcement into a channel with the configured TTS provider (`tts:write`; `{"text":…,"voice":…}` → `request_id`, progress as `tts.status` events) / list voices and limits (see [Transcripts and text-to-speech](#transcripts-and-text-to-speech)) |
| API key | `POST /v1/recordings/start`, `POST /v1/recordings/:id/stop`, `GET /v1/recordings[/:id]`, `GET …/:id/download`, `DELETE …/:id` | recording |
| API key | `GET /v1/channels/:id/audio/streams/pull` (WebSocket), `POST|GET /v1/channels/:id/audio/streams`, `GET|DELETE …/audio/streams/:sid`, `GET /v1/audio/streams` | real-time audio out of the node — pull it over a WebSocket or have the node push it to yours (`audio_streams:read|write`; see [Live audio streams](#live-audio-streams)) |
| API key | `POST|GET /v1/api-keys`, `DELETE /v1/api-keys/:id`, `GET /v1/audit-log`, `GET /v1/analytics` | account |
| API key | `POST|GET /v1/webhooks`, `GET /v1/webhooks/events`, `GET|PATCH|DELETE /v1/webhooks/:id`, `POST …/:id/{rotate-secret,test,resync}`, `GET …/:id/deliveries[/:did]`, `POST …/:id/deliveries/:did/retry` | webhook subscriptions + delivery log (`webhooks:read|write`) |
| API key | `GET /v1/events` (SSE), `GET /v1/events/snapshot` | live server event stream for game servers (`events:read`) |
| player JWT | `GET /v1/me/turn-credentials`, `POST /v1/me/reports`, `POST /v1/me/recordings/:id/consent`, `POST /v1/webrtc/offer` | end users |

API-key permissions: `*`, `tokens:issue`, `turn:issue`, `channels:read|write`, `users:read|write`,
`moderation:read|write`, `recordings:read|write`, `chat:read|write`, `webhooks:read|write`, `events:read`,
`users:erase|export`, `tts:write`, `audio_streams:read|write`, `keys:manage`. A key can only mint keys with a subset of its own permissions. Errors are
`{"error":{"code":"…","message":"…"}}`.

### Webhooks and the event stream

Game servers learn what happens in voice either by **push** (signed HTTP webhooks) or by
**streaming** (`GET /v1/events`, server-sent events). Both carry the same envelope and the same
event ids, so a consumer can mix them and de-duplicate:

```json
{"id":"<uuid, stable per event>","type":"participant.joined","app_id":"…",
 "created_at":"2026-…Z","data":{"channel_id":"…","user_id":"…","display_name":"…","session_id":"…"}}
```

Event types (`GET /v1/webhooks/events` lists them): `channel.created|destroyed|activated|deactivated`
(activated = first participant in, deactivated = last one out — also emitted for channels a
crashed node left behind), `participant.joined|left|muted|unmuted|kicked`, `user.banned`,
`user.block_changed`, `moderation.event`, `recording.started|stopped|consent_required`,
`audio_stream.started|stopped`, `quality.alert`, `chat.message`. `participant.typing`, `participant.speaking` and `channel.energy`
are high-frequency UX signals: SSE delivers them only when named in `?types=`, webhooks refuse them.

**Webhooks.** `POST /v1/webhooks {"url","events":["*"]|[…],"description"}` returns the signing
secret once (`whsec_…`; `POST …/rotate-secret` issues a new one). Every delivery is a JSON `POST`
with headers `X-Aurix-Event`, `X-Aurix-Delivery-Id`, `X-Aurix-Webhook-Id`,
`X-Aurix-Attempt` and

```
X-Aurix-Signature: t=<unix seconds>,v1=<hex HMAC-SHA256(secret, "<t>.<raw body>")>
```

Verify by recomputing the HMAC over the *raw* body and rejecting `t` older than a few minutes
(reference implementation: `aurix_control::webhooks::verify_signature`).
Answer any 2xx within `webhooks.timeout_ms` (5 s). Deliveries are queued in PostgreSQL (so they
survive restarts and are shared by every node), retried on `webhooks.retry_delays_secs`
(`5, 30, 120, 600, 1800, 3600, 7200`) with the same delivery id and event id, then marked `failed`;
`GET …/deliveries` shows the log, `POST …/deliveries/:id/retry` re-sends one, `POST …/test` sends a
`webhook.test`, `POST …/resync` sends a `webhook.resync` with every live channel and its members
(use it after downtime instead of replaying history). A subscription's `consecutive_failures`,
`last_status` and `last_error` are visible on `GET /v1/webhooks/:id`. URLs must be `https://` in
production and may not point at private, loopback or link-local addresses (DNS is resolved and
pinned per delivery; redirects are not followed) — `webhooks.require_https` /
`webhooks.allow_private_urls` relax this for development.

**SSE.** `GET /v1/events[?types=a.b,c.d]` with an API key streams `event:`/`id:`/`data:` frames,
opening with `stream.open` and sending a keep-alive comment every `webhooks.sse_keepalive_secs`.
A `lagged` frame (`{"dropped": n}`) means the consumer fell behind the broadcast buffer — fetch
`GET /v1/events/snapshot` (live channels + members of your app) to resynchronise. Both endpoints
only ever carry the caller's tenant.

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
| `AURIX__MEDIA__SPEAKING_TIMEOUT_MS`, `AURIX__MEDIA__SPEAKING_ENERGY_THRESHOLD`, `AURIX__MEDIA__ENERGY_INTERVAL_MS` | speaking indicator hangover (400), linear RMS level a labelled frame must reach to count as speech (0.01 ≈ −40 dBov), period of `ChannelEnergy` reports (200; `0` disables them) |
| `AURIX__MEDIA__CASCADE_SECRET`, `AURIX__MEDIA__CASCADE_PEERS` | shared secret + allow-list for SFU↔SFU relay |
| `AURIX__TURN__*` | `ENABLED`, `EXTERNAL_IP`, `REALM`, `AUTH_SECRET` (≥ 32 bytes), `MIN_PORT`/`MAX_PORT` relay range |
| `AURIX__SERVER__CORS_ORIGINS` | explicit origins; `*` is rejected in production |
| `AURIX__SERVER__TRUSTED_PROXIES` | CIDRs whose `X-Forwarded-For` is trusted for rate limiting / audit |
| `AURIX__SERVER__SESSION_RESUME_GRACE_SECS` | how long a dropped session waits for a resume (default 30, `0` disables; must be ≤ `AURIX__MEDIA__SESSION_TIMEOUT_SECS`) |
| `AURIX__SERVER__TLS_CERT_PATH`, `AURIX__SERVER__TLS_KEY_PATH` | native TLS for API + WebSocket (PEM). Otherwise terminate TLS on your proxy |
| `AURIX__RECORDING__*` | `ENABLED`, `STORAGE_PATH`, `RETENTION_DAYS`, `REQUIRE_CONSENT`, `ENCRYPTION_ENABLED` + `ENCRYPTION_KEY` (≥ 32 chars), S3 settings |
| `AURIX__RATE_LIMITING__*` | per-IP / per-key limits (Redis-backed when available) |
| `AURIX__CHAT__*` | `ENABLED` (default `true`), `MAX_MESSAGE_BYTES` (1024, text + metadata, ≤ 16384), `MESSAGES_PER_SECOND`/`MESSAGE_BURST` (2 / 10 per session), `TYPING_INTERVAL_MS` (1500), `SERVER_MUTE_BLOCKS_TEXT` (`true`), `FILTER_WEBHOOK` + `FILTER_TIMEOUT_MS` (1500) + `FILTER_FAIL_OPEN` (`false`), `PERSIST` (`false`) + `RETENTION_DAYS` (30) |
| `AURIX__RETENTION__*` | `ENABLED` (`true`), `SESSIONS_DAYS` (90), `MODERATION_EVENTS_DAYS` (365, resolved cases only), `AUDIT_LOG_DAYS` (0 = keep), `ANALYTICS_DAYS` (400), `TOMBSTONES_DAYS` (30, must cover the longest token lifetime), `INACTIVE_USERS_DAYS` (0 = never auto-erase), `BATCH_SIZE` (5000), `INTERVAL_SECS` (3600, ≥ 60) — see [User erasure, export and retention](#user-erasure-export-and-retention) |
| `AURIX__WEBHOOKS__*` | `ENABLED` (`true`), `TIMEOUT_MS` (5000), `RETRY_DELAYS_SECS` (`5,30,120,600,1800,3600,7200`), `CONCURRENCY` (16), `BATCH_SIZE` (100), `MAX_PENDING_PER_SUBSCRIPTION` (10000 — older events are dropped for a dead endpoint), `RETENTION_HOURS` (72, delivery log), `MAX_SUBSCRIPTIONS_PER_APP` (20), `REQUIRE_HTTPS` / `ALLOW_PRIVATE_URLS` (default: strict in production), `SSE_KEEPALIVE_SECS` (15) |
| `AURIX__STT__*` | `ENABLED` (`false`), `ENDPOINT` (OpenAI-compatible `/v1/audio/transcriptions`), `API_KEY`, `MODEL`, `LANGUAGE` (unset = auto-detect), `SEGMENT_SECS` (3.0), `SILENCE_FLUSH_MS` (700), `MIN_SEGMENT_MS` (400), `TIMEOUT_MS` (15000), `MAX_CONCURRENT_REQUESTS` (8), `INCLUDE_WORDS` (`false`) — see [Transcripts and text-to-speech](#transcripts-and-text-to-speech) |
| `AURIX__TTS__*` | `ENABLED` (`false`), `ENDPOINT` (OpenAI-compatible `/v1/audio/speech`, WAV), `API_KEY`, `MODEL`, `VOICES` (`alloy`), `DEFAULT_VOICE`, `ALLOW_CLIENT_REQUESTS` (`true`), `MAX_TEXT_CHARS` (500), `MAX_AUDIO_SECS` (30), `TIMEOUT_MS` (15000), `MAX_CONCURRENT_REQUESTS` (4), `MAX_QUEUED_PER_SESSION` (3), `MAX_QUEUED_PER_CHANNEL` (8), `REQUESTS_PER_MINUTE_PER_SESSION` (10) |

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

### User erasure, export and retention

Aurix keeps per-user data only in PostgreSQL, Redis and the recording store, so a "right to be
forgotten" request is one call:

```bash
curl -X DELETE -H "X-API-Key: $KEY" "$API/v1/users/$USER_ID"            # 404 if unknown
curl -X DELETE -H "X-API-Key: $KEY" "$API/v1/users/$USER_ID?purge_moderation=true"
```

`DELETE /v1/users/:id` (`users:erase`) is tenant-scoped and, in one transaction after the
recording store has been purged:

* closes the user's live sessions on **every** node (`user.deleted` travels over the event bus;
  other participants get the usual `participant_left`) and drops their Redis state;
* removes sessions, channel memberships, chat messages sent or received, cross-mute blocks in
  both directions, recording rows **and files/objects** (an in-progress recording is stopped
  first and its file discarded);
* anonymises reports the user *filed* (the reporter becomes the nil user) but keeps moderation
  events and bans *about* them, so an active ban survives the deletion — pass
  `purge_moderation=true` to remove those too;
* leaves a **tombstone** `(app_id, user_id, deleted_at)` and deletes the user row.

Any player JWT, action token or resume token minted **before** `deleted_at` is refused from then
on (WebSocket and REST alike), even if it has not expired. A token minted afterwards for the same
external id simply creates a fresh user with a new id — the game decides whether that is allowed.
Tombstones are dropped after `retention.tombstones_days`, which the config validator forces to be
at least as long as the longest session / action token lifetime. The response reports what was
removed; the action is audited as `user_deleted` and published as `user.deleted`.

`GET /v1/users/:id/export` (`users:export`) returns JSON with the profile, sessions, channel
memberships, chat messages (when persisted), blocks, bans, moderation events (as target and as
reporter) and recording metadata (not the audio). Each collection is capped at 10 000 rows;
`truncated` lists the ones that hit the cap. Exports are audited (`user_data_exported`).

**Retention sweep.** Every node runs the sweep every `retention.interval_secs`, but only one
performs it at a time (PostgreSQL advisory lock), in batches of `batch_size` rows, off the media
path. Rules with `0` days are skipped:

| rule | what goes |
|---|---|
| `sessions_days` | sessions **disconnected** longer ago than this, with their memberships (open sessions are never touched) |
| `moderation_events_days` | **resolved** events older than this; open cases are kept indefinitely |
| `audit_log_days` | audit entries (default `0`: keep forever — most compliance regimes want them) |
| `analytics_days` | analytics snapshots |
| `tombstones_days` | deletion tombstones (after which old tokens can no longer be recognised — hence the validator) |
| `inactive_users_days` | full erasure (as above, `purge_moderation=false`) of users not seen for this long who are not banned and have no open session; default `0` = off |

The first pass runs one interval after start; `POST /admin/retention/sweep` forces one and
returns the per-rule counts. Sweeps that removed anything are audited as `retention_sweep`.

**Caveats.** Erasure is not retroactive for backups: a `pg_dump` or object-store version taken
before the request still contains the data — set your backup retention accordingly. Webhook
deliveries already queued that mention the user are delivered as-is (the events happened), and
nothing is recalled from game servers that consumed the event stream.

### Transcripts and text-to-speech

Both features are off by default and point at **HTTP providers you run yourself** (no cloud
account is baked in): speech-to-text expects an OpenAI-compatible `POST /v1/audio/transcriptions`
(faster-whisper-server, whisper.cpp server, …) and text-to-speech an OpenAI-compatible
`POST /v1/audio/speech` returning WAV (OpenedAI-Speech/Piper, Kokoro-FastAPI, …). Provider keys
stay on the node: they never appear in events, REST responses or logs.
`cargo run -p aurix-server --example mock_speech` starts a stand-in for both during development.

**Transcripts.** A channel opts in with `"transcription": true` in its config; nothing else is
ever sent to STT, and neither are end-to-end-encrypted frames (the node cannot read them). The
SFU decodes each speaker's Opus, cuts segments of `stt.segment_secs` (earlier after
`silence_flush_ms` of silence, dropping anything shorter than `min_segment_ms`) and pushes the
result as `Transcript {id, channel_id, user_id, text, language, started_at, duration_ms, words}`
to the speaker and to the members of that channel who would hear them (local mutes, blocks and
zero gain suppress captions too), on every node. Clients opt out/in with
`SetTranscripts {enabled}` (Web `setTranscripts()`, Unity `SetTranscriptsAsync()`); the
`ChannelJoinAck.transcription` flag tells them whether a channel is captioned. Game servers get the
same segments as `channel.transcript` (webhooks / SSE, only when asked for). Transcripts are
**ephemeral** — the server stores nothing; keep them yourself if your policy requires it.

**Text-to-speech.** A participant sends `TtsSpeak {channel_id?, text, voice?, destination,
client_ref?}` (Web `speak()`, Unity `SpeakAsync()`); `destination` is `channel` (everyone their
microphone would reach — same routing, mutes, blocks, focus and cascade as their voice),
`local` (only themselves, e.g. accessibility read-out) or `both`. The node synthesizes, Opus-encodes
and paces the audio in real time on a **synthetic SSRC** — the participant's SSRC with the top bit
set — so native receivers attribute it to the right user while telling it apart from the microphone
(`participant_voice_ssrc` / Unity `IsSynthesizedSsrc`); browsers get it inside their mixed
WebRTC downlink like any other voice. The requester alone receives `TtsStatus` (`queued → playing → finished | cancelled |
failed`, correlated by `client_ref`) and can `TtsCancel` everything still pending; a closed
connection cancels too. Text runs through the chat content filter when it is destined for the
channel, server-muted participants cannot speak into a channel, and `tts.max_text_chars`,
`max_audio_secs` (longer audio is truncated), per-session/per-channel queue depth and
`requests_per_minute_per_session` bound the cost. Provider failures reach the client as a
sanitized `failed` status.

Operators announce with `POST /v1/channels/:id/tts` (`tts:write`): every node hosting the
channel plays the announcement to its participants on a per-channel system SSRC (no
participant attached), and progress is published as `tts.status` events. `GET /v1/tts/voices`
lists the configured voices and limits.

### Live audio streams

Besides file recordings, a node can hand the audio of a channel to an external service **as it
happens** — your own moderation/toxicity pipeline, a stream overlay, an archival or analytics
sink. It is provider-neutral: the node speaks a small WebSocket protocol and you bridge it to
whatever you run. Off by default; enable with `recording.live.enabled = true` (file recording
`recording.enabled` may stay off).

Two transports, same frames:

* **Pull** — `GET /v1/channels/:id/audio/streams/pull[?format=opus|pcm_s16le&users=<id,id>&label=…]`
  with an API key (`audio_streams:write`) upgrades to a WebSocket; frames flow until you close it.
* **Push** — `POST /v1/channels/:id/audio/streams {"url":"wss://…","headers":{"Authorization":"…"},
  "format":…,"users":[…],"label":…}` makes the node dial your endpoint (custom headers are sent
  on the handshake, never echoed back), reconnect with exponential backoff up to
  `recording.live.max_reconnects` (a fresh `hello` after each reconnect) and give up with reason
  `push_unreachable`. `GET`/`DELETE …/audio/streams/:sid` show status (`frames_sent`,
  `frames_dropped`, `reconnects`, `state`, per-participant consent) and stop it; header values
  are never returned. URLs with embedded credentials are refused; in production they must be
  `wss://` and public (`recording.live.require_tls` / `allow_private_urls` relax this for
  development).

The socket carries JSON text frames for control and binary frames for audio:

```json
{"type":"hello","stream_id":"…","channel_id":"…","format":"opus","sample_rate":48000,
 "channels":1,"frame_ms":20,"frame_version":1,"users":null,"consent_required":true,…}
{"type":"participant","user_id":"…","ssrc":123,"event":"audio_started|consent|left","consent":"accepted"}
{"type":"dropped","frames":12}
{"type":"end","reason":"consumer_disconnected|operator|duration_limit|channel_stopped|push_unreachable|shutdown","frames_sent":…,"frames_dropped":…}
```

Binary frame (36-byte header, big-endian, then the payload; `aurix_recording::live::decode_frame`
is the reference parser):

```
 0  version (1)      1  codec (1 = Opus, 2 = PCM s16le)    2  flags (bit0 gap, bit1 first)   3  reserved
 4  ssrc (u32)       8  rtp timestamp (u32, 48 kHz)       12  server receive time (i64, unix ms)
20  participant user id (16 bytes, RFC 4122)             36  payload
```

`opus` forwards each participant's packets untouched (one 20 ms frame each; the `gap` flag
marks a hole in the RTP timeline so a decoder can run PLC); `pcm_s16le` decodes on the node —
mono 48 kHz, 960 samples per frame — with one Opus decoder per active talker
(`recording.live.allow_pcm = false` disables it). Streams are **per participant, not mixed**;
mixing, transcoding and container formats are your side's job.

Semantics worth knowing:

* **Consent** follows file recording: with `recording.require_consent` every participant is
  `pending` until they answer the `RecordingNotification` (`live: true`) with
  `RecordingConsentResponse`; only `accepted` participants' frames leave the node, `declined`
  ones never do, and the consumer sees each decision as a `participant` control frame. This
  works across cascaded nodes (the decision is relayed to the node hosting the stream).
* **End-to-end-encrypted frames are never streamed** — the node cannot read them; the same
  applies to file recordings and transcripts.
* **Streams are node-local.** Open them against the node that hosts participants of the
  channel (`409` otherwise); participants on other nodes of a cascaded channel are included
  through the relay. Behind a load balancer, pin the operator connection to one node or use push.
* **Backpressure never reaches players.** Each stream buffers `recording.live.queue_frames`
  frames; a slow consumer loses the oldest ones and gets a `dropped` count, the media path is not
  blocked. `max_per_channel` / `max_per_app` bound the number of streams per node,
  `max_duration_secs` (and always `recording.max_recording_duration_secs`) their length.
* **Lifecycle** is announced as `audio_stream.started|stopped` (webhooks/SSE, with the end
  reason and frame counters, without URLs or headers), written to the audit log, and shown to
  players like a recording (`RecordingNotification` with `live: true`). Streams end with the
  channel and on node shutdown (`end` frame); erasing a user drops their per-stream state
  (consent, decoder) while the stream itself keeps running.

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

Recordings: back up the `recordings` volume (or use S3 with versioning). Backups outlive user
erasure (see above) — keep their retention in line with your privacy commitments. If recording encryption
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
| Native core (Rust + C ABI) | [`crates/aurix-client`](crates/aurix-client) | same native AURX v2 path in Rust: Opus/VAD/jitter/mixer, reconnect + resume, all control-plane features; `libaurix_client` + `include/aurix_client.h` for Unreal, mobile and custom engines | unit + fake-server tests, C sample compiled/linked/run in CI, live two-client E2E (`cargo test -p aurix-client --test e2e_live`) |
| Unreal Engine 5.3+ (C++/Blueprint) | [`sdk/unreal`](sdk/unreal) | `AurixVoice` plugin over the `aurix-client` C ABI: `UAurixVoiceSubsystem` with typed Blueprint events, `AudioCapture` microphone bridge, procedural playback wave; static-Opus native library staged by `sdk/unreal/scripts/build_native.*` | native library build for Linux + ABI-reference test in CI; UHT/engine compile **not** run here (no Unreal in the dev environment — see the [README](sdk/unreal/README.md)) |

All SDKs authenticate with the per-user JWT from `POST /v1/tokens`; API keys stay on your backend.

## Limitations

* Native TLS uses rustls with PEM files; ACME/auto-renewal is left to your proxy.
* No SIP/PSTN gateway, no server-side noise suppression (clients do that).
* Text chat is deliberately "lite": live channel/directed messages and typing only — no offline
  delivery, conversations, read markers or attachments; history is an opt-in per deployment.
* STT/TTS talk to OpenAI-compatible HTTP servers you host; no speech model ships with Aurix, and
  transcripts are not stored server-side.
* Live audio streams are per participant (no server-side mix) and node-local; the node does not
  buffer them across a consumer outage beyond `recording.live.queue_frames`.
* Cascade is a one-hop mesh between the nodes that host a channel (no hierarchical relay trees);
  it assumes nodes can reach each other directly on `media.port + 1`/UDP.
* The Unreal plugin has not been compiled against a real engine install yet (none is available
  in the development environment); the first build in your project is the verification step.
  The protocol is documented in `crates/aurix-common/src/protocol.rs`.

## License

Apache-2.0
