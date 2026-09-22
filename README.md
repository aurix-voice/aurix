# Aurix Voice Platform

Open-source, self-hosted voice chat for games and real-time apps — a drop-in alternative to
Vivox / Agora / Photon Voice that you run on your own infrastructure.

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

> Status: 1.2 — production-hardened core (auth, tenant isolation, media auth, TURN, recording) plus
> the full player feature set: reconnect/resume, chat, energy/VAD, positional/directional/ambient
> audio with radius-scoped presence, action tokens, webhooks/SSE, transcripts/TTS, content safety,
> PCMU fallback, QUIC media with 0-RTT resume and connection migration, a TLS tunnel on 443 and a
> WebSocket tunnel for blocked UDP, AURX over WebTransport for browsers, large channels (listeners, per-receiver stream
> caps, a server mix for native clients), cross-node failover with Redis session mirrors, a
> region-aware cascade backbone, stereo/music uplinks, per-participant PCM for engine
> spatialization, per-participant WebRTC tracks with Web Audio HRTF for browsers / Unity WebGL,
> group end-to-end encrypted channels (native + browser), priority speakers with
> attack/hold/release ducking, lip-sync visemes and a voice-effects library in every SDK,
> libopus 1.6 with DRED / neural PLC / OSCE and a loss-adaptive FEC profile,
> fleet-wide rate limits, recording mixdown + post-hoc STT, live translation,
> chat history/offline delivery/read markers, IPv6 dual-stack, admin SSO + roles, usage
> analytics/quotas, and Web / Unity (incl. WebGL) / native (C ABI) / Unreal / Godot SDKs.
> See [Limitations](#limitations) before deploying at scale.

**Documentation**: the full book lives in [`docs/`](docs/src/SUMMARY.md) (`mdbook serve docs`) —
quick start, protocols, SDK guides, operations. The REST contract is
[`api/openapi.json`](api/openapi.json) (OpenAPI 3.1), also served by every node at `GET /openapi.json`.

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
| `aurix-auth` | player JWTs, admin JWTs (Argon2) + OIDC SSO, API keys, TURN credentials |
| `aurix-media` | SFU: sessions, channels, routing, Opus mixing, WebRTC (str0m), cascade |
| `aurix-turn` | RFC 5766/5389 TURN/STUN server (UDP + TCP, long-term credentials) |
| `aurix-control` | control plane: sessions, channels, nodes, events, rate limits, audit |
| `aurix-api` | REST API (axum) |
| `aurix-ws` | WebSocket signalling |
| `aurix-moderation` | bans, mutes, kicks, reports, STT-based content analysis hooks |
| `aurix-recording` | Ogg/Opus writer/reader, consent, retention, encryption, S3, mixdown + post-hoc STT jobs |
| `aurix-metrics` | Prometheus registry |
| `aurix-server` | the binary; wires everything together |
| `aurix-cli` | `aurix` CLI: operator, backend and diagnostic commands over the embedded OpenAPI contract |

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
* **End-to-end encrypted channels** (`"config": {"e2ee": true}`): members seal their Opus
  frames with per-sender group keys (AES-256-CTR + HMAC-SHA256; keys wrapped per peer via
  X25519 + HKDF and rotated on every join/leave) that the node never sees — it authenticates
  membership, relays the wrapped keys and forwards ciphertext, and cannot mix, record,
  transcribe, translate or classify those channels. Supported by the native core (Rust / C ABI /
  Unreal; Godot encrypts through the core but does not bind the fingerprint API yet), Unity
  (managed C#) and browsers via WebCrypto + encoded-frame transforms
  (`RTCRtpScriptTransform` / `createEncodedStreams()`); sessions that cannot encrypt are refused
  with `E2EE_REQUIRED` instead of being served plaintext ([docs](docs/src/features/e2ee.md)).
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
   `SessionInitAck { session_id, ssrc, media_addr, media_addrs, media_key, resume_token, resume_grace_ms }`.
2. **Native clients**: send an authenticated `SessionBind` datagram to `media_addr` (or try the
   `media_addrs` candidates in order — IPv4 first, then IPv6 on dual-stack nodes)
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
   If the node itself is gone, the SDKs rotate through the failover nodes advertised in
   `SessionInitAck.failover` and the node that answers **takes the session over** from its Redis
   mirror (`resumed: true, migrated: true`): same session id and SSRC, new media key and
   endpoint, channels/mutes/codec restored, peers see no leave — see
   [High availability](docs/src/operations/high-availability.md).
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
7. **Text chat**: `ChatSend { channel_id, text, metadata?, client_ref? }` to a channel you
   are a member of, `ChatSendDirect { user_id, … }` to a user of the same application (live-only
   by default; with `chat.persist` an offline recipient gets it queued and replayed on connect,
   and `ChatHistory` / `ChatMarkRead` / `ChatReadMarkers` give cursor-paged history, read markers
   and unread counts, `ChatEdit` / `ChatDelete` / `ChatReact` / `ChatSearch` edit or tombstone
   your own messages, react and search — see [Text chat](docs/src/features/chat.md)),
   `ChatTyping { channel_id, typing }`. Everyone
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
    SDK offers `stereo=1` so browsers decode both channels; uplinks are mono unless the channel
    opts into stereo — see below), and browsers additionally negotiate up to
    `media.webrtc_participant_streams` per-participant tracks they spatialize themselves with
    Web Audio HRTF — see below. End-to-end
    encrypted native frames are forwarded untouched (no server-side metadata).
    `positional_config.roster_radius` / `text_radius` additionally scope *presence* (join
    ack roster, `ParticipantJoined`/`Left`, positions, mute/speaking/energy) and *text*
    (channel chat, typing, transcripts) by distance, independently of the audio range: a
    member is visible/reachable only when both poses are known and within the radius, moving
    in and out of the roster radius is reported as `ParticipantJoined`/`ParticipantLeft` (exit
    10 % wider, so nobody flickers), the sender always gets their own chat echo, and the radii
    are announced in `ChannelJoinAck` (SDK `channelScope` / `GetChannelScope` /
    `channel_scope`). `"ambient": {"max_voices": 4, "ambient_gain": 0.15}` on any channel turns
    on cocktail-party mixing: per receiver the loudest `max_voices` speakers (delivery gain ×
    the RFC 6464 level the sender reported, sticky slots, 400 ms hold, also for speakers
    relayed from other nodes) arrive at full gain and the rest are dimmed to `ambient_gain`.
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
| none | `GET /admin/auth/methods`, `GET /admin/oidc/login`, `GET /admin/oidc/callback` | login page discovery and [OIDC SSO](docs/src/operations/admin-sso.md) |
| admin JWT | `POST /admin/login`, `GET /admin/me`, `POST /admin/me/password`, `POST /admin/logout-all`, `GET|POST /admin/admins`, `GET|PATCH /admin/admins/:id`, `POST /admin/admins/:id/password`, `POST /admin/admins/:id/logout-all`, `GET /admin/audit-log`, `POST /admin/retention/sweep` | operators — roles `viewer` / `moderator` / `admin` / `superadmin` with a [permission matrix](docs/src/concepts/auth.md#administrators); tokens are revoked on role change, deactivation, password change and `logout-all`; `sweep` runs one retention pass now (`409` while another node holds the sweep lock) |
| admin JWT | `POST|GET /v1/apps`, `GET|PATCH|DELETE /v1/apps/:id`, `POST /v1/apps/:id/rotate-key`, `GET /v1/nodes` | tenants & fleet (`apps:read` / `apps:write` / `apps:delete` / `keys:rotate` / `nodes:read`) |
| API key | `POST /v1/tokens` | issue player JWT; a channel grant is `{"channel_id":…}` or `{"ad_hoc":{"name":…,"channel_type":…,"max_participants":…}}` (created on first join, dropped when empty; `max_participants` is clamped to the app limit, creation counts against the app's channel quota) |
| API key | `POST /v1/tokens/action` | one-time `login`/`join`/`kick`/`mute`/`unmute` token (moderation actions also need `moderation:write`; `join` accepts `ad_hoc` too) |
| API key | `POST /v1/turn/credentials` | TURN credentials for a user |
| API key | `GET /v1/regions[?region=…&latitude=…&longitude=…]` | region discovery for your matchmaking: one entry per region with a healthy, non-saturated node that advertises a public `wss://` URL — `ws_url`, `probe_url`, coordinates, `distance_km`, `nodes`, `load_factor`, best first (`tokens:issue`); `POST /v1/tokens` takes the same `region`/`location` hints and returns the chosen `endpoint` (see [Scaling out](#scaling-out)) |
| API key | `POST|GET /v1/channels`, `GET|DELETE /v1/channels/:id`, `PUT …/config`, `GET …/participants` | channels |
| API key | `GET /v1/sessions/:id/stats` | media-plane statistics of one live session on this node: packet/byte counters, the client's last `QualityReport` and the merged `quality` (1–5 bars; see [Network quality and client statistics](#network-quality-and-client-statistics)) (`channels:read`) |
| API key | `GET /v1/users`, `GET /v1/users/:id`, `POST /v1/users/:id/unban` | users |
| API key | `DELETE /v1/users/:id[?purge_moderation=true]`, `GET /v1/users/:id/export` | erase a user and everything they own / export it as JSON (`users:erase`, `users:export`; see [User erasure, export and retention](#user-erasure-export-and-retention)) |
| API key | `GET|POST /v1/users/:id/blocks`, `DELETE /v1/users/:id/blocks/:blocked_id` | persistent cross-mute (applied to live sessions on every node) |
| API key | `POST /v1/channels/:id/messages`, `POST /v1/users/:id/messages` | server/system text message into a channel or to one user's live sessions (`chat:write`; sender is the nil user id, bypasses the content filter; fire-and-forget — dropped if nobody is online unless persisted) |
| API key | `GET /v1/channels/:id/messages`, `GET /v1/users/:id/messages[?peer=]` | history, newest first, opaque `?before=`/`?after=` cursors (`next_before`/`next_after` in the page) and `limit` up to `chat.history_page_max` — only when `chat.persist = true`, otherwise `404 NOT_FOUND` (`chat:read`) |
| API key | `GET|PUT /v1/users/:id/read-markers`, `GET /v1/channels/:id/read-markers` | read markers and unread counts per channel / direct conversation (`chat:read`, `PUT` needs `chat:write`; stored chat only) |
| API key | `GET|PATCH|DELETE /v1/messages/:id`, `PUT|DELETE /v1/messages/:id/reactions/:reaction`, `GET /v1/channels/:id/messages/search?q=`, `GET /v1/users/:id/messages/search?q=[&peer=]` | one stored message with reaction tallies; operator edit / tombstone deletion (no author-window rule, fans out `ChatMessageUpdated`); set / clear a reaction on behalf of `user_id`; full-text search, newest first, `?before=` cursor (`chat:read` / `chat:write`; stored chat only) |
| API key | `POST /v1/moderation/{ban,mute,kick,report}`, `GET /v1/moderation/bans`, `POST …/bans/:id/revoke`, `GET /v1/moderation/events[/:id]`, `POST …/:id/resolve` | moderation |
| API key | `POST /v1/moderation/{mute-all,kick-all}` | channel-wide server mute / kick of everyone currently present minus `except: [user ids]`; response lists `affected`, `skipped`, `failed`; every target still gets its own `user.muted`/`user.kicked` event and audit entry plus one `channel_mute_all`/`channel_kick_all` summary |
| API key | `GET /v1/safety/incidents[/:id]`, `GET …/:id/export`, `GET /v1/safety/users/:id/risk` | content-safety incidents (moderation events `safety.voice`/`safety.text`), self-contained evidence bundle (inline decrypted audio when the key also has `recordings:read`), decayed per-user risk (`moderation:read`; see [Content safety](#content-safety)) |
| API key | `POST /v1/channels/:id/tts`, `GET /v1/tts/voices` | speak a server announcement into a channel with the configured TTS provider (`tts:write`; `{"text":…,"voice":…}` → `request_id`, progress as `tts.status` events) / list voices and limits (see [Transcripts and text-to-speech](#transcripts-and-text-to-speech)) |
| API key | `POST /v1/recordings/start`, `POST /v1/recordings/:id/stop`, `GET /v1/recordings[/:id]`, `GET …/:id/download`, `DELETE …/:id`, `POST /v1/recordings/mixdown`, `POST …/:id/transcribe`, `GET …/:id/transcript[?format=srt\|vtt]` | per-participant recording, channel mixdown (Ogg/Opus or WAV) and post-hoc transcript with speakers |
| API key | `GET /v1/channels/:id/audio/streams/pull` (WebSocket), `POST|GET /v1/channels/:id/audio/streams`, `GET|DELETE …/audio/streams/:sid`, `GET /v1/audio/streams` | real-time audio out of the node — pull it over a WebSocket or have the node push it to yours (`audio_streams:read|write`; see [Live audio streams](#live-audio-streams)) |
| API key | `POST|GET /v1/api-keys`, `PATCH|DELETE /v1/api-keys/:id`, `GET /v1/audit-log` | account |
| API key | `GET /v1/analytics[?from&to&step]`, `GET /v1/analytics/channels[/:id]`, `GET /v1/analytics/sessions[?min_samples&limit]`, `GET /v1/analytics/quota`, `GET /v1/analytics/export[?scope=app\|channels&format=json\|csv]` | usage time series — CCU, session/participant minutes, unique users, media bytes, chat/TTS/STT, average MOS / RTT / jitter / loss and poor-quality share — per application (5-minute buckets) and per channel (hourly), the worst-rated sessions of a range, quota state, raw bucket export for billing (`analytics:read`; see [Usage analytics and quotas](#usage-analytics-and-quotas)) |
| API key | `POST|GET /v1/webhooks`, `GET /v1/webhooks/events`, `GET|PATCH|DELETE /v1/webhooks/:id`, `POST …/:id/{rotate-secret,test,resync}`, `GET …/:id/deliveries[/:did]`, `POST …/:id/deliveries/:did/retry` | webhook subscriptions + delivery log (`webhooks:read|write`) |
| API key | `GET /v1/events` (SSE), `GET /v1/events/snapshot` | live server event stream for game servers (`events:read`) |
| player JWT | `GET /v1/me/turn-credentials`, `GET /v1/me/regions`, `POST /v1/me/reports`, `POST /v1/me/recordings/:id/consent`, `POST /v1/webrtc/offer` | end users (`/me/regions` is the same discovery list for the SDKs' RTT probing) |

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
`user.block_changed`, `moderation.event`, `recording.started|stopped|consent_required|processed`,
`audio_stream.started|stopped`, `quality.alert`, `quality.recovered`, `chat.message`, `chat.message_updated`, `chat.reaction`, `chat.read_marker`. `participant.typing`, `participant.speaking` and `channel.energy`
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
docker compose up -d --build                   # node + operator dashboard on localhost:8090
docker compose --profile observability up -d   # + Prometheus & Grafana on localhost
```

The image runs as uid 10001 with a read-only root filesystem, drops all capabilities and has a
`/ready` healthcheck. Secrets are only taken from the environment; nothing sensitive is baked in.

### Operator dashboard

[`dashboard/`](dashboard) is the operator web UI — a separate service, never served by the node:
a Vite + React SPA on Caddy (`ghcr.io/aurix-voice/aurix-dashboard`, Compose service `dashboard`,
Helm `dashboard.enabled=true` with a standard `Ingress` or a Traefik `IngressRoute`). Caddy
proxies `/v1`, `/admin`, `/health`, `/ready` and `/openapi.json` to the node so the browser stays
same-origin (SSE unbuffered, no CORS entry). Overview / fleet health, nodes with drain / undrain,
applications / keys / limits / webhooks, live channels and sessions with moderation, safety
incidents and evidence, bans, users, stored chat with search, recordings / mixdown / transcripts,
analytics with CSV export, administrators / roles / SSO / audit, and the node's effective
configuration read-only with secrets masked. RU/EN, light / dark. Permissions come from the node
(`AdminPermission`); Playwright E2E runs against a live node and the built image in CI —
[Operator dashboard](docs/src/operations/dashboard.md).

### Configuration

`configs/default.toml` holds **development** defaults. Override anything with environment
variables `AURIX__<SECTION>__<KEY>` (lists are comma-separated), or pass `--config path/to/file`.
Set `AURIX__SERVER__ENVIRONMENT=production` for strict validation. Key settings:

| variable | notes |
|---|---|
| `AURIX__DATABASE__URL`, `AURIX__REDIS__URL` | Redis is optional for a single node but required for multi-node events / distributed rate limits |
| `AURIX__REDIS__SENTINELS`, `AURIX__REDIS__SENTINEL_MASTER` | Redis Sentinel mode (comma-separated sentinel URLs + master name; `REDIS__URL` then only supplies credentials/db/TLS) |
| `AURIX__REDIS__CLUSTER`, `AURIX__REDIS__SHARDED_PUBSUB` | Redis Cluster mode (comma-separated seed URLs carrying credentials/db/TLS; `REDIS__URL` ignored, exclusive with Sentinel); sharded Pub/Sub on by default, `false` for Redis < 7 |
| `AURIX__CLUSTER__SESSION_MIRROR`, `AURIX__CLUSTER__SESSION_MIRROR_TTL_SECS`, `AURIX__CLUSTER__NODE_LOST_AFTER_SECS`, `AURIX__CLUSTER__FAILOVER_ENDPOINTS` | cross-node failover: mirror sessions in Redis (default on, TTL 180), reap nodes silent for 30 s, advertise 3 failover nodes per session |
| `AURIX__AUTH__JWT_SECRET` | ≥ 32 random bytes; or `AURIX__AUTH__JWT_PUBLIC_KEY_PATH` for RS256 |
| `AURIX__AUTH__ADMIN_BOOTSTRAP_TOKEN` | allows `/admin/setup` after the first admin exists; unset after use |
| `AURIX__AUTH__ADMIN_PASSWORD_LOGIN`, `AURIX__AUTH__OIDC__*` | `false` makes the deployment SSO-only; `OIDC__ENABLED`, `ISSUER`, `CLIENT_ID`, `CLIENT_SECRET`, `REDIRECT_URL`, `FRONTEND_REDIRECT`, `SUPERADMIN_EMAILS`, `ALLOWED_DOMAINS`, `DEFAULT_ROLE`, `SYNC_ROLES`, `AUTO_PROVISION` (group → role mapping is best kept in TOML) — [Administrator accounts and SSO](docs/src/operations/admin-sso.md) |
| `AURIX__AUTH__ACTION_TOKEN_TTL_SECS`, `AURIX__AUTH__ACTION_TOKEN_MAX_TTL_SECS` | default (90) and maximum (600) lifetime of one-time action tokens |
| `AURIX__AUTH__REQUIRE_ACTION_TOKENS` | `true` — WebSocket login and `ChannelJoin` accept only one-time action tokens (player JWTs stay valid for REST) |
| `AURIX__MEDIA__EXTERNAL_IP` | public IPv4 advertised to clients for UDP media |
| `AURIX__MEDIA__HOST`, `AURIX__MEDIA__EXTERNAL_IPV6` | `0.0.0.0` (IPv4 only, default), `::` (dual-stack) or an IPv6 literal (IPv6 only); public IPv6 advertised as an additional media candidate — [IPv6](docs/src/operations/deployment.md#ipv6-and-dual-stack) |
| `AURIX__MEDIA__REQUIRE_PACKET_AUTH` | `true` (default) — drop unauthenticated media |
| `AURIX__MEDIA__RX_WORKERS` | concurrent UDP receive workers on the SFU socket; `0` (default) = CPU count clamped to 2–8 |
| `AURIX__MEDIA__SPEAKING_TIMEOUT_MS`, `AURIX__MEDIA__SPEAKING_ENERGY_THRESHOLD`, `AURIX__MEDIA__ENERGY_INTERVAL_MS` | speaking indicator hangover (400), linear RMS level a labelled frame must reach to count as speech (0.01 ≈ −40 dBov), period of `ChannelEnergy` reports (200; `0` disables them) |
| `AURIX__MEDIA__CASCADE_SECRET`, `AURIX__MEDIA__CASCADE_PEERS` | shared secret + allow-list for SFU↔SFU relay |
| `AURIX__TURN__*` | `ENABLED`, `HOST`, `EXTERNAL_IP`, `EXTERNAL_IPV6`, `REALM`, `AUTH_SECRET` (≥ 32 bytes), `MIN_PORT`/`MAX_PORT` relay range |
| `AURIX__SERVER__CORS_ORIGINS` | explicit origins; `*` is rejected in production |
| `AURIX__SERVER__TRUSTED_PROXIES` | CIDRs whose `X-Forwarded-For` is trusted for rate limiting / audit |
| `AURIX__SERVER__SESSION_RESUME_GRACE_SECS` | how long a dropped session waits for a resume (default 30, `0` disables; must be ≤ `AURIX__MEDIA__SESSION_TIMEOUT_SECS`) |
| `AURIX__SERVER__TLS_CERT_PATH`, `AURIX__SERVER__TLS_KEY_PATH` | native TLS for API + WebSocket (PEM). Otherwise terminate TLS on your proxy |
| `AURIX__RECORDING__*` | `ENABLED`, `STORAGE_PATH`, `RETENTION_DAYS`, `REQUIRE_CONSENT`, `ENCRYPTION_ENABLED` + `ENCRYPTION_KEY` (≥ 32 chars), S3 settings |
| `AURIX__RATE_LIMITING__*` | `REQUESTS_PER_SECOND` / `BURST_SIZE` per client IP, `PER_KEY` (each API key's own requests/minute), `CONNECTS_PER_MINUTE`, `CHANNEL_JOINS_PER_MINUTE`, `BLOCK_CHANGES_PER_MINUTE`, `REPORTS_PER_MINUTE`, `ADMIN_LOGIN_PER_MINUTE`; `FLEET` shares the buckets through Redis so limits hold across the whole fleet, `FAIL_CLOSED` refuses instead of falling back to per-node buckets when Redis is down |
| `AURIX__CHAT__*` | `ENABLED` (default `true`), `MAX_MESSAGE_BYTES` (1024, text + metadata, ≤ 16384), `MESSAGES_PER_SECOND`/`MESSAGE_BURST` (2 / 10 per session), `TYPING_INTERVAL_MS` (1500), `SERVER_MUTE_BLOCKS_TEXT` (`true`), `FILTER_WEBHOOK` + `FILTER_TIMEOUT_MS` (1500) + `FILTER_FAIL_OPEN` (`false`), `PERSIST` (`false`) + `RETENTION_DAYS` (30) |
| `AURIX__RETENTION__*` | `ENABLED` (`true`), `SESSIONS_DAYS` (90), `MODERATION_EVENTS_DAYS` (365, resolved cases only), `AUDIT_LOG_DAYS` (0 = keep), `ANALYTICS_DAYS` (400, legacy snapshot rows), `TOMBSTONES_DAYS` (30, must cover the longest token lifetime), `INACTIVE_USERS_DAYS` (0 = never auto-erase), `BATCH_SIZE` (5000), `INTERVAL_SECS` (3600, ≥ 60) — see [User erasure, export and retention](#user-erasure-export-and-retention) |
| `AURIX__USAGE__*` | `ENABLED` (`true`), `FLUSH_INTERVAL_SECS` (15), `AGGREGATE_INTERVAL_SECS` (60), `RETENTION_DAYS` (400, application buckets), `CHANNEL_RETENTION_DAYS` (90), `QUOTA_CACHE_SECS` (30) — see [Usage analytics and quotas](#usage-analytics-and-quotas) |
| `AURIX__QUALITY__*` | `MOS_ALERT_THRESHOLD` (3.1, `0` disables), `MOS_ALERT_PERIODS` (3 consecutive `media.quality_interval_ms` periods), `LOSS_ALERT_PERCENT` (20), `PERSIST_INTERVAL_SECS` (60, checkpoint of the per-session summary; `0` = on disconnect only) — see [Network quality and client statistics](#network-quality-and-client-statistics) |
| `AURIX__WEBHOOKS__*` | `ENABLED` (`true`), `TIMEOUT_MS` (5000), `RETRY_DELAYS_SECS` (`5,30,120,600,1800,3600,7200`), `CONCURRENCY` (16), `BATCH_SIZE` (100), `MAX_PENDING_PER_SUBSCRIPTION` (10000 — older events are dropped for a dead endpoint), `RETENTION_HOURS` (72, delivery log), `MAX_SUBSCRIPTIONS_PER_APP` (20), `REQUIRE_HTTPS` / `ALLOW_PRIVATE_URLS` (default: strict in production), `SSE_KEEPALIVE_SECS` (15) |
| `AURIX__STT__*` | `ENABLED` (`false`), `ENDPOINT` (OpenAI-compatible `/v1/audio/transcriptions`), `API_KEY`, `MODEL`, `LANGUAGE` (unset = auto-detect), `SEGMENT_SECS` (3.0), `SILENCE_FLUSH_MS` (700), `MIN_SEGMENT_MS` (400), `TIMEOUT_MS` (15000), `MAX_CONCURRENT_REQUESTS` (8), `INCLUDE_WORDS` (`false`) — see [Transcripts and text-to-speech](#transcripts-and-text-to-speech) |
| `AURIX__TTS__*` | `ENABLED` (`false`), `ENDPOINT` (OpenAI-compatible `/v1/audio/speech`, WAV), `API_KEY`, `MODEL`, `VOICES` (`alloy`), `DEFAULT_VOICE`, `ALLOW_CLIENT_REQUESTS` (`true`), `MAX_TEXT_CHARS` (500), `MAX_AUDIO_SECS` (30), `TIMEOUT_MS` (15000), `MAX_CONCURRENT_REQUESTS` (4), `MAX_QUEUED_PER_SESSION` (3), `MAX_QUEUED_PER_CHANNEL` (8), `REQUESTS_PER_MINUTE_PER_SESSION` (10) |
| `AURIX__SAFETY__*` | `ENABLED` (`false`), `INCIDENT_THRESHOLD` (0.7), `CATEGORIES` (empty = all), `RISK_HALF_LIFE_SECS` (900), `RISK_ELEVATED` (1.0), `RISK_HIGH` (2.5); `CLASSIFIER__ENDPOINT` + `FORMAT` (`openai_moderation` / `aurix`) + `API_KEY` + `MODEL` + `TIMEOUT_MS` (5000) + `MAX_CONCURRENT_REQUESTS` (8); `VOICE__ENABLED` (`true`), `VOICE__EVIDENCE` (`true`), `VOICE__EVIDENCE_PRE_SEGMENTS` (2), `VOICE__EVIDENCE_RETENTION_DAYS` (30), `VOICE__AUTO_MUTE` / `VOICE__AUTO_KICK` (`never` / `incident` / `elevated` / `high`); `TEXT__ENABLED` (`true`), `TEXT__LEXICON_PATH`, `TEXT__MASK_CHAR` (`*`), `TEXT__CLASSIFY` (`true`), `TEXT__BLOCK_THRESHOLD` (0.9), `TEXT__CONTEXT_MESSAGES` (5), `TEXT__FAIL_OPEN` (`true`), `TEXT__AUTO_MUTE` / `TEXT__AUTO_KICK` — see [Content safety](#content-safety) |

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
| `analytics_days` | legacy `analytics_snapshots` rows (usage buckets follow `[usage]`, see below) |
| `tombstones_days` | deletion tombstones (after which old tokens can no longer be recognised — hence the validator) |
| `inactive_users_days` | full erasure (as above, `purge_moderation=false`) of users not seen for this long who are not banned and have no open session; default `0` = off |

The first pass runs one interval after start; `POST /admin/retention/sweep` forces one and
returns the per-rule counts. Sweeps that removed anything are audited as `retention_sweep`.

**Caveats.** Erasure is not retroactive for backups: a `pg_dump` or object-store version taken
before the request still contains the data — set your backup retention accordingly. Webhook
deliveries already queued that mention the user are delivered as-is (the events happened), and
nothing is recalled from game servers that consumed the event stream.

### Usage analytics and quotas

Every application's consumption is metered into 5-minute buckets (and hourly per-channel
buckets): CCU (`peak_sessions`), session and participant minutes, sessions started, unique
users, active channels, recording seconds, media bytes in/out, chat messages, TTS requests and
characters, STT audio. CCU and minutes are **derived from the session/membership intervals in
PostgreSQL** by one node at a time (advisory lock) — reconnects, resumes and cross-node
failover keep the same session and count once, a node that dies is closed at its last
heartbeat by the fleet reaper and the buckets are re-derived — while the metered counters
(bytes, chat, TTS, STT) are accumulated on each node and flushed every
`usage.flush_interval_secs`. Buckets before `range.finalized_through` are final; the current
one is still accruing.

`GET /v1/analytics?from&to[&step]` returns live counters, range totals and the series (step
auto-picked among 5 min / 1 h / 1 day, or an explicit multiple of 300 s) — totals and every
application point carry a derived `quality {mos_avg, rtt_avg_ms, jitter_avg_ms,
loss_avg_percent, poor_percent, samples}` from the metered quality sums — `GET
/v1/analytics/channels[/:id]` the busiest channels and one channel's hourly series, `GET
/v1/analytics/sessions?from&to&min_samples&limit` the sessions of the range worst average MOS
first with their persisted summaries, `GET
/v1/analytics/export?scope=app|channels&format=json|csv` every raw bucket for your billing or BI
(200 000 rows per call, `truncated` / `X-Aurix-Truncated` when cut), `GET /v1/analytics/quota`
the limits below. Administrators (`analytics:read`) see the fleet under `/admin/analytics/*`.

Two per-application limits complement `max_channels` / `max_participants_per_channel`
(`POST|PATCH /v1/apps`, `0` = unlimited): **`max_concurrent_sessions`** — fleet-wide CCU,
admitted atomically under a per-application advisory lock, the session beyond it is refused with
`QUOTA_EXCEEDED`; **`monthly_participant_minutes`** — channel minutes per UTC month, checked at
every channel join as finalized minutes + the live overlap of open memberships, joins are
refused with `QUOTA_EXCEEDED` until the month rolls over (sessions may still connect, members
already present stay). Rejections count in `aurix_quota_rejections_total{quota}`. Details,
retention and the billing recipe: [Usage analytics and quotas](docs/src/operations/usage-analytics.md).

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

**Live translation.** With `[translation]` (off by default; LibreTranslate-compatible
`POST /translate` or an OpenAI-compatible chat endpoint you host — vLLM, Ollama, llama.cpp) a
listener asks with `SetTranslation {language, spoken_language?, speech}` (Web `setTranslation()`,
Unity `SetTranslationAsync()`, native `aurix_client_set_translation`, Unreal `SetTranslation`)
to receive the captions of the channels they hear in their own language: the node translates
each segment once per requested language and delivers the translated `Transcript` — same id,
speaker and timing, the source attached as `original {text, language}` — only to the listeners
who asked for that language, while the speaker and everyone else keep the original, which is
delivered immediately and never waits for the translator. `speech: true` additionally has the
translation synthesized through `[tts]` and played **privately** to that listener on a
per-channel translator SSRC — no other participant receives a frame. Tenant, membership,
transcript opt-in, mutes, blocks, radius and ambient rules are those of the original caption
and are re-checked after the provider round trip; provider failures, timeouts, over-long
segments or a busy node fall back to the original text; `SessionInitAck.translation`
advertises the offered languages, `max_languages_per_channel`, `max_concurrent_requests`,
`cache_entries` and `timeout_ms` bound the cost, `aurix_translations_total{outcome}` counts it.

**Voice effects, lip-sync, priority speakers.** One effects library — high/low-pass, formant and
pitch shift, ring modulation, distortion, tremolo, static, reverb, with `robot` / `monster` /
`radio` / `helium` / `ghost` presets — runs on the microphone uplink only (after DSP and input
gain, before VAD and the encoder; injected audio, TTS and the downlink are untouched) in the native
core (Rust / C ABI / Unreal / Godot / Unity players) and as an `AudioWorklet` port in the Web SDK /
Unity WebGL. Lip-sync visemes (`sil PP FF SS aa E ih oh ou` + mouth openness) are analysed on the
receiving device from decoded — in E2EE channels decrypted — audio for every heard participant and
the local microphone; nothing about them is sent anywhere (browsers analyse dedicated
per-participant tracks only). Channels with `"ducking": {gain, attack_ms, release_ms, hold_ms,
moderators}` get priority speakers — `priority` token grant, moderator promotion via `SetPriority` /
`POST /v1/moderation/priority`, optionally every moderator — whose speech makes the node attenuate
the other voices for every receiver with that envelope (several priority speakers at once, never the
speaker themselves; local mute/volume/block stay separate), while `DuckingChanged` events and the
Unity `AurixGameAudioDucker` let the game duck its own music/SFX
([docs](docs/src/features/speech.md#voice-effects),
[ducking](docs/src/features/channels.md#priority-speakers-and-ducking)).

**Packet loss: FEC, DRED, neural PLC.** The native core and the server link a **bundled, static
libopus 1.6** (no system `libopus`; CMake at build time) and use its three loss tools: in-band
FEC for the frame just before a packet, **Deep REDundancy** (`dred_duration_ms`, up to 1040 ms of
history a later packet carries — actual coverage is what the bitrate fits) for bursts, and the
neural PLC / OSCE speech enhancer (`DecoderSettings { complexity, osce_bwe }`, `>= 5` deep PLC,
`>= 6` OSCE). Receivers repair gaps FEC → DRED → PLC when the packet that ends them arrives, keep
reordered packets and count `frames_fec_recovered` / `frames_dred_recovered` / `frames_late`; the
server mixers do the same per sender (`media.mixer_decoder_complexity`,
`aurix_mixer_lost_frames_total{method}`) and the per-sender forwarded sequence keeps short uplink
gaps visible so downstream repair can act. The encoder follows the server-measured uplink loss
with a **loss profile** — `Low` / `Moderate` (≥ 3 %: FEC on, expected loss ≥ 10 %) / `High`
(≥ 10 %: expected loss ≥ 20 %, DRED ≥ 400 ms, 28 kbit/s floor), hysteresis and a 6 s dwell,
pinnable per client — in native / C ABI / C++ / Unity / Unreal / Godot; browsers keep FEC and
their own PLC ([docs](docs/src/sdk/native.md#packet-loss-fec-dred-and-the-neural-plc)).

### Content safety

`[safety]` (off by default) turns what a node already sees into moderation data: transcripts of
channels with `"safety_voice": true` (transcribed for the classifier only — never shown to
participants unless `transcription` is also on, never for E2EE frames) and every chat message go
through a **lexicon** with obfuscation-resistant normalization (`sh1t`, `k.y.s`, Cyrillic
look-alikes; `mask` / `block` / `flag` rules in a TOML file, `configs/lexicon.example.toml`) and an
**HTTP classifier you host** (`openai_moderation` response format — OpenAI, llama-guard and
Detoxify/Perspective wrappers — or the `aurix` format with chat context). Scores ≥
`incident_threshold` become moderation events (`safety.voice` / `safety.text`, resolved like any
other) with the flagged text, categories, the preceding chat messages and — for voice, when
`recording.enabled` — an **Ogg/Opus evidence clip** of that speaker (offending segment +
`evidence_pre_segments`), stored as a recording of kind `evidence` with the same encryption,
object storage, download audit and retention. Each user carries a **risk score**, the decayed sum
of their incidents (`risk_half_life_secs`), rebuilt from the database so it is the same on every
node and survives restarts; `auto_mute` / `auto_kick` per source fire on `incident`, `elevated`
or `high` through the normal server-mute / kick path. Chat at `block_threshold` is rejected
(`MESSAGE_BLOCKED`), below that delivered (masked) but recorded; the `chat.filter_webhook` runs
last. Game servers get `safety.incident` / `safety.risk_changed` (webhooks/SSE), query
`GET /v1/safety/incidents`, `GET /v1/safety/users/:id/risk` and export a hand-off bundle with the
decrypted audio inline from `GET /v1/safety/incidents/:id/export`. Players are told a channel is
monitored through `ChannelJoinAck.safety_voice` (Web `isChannelMonitored()`, Unity
`IsChannelMonitored()`). The `mock_speech` example also serves `/v1/moderations` for local runs.
Details: [Content safety](docs/src/features/safety.md).

### Live audio streams

Besides file recordings, a node can hand the audio of a channel to an external service **as it
happens** — your own moderation/toxicity pipeline, a stream overlay, an archival or analytics
sink. It is provider-neutral: the node speaks a small WebSocket protocol and you bridge it to
whatever you run. Off by default; enable with `recording.live.enabled = true` (file recording
`recording.enabled` may stay off).

Two transports, same frames:

* **Pull** — `GET /v1/channels/:id/audio/streams/pull[?format=opus|pcm_s16le&mix=true&users=<id,id>&label=…]`
  with an API key (`audio_streams:write`) upgrades to a WebSocket; frames flow until you close it.
  If you drop off, reconnect with `?resume=<stream id>` within `recording.live.outage_buffer_ms`
  and get what you missed.
* **Push** — `POST /v1/channels/:id/audio/streams {"url":"wss://…","headers":{"Authorization":"…"},
  "format":…,"mix":…,"users":[…],"label":…}` makes the node dial your endpoint (custom headers are sent
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
 "channels":1,"frame_ms":20,"frame_version":1,"users":null,"consent_required":true,
 "mix":false,"node_id":"…","reconnects":0,…}
{"type":"participant","user_id":"…","ssrc":123,"event":"audio_started|consent|left","consent":"accepted"}
{"type":"dropped","frames":12}
{"type":"end","reason":"consumer_disconnected|consumer_timeout|operator|duration_limit|channel_stopped|push_unreachable|shutdown","frames_sent":…,"frames_dropped":…}
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
(`recording.live.allow_pcm = false` disables it). By default streams are **per participant**;
`mix=true` gives **one server-side mix of the channel** instead — one 20 ms frame per tick with an
all-zero `user id` and `ssrc` 0, Opus (`recording.live.mix_bitrate`) or PCM, soft-clipped, with
limited concealment while a talker's packets are late. A node runs at most
`recording.live.max_mix_streams` mixers (one Opus decoder per talker plus one encoder each;
`409` beyond). Transcoding and container formats remain your side's job.

Semantics worth knowing:

* **Consent** follows file recording: with `recording.require_consent` every participant is
  `pending` until they answer the `RecordingNotification` (`live: true`) with
  `RecordingConsentResponse`; only `accepted` participants' frames leave the node, `declined`
  ones never do, and the consumer sees each decision as a `participant` control frame. This
  works across cascaded nodes (the decision is relayed to the node hosting the stream).
* **End-to-end-encrypted frames are never streamed** (nor mixed) — the node cannot read them;
  the same applies to file recordings and transcripts.
* **Streams belong to the fleet.** The node you open a stream on owns it — even when it hosts
  none of the channel's participants: it pins the channel so the cascade relays their audio to
  it. The stream is listed (`node_id`), read, stopped (`202`, forwarded to the owner) and
  resumed from every node, so a load balancer in front is fine. Ownership lives in PostgreSQL;
  rows of a node that vanished are pruned with the node.
* **Outages are bridged, not dropped.** While a pull consumer is away or a push target is being
  re-dialled the owner keeps the last `recording.live.outage_buffer_ms` (default 10 s) of frames
  (`state: reconnecting`) and replays them after the reconnect — a fresh `hello` with a bumped
  `reconnects`, a `dropped` count if the window overflowed, then the backlog in order. Past the
  window the stream ends (`consumer_timeout`; `consumer_disconnected` when the buffer is off).
* **Backpressure never reaches players.** Towards a connected consumer each stream queues
  `recording.live.queue_frames` frames; a slow consumer loses the oldest ones and gets a
  `dropped` count, the media path is not blocked. `max_per_channel` / `max_per_app` bound the
  number of streams per node, `max_duration_secs` (and always
  `recording.max_recording_duration_secs`) their length.
* **Lifecycle** is announced as `audio_stream.started|stopped` (webhooks/SSE, with the end
  reason and frame counters, without URLs or headers), written to the audit log, and shown to
  players like a recording (`RecordingNotification` with `live: true`). Streams end with the
  channel and on node shutdown (`end` frame); erasing a user drops their per-stream state
  (consent, decoder) while the stream itself keeps running.

### Network quality and client statistics

Every SDK exposes one statistics snapshot (native `Client::stats()` / `aurix_client_stats`,
Web `getStats()`, Unity `GetStats()`, Unreal `GetStats()`/`GetNetworkQuality()`) with the same
vocabulary: packets/bytes in both directions, RTT last/min/avg/max, downlink jitter, loss over
the last period (**percent**, `0..=100`), frames lost / late (out of time) / discarded and
jitter-buffer underruns, authentication and replay failures, heartbeat loss, active remote
streams, and the derived rating — a simplified E-model **R-factor** (`0..=100`), the **MOS**
it maps to and **1–5 bars** (`R ≥ 80 → 5`, `≥ 70 → 4`, `≥ 60 → 3`, `≥ 50 → 2`, else `1`). The
same formula (`aurix_common::types::quality`) runs on the server, so a HUD can show either side's
number without recalibrating.

Clients send a `QualityReport {rtt_ms, jitter_ms, packet_loss}` (loss in percent) every
`qualityReportIntervalMs` / `QualityReportInterval` (5 s; `0` disables). The node uses it for the
adaptive downlink bitrate (`BitrateCommand`: > 10 % loss or > 50 ms jitter lowers the target,
> 20 % loss also raises a `quality.alert` with `metric: "packet_loss"`) and merges it every
`media.quality_interval_ms` (2 s) with what the SFU measures on that session's **uplink** —
sequence gaps (loss), RFC 3550 inter-arrival jitter and bitrate. The worse direction decides the
rating, which goes back to the client as `NetworkQuality` whenever the bars change and every fifth
period as a summary:

```json
{"type":"NetworkQuality","data":{"quality":{"bars":4,"r_factor":76.2,"mos":3.9,"rtt_ms":48.0,
 "downlink_jitter_ms":6.5,"downlink_loss_percent":1.2,"uplink_jitter_ms":3.1,
 "uplink_loss_percent":4.0,"uplink_bitrate_kbps":31,"uplink_packets_received":4120,"uplink_packets_lost":170}}}
```

Uplink loss above `quality.loss_alert_percent` (20 %) over a period raises `quality.alert` with
`metric: "uplink_packet_loss"` (webhooks/SSE) even if the client reports nothing. **MOS alerts**
are debounced per session: MOS below `quality.mos_alert_threshold` (3.1) for
`quality.mos_alert_periods` (3) consecutive periods raises one `quality.alert {metric: "mos"}`,
and `quality.recovered` follows only after it has stayed 0.2 above the threshold as long —
no event per bad sample, no flapping at the threshold. Every rated period also feeds a
per-session **summary** (`samples`, `seconds`, `mos_avg/min/last`, `r_factor_avg`, RTT/jitter/loss
avg and max, samples per bar, `poor_seconds`, `mos_alerts`) that is live in
`GET /v1/sessions/:id/stats` (`quality_summary`, `mos_alerting`), checkpointed to
`sessions.quality_stats` every `quality.persist_interval_secs` and on disconnect, continued by
the adopting node on cross-node failover, and ranked worst-first by
`GET /v1/analytics/sessions`; the fleet aggregates (average MOS/RTT/jitter/loss, poor share)
ride the usage buckets of `GET /v1/analytics`. Prometheus gets distributions only
(`aurix_session_mos`, `aurix_uplink_loss_percent`, `aurix_uplink_jitter_milliseconds`,
`aurix_sessions_by_bars{bars}`, `aurix_sessions_mos_degraded`,
`aurix_quality_events_total{metric,event}` — never a session or user label) with alert rules in
`deploy/prometheus-alerts.yml` and the `aurix-quality` Grafana dashboard. Operators read the
live view per session with `GET /v1/sessions/:id/stats` (node-local; the session's node is
listed in `GET /v1/users/:id`). Client counters are cumulative for the current transport; loss,
R-factor, MOS and bars describe the latest period.

### Opus controls and channel audio policy

Every channel carries an **audio policy** derived from its `ChannelConfig` — `bitrate`
(target), `min_bitrate` (floor for the adaptive bitrate), `enable_fec`, `enable_dtx`,
`max_bandwidth` (`narrowband` … `fullband`), an optional `complexity` hint (0–10) and the signal
mode from `audio_profile` (`voice`/`low_bandwidth` → voice, `music` → music, `broadcast` → auto).
Configs are validated against the node (`6000..=media.max_bitrate`, `min_bitrate ≤ bitrate`,
sample rate ∈ {8, 12, 16, 24, 48} kHz). The policy is delivered in `ChannelJoinAck.audio` and,
when a channel is edited over REST, pushed live as `ChannelAudioPolicy` to everyone in it
(`channel.config_updated` on webhooks/SSE). A client in several channels merges them: highest
bitrate and floor, widest bandwidth, FEC if any channel wants it, DTX only if all allow it,
music > voice > auto, highest complexity hint, stereo if any channel allows it.

The native core, Unity and Unreal SDKs expose every libopus encoder control (bitrate 6–300 kbit/s
mono / up to 510 stereo, complexity, max bandwidth, signal, VBR / constrained VBR, FEC, expected
loss, DTX, channels) as a
**baseline**, apply the merged policy on top unless `follow_channel_policy` is off (complexity
can always be pinned locally — the CPU budget is the game's decision), and finally the server's
transient `BitrateCommand {target_bitrate_kbps, reason, expected_loss_percent}`, which the node
clamps to the policy's `min_bitrate..=bitrate` and which also raises the FEC loss tuning.
Browsers own their encoder, so the Web SDK sets only what WebRTC allows: bitrate ceiling
(`RTCRtpSender.setParameters` + `maxaveragebitrate`), `useinbandfec`, `usedtx`,
`maxplaybackrate`, `cbr` and `stereo` on the answer's Opus `fmtp`. Unity gets libopus through
`NativeOpusCodec` (P/Invoke into `libaurix_client`, non-variadic entry points, FEC recovery) as
an alternative to the pure-C# Concentus sample; the C ABI exposes the same encoder/decoder
standalone (`aurix_opus_*`).

**Stereo / music uplinks.** `ChannelConfig.stereo = true` (default `false`; pair it with
`audio_profile: music` and a higher `bitrate`) lets senders encode two-channel Opus — music bots,
DJ decks, broadcast feeds. It is opt-in on both sides (the policy allows it, the client asks for
`channels = 2` / `Stereo` / `opus.stereo`); a voice channel forces stereo-configured clients back
to mono. Nothing is negotiated on the wire: an Opus packet's TOC byte says how many channels it
carries, so the SFU forwards mono and stereo frames alike and every receiver decides per packet —
native and Unity mixers upgrade a stream to a stereo decoder on its first stereo packet, keep the
L/R image for non-positional senders, downmix before panning directional ones and average for
mono outputs; mono-only decoders (recording, transcription, safety, live PCM taps, PCMU edges,
older SDKs) get libopus' downmix. Recordings of a stereo channel carry a 2-channel `OpusHead`.
The capture DSP is a voice chain and is bypassed for stereo frames; browsers are opened with a
2-channel track and voice processing off. PCMU stays mono.

**Server-side noise suppression (opt-in).** Capture DSP runs on the client, but for devices that
cannot (PCMU handsets, embedded boards, bots feeding raw microphone audio) a node with
`media.noise_suppression.enabled = true` denoises a session's uplink itself: the same
RNNoise-class model as the native core (`nnnoiseless`, pure Rust) over the decoded Opus or the
PCMU transcode, re-encoded at `noise_suppression.bitrate` before the frame reaches receivers,
recording, STT, server mixes and the cascade. A client asks with `SetNoiseSuppression`
(`SessionInitAck.noise_suppression` advertises it, `NOISE_SUPPRESSION_UNAVAILABLE` when the node
is off or `max_sessions` are busy; the preference survives resume and failover), or a channel
with `noise_suppression: true` cleans every uplink into it. E2EE frames and frames into stereo
channels are never touched; a frame sent to several channels is cleaned once. `level` = `high` /
`moderate` / `low`; metrics `aurix_noise_suppression_sessions`,
`aurix_noise_suppression_frames_total{path,outcome}`. All SDKs expose
`setServerNoiseSuppression` + a changed event ([channels](docs/src/features/channels.md#server-side-noise-suppression)).

**Per-participant PCM for engine spatialization.** Native, Unity and Unreal clients can pull
each talker's decoded voice separately — unpanned, microphone + TTS, per-participant volume /
server gain / master volume applied — and let the game engine do HRTF, occlusion, reverb and
mixer routing instead of the server's stereo panning: `aurix_client_pull_participant_*` +
`aurix_client_set_participant_claimed` in the C ABI, `AurixParticipantAudioSource` +
`VoicePlaybackMode.PerParticipant` in Unity, `UAurixParticipantSoundWave` /
`SpawnParticipantAudioComponent` in Unreal. Claiming a user takes their streams out of the
aggregate mix, so spatialized avatars and the 2D mix for everyone else coexist without double
playback; claims are by user id and survive rejoins and node failover. Not available for the
server-mixed downlink (one aggregate stream).

**Per-participant WebRTC tracks + HRTF for browsers and Unity WebGL.** A browser's peer connection
always carries the server mix on its first audio m-line; the Web SDK offers extra `recvonly`
m-lines and the node fills up to `media.webrtc_participant_streams` of them (default 16, ≤ 64,
advertised as `SessionInitAck.webrtc_participant_streams`) with **one speaker each, Opus frames
forwarded as-is** (no transcoding, sender cadence kept). The SDK decodes them through a Web Audio
graph — gain (local volume × focus × distance roll-off from `ChannelJoinAck.positional`, `0` for
muted/blocked) → `PannerNode` (`HRTF`, or `equalpower`) placed from the same `updatePosition`
poses — so browsers get binaural per-speaker positioning instead of the server's stereo pan.
Slots are bounded and sticky (1 s hold, released after 30 s of silence), `SetParticipantStreams
{ pinned }` keeps chosen users on a track, `ParticipantStreams { streams: [{ mid, user_id }] }`
pushes the layout on every change; speakers beyond the tracks, ambient channels, and clients or
nodes without the feature stay in the mixed track. Unity WebGL exposes it as
`WebGLClientOptions.ParticipantStreams` / `SpatialAudio`, `SetPinnedParticipantsAsync`,
`OnParticipantStreams`, `IsParticipantSpatialized`.

### PCMU (G.711) fallback for weak devices

Channels are Opus internally, but a native AURX session can ask to run on **G.711 μ-law** —
8 kHz, 64 kbit/s, no Opus CPU on the device (old phones, embedded/handheld hardware, very cheap
SoCs). It is a **per-session** negotiation, never a channel setting: `SetAudioCodec {codec:
"pcmu"}` over the control connection → `AudioCodecChanged {codec}` ack, after which the client
sends 8 kHz μ-law frames (80/160/320/480 bytes = 10/20/40/60 ms) flagged `Pcmu` (`0x2000`) and
receives its downlink as PCMU. The node transcodes at the edge: PCMU uplink is decoded and
encoded to narrowband Opus **before** recording, transcription, safety, live streams, cascade
and fan-out (so every other participant, browsers included, keeps receiving Opus), and internal
Opus is decoded/encoded to μ-law only for PCMU receivers, after mutes, blocks, volume, focus,
positional attenuation and direction have been applied (the gain/direction bytes and the
per-receiver seal are identical to Opus downlinks). `ReceiverPreferences.codec` replays the
session's codec after a resume; a fresh session starts on Opus and the SDKs re-negotiate the
preferred codec. Not available to WebRTC sessions (browsers negotiate Opus in SDP), never for
`E2ee` frames (the server cannot transcode what it cannot read — such frames are dropped), and
switched off node-wide with `media.pcmu_fallback = false`
(`CODEC_NOT_AVAILABLE`). Metrics: `aurix_pcmu_sessions`,
`aurix_pcmu_frames_total{direction="uplink"|"downlink",outcome="ok"|"error"}`. SDKs: Unity `SetAudioCodecAsync` / behaviour
`PreferredCodec`, native `aurix_client_set_audio_codec`, Unreal `SetAudioCodec`.

### When UDP is blocked: AURX over the control WebSocket

Corporate NATs, hotel Wi-Fi and some carriers drop UDP entirely. A native AURX session can then
carry **the same sealed packets** — one per binary WebSocket frame, both directions — over the
control WebSocket it already authenticated with; text frames stay the JSON control plane. The
node advertises it in `SessionInitAck.media_tunnel` (`media.media_tunnel = true`, default) and
reports the live link in `MediaBound.transport` (`quic` / `udp` / `tunnel`) and
`GET /v1/sessions/{id}/stats`. A tunnel belongs to exactly one connection and therefore one
session: the binary `SessionBind` must name that session, the SSRC/HMAC/AEAD/replay checks are
the ones UDP uses, and the packet then walks the same router — mutes, blocks, volume, focus,
positional/ambient, PCMU transcoding, recording, cascade and the WebRTC fan-out do not know which
link it came from. Downlink to a tunnelled receiver is sealed per receiver and queued behind its
socket (`media.tunnel_queue_packets`, 128); a stalled TCP connection drops *its own* packets
(`aurix_tunnel_packets_total{direction="downlink",outcome="dropped"}`), never anyone else's. The
newest signed bind wins, so a client moves between UDP and the tunnel by simply binding again on
the other link, keeping one sequence counter. Where only 443/TCP gets out, the node can also run
a **dedicated TLS tunnel** (`media.tls_tunnel_port`, normally 443 or behind a TLS-passthrough
Caddy/Traefik): TLS 1.3 with ALPN `aurix-tunnel/1`, the QUIC certificate pinned by the hash from
`SessionInitAck.tls_tunnel`, one sealed packet per length-prefixed frame, the same bind/ownership,
queue (`tls_tunnel_queue_packets`), bind-timeout and connection-cap rules, `MediaBound.transport =
"tls"`. SDKs (`Auto` by default): QUIC, then UDP (native; the Unity C# transport starts at UDP),
then the TLS tunnel, then the WebSocket tunnel when no earlier bind gets an answer or
`udp_fallback_lost_heartbeats` heartbeats vanish mid-call; UDP (and QUIC) re-probed every
`udp_reprobe_interval` and taken back as soon as one answers — native `Event::MediaPathChanged` /
`aurix_client_media_path`, Unity `OnMediaPathChanged` / `ActiveMediaPath`, Unreal
`OnMediaPathChanged` / `GetMediaPath`; `QuicOnly`, `UdpOnly`, `TlsOnly` and `TunnelOnly` pin a
link. TCP head-of-line blocking applies: the tunnels keep the player in the call, UDP remains the
path to be on. WebRTC clients are untouched (they have ICE/TURN). Metrics: `aurix_tunnel_sessions`,
`aurix_tunnel_packets_total{direction,outcome}`, `aurix_tls_tunnel_connections`,
`aurix_tls_tunnel_sessions`, `aurix_tls_tunnel_handshakes_total{outcome}`,
`aurix_tls_tunnel_packets_total{direction,outcome}`.

### QUIC for native media: 0-RTT resume and connection migration

Next to raw UDP a native session may send **the same sealed AURX packets as QUIC DATAGRAM
frames** to the same media address: the node speaks QUIC, AURX and WebRTC on one socket
(server-chosen connection ids start with a byte ≥ 0x80, so they never spell the AURX magic),
streams are disabled, so a lost datagram never delays the next one — none of the tunnel's
head-of-line blocking. The node advertises `SessionInitAck.quic { cert_sha256, server_name }`
(`media.quic = true`, default; self-signed certificate generated at start unless
`quic_cert_path`/`quic_key_path` name a PEM pair) and clients **pin that hash** — it arrived over
the authenticated control channel, so no CA and no trust store are involved. TLS is not trusted
for ownership: a connection speaks for a session only after the signed `SessionBind` arrived on
it, it belongs to that one session for its lifetime, and datagrams are attributed to the bound
connection rather than to a source address. That is what makes **connection migration** safe —
when the address changes (Wi-Fi ↔ cellular, NAT rebinding; the game calls
`network_changed()` / Unreal `NetworkChanged()` / Godot `network_changed()`) QUIC path validation
moves the connection and the session keeps id, key, sequence counter, replay and E2EE state
without a re-bind. **0-RTT**: resumption state is kept per node in the client, so a reconnect
sends the `SessionBind` as early data and media is back one round trip later; early data is
replayable, which the AURX layer already tolerates (strictly increasing bind timestamps,
per-session anti-replay windows), and `media.quic_zero_rtt = false` forces 1-RTT. The newest
signed bind still wins across links, superseded connections are closed, stale ones cannot
deliver media or reclaim a session. SDKs (`Auto`): QUIC → UDP → tunnel, a failed QUIC bind is
remembered for `udp_reprobe_interval`, `QuicOnly` / `quic = false` pin the behaviour,
`MediaBound.transport = "quic"` and `SessionInfo.media_quic` report it; old clients and
`media.quic = false` nodes interoperate unchanged. Metrics: `aurix_quic_connections`,
`aurix_quic_sessions`, `aurix_quic_handshakes_total{outcome}`,
`aurix_quic_packets_total{direction,outcome}`, `aurix_quic_migrations_total`.

### WebTransport for browsers: AURX datagrams without WebRTC

Browsers cannot open UDP or QUIC sockets, but they can open a WebTransport session (HTTP/3 over
QUIC, datagrams). A node with `media.webtransport_port` set (default off; meant for **UDP/443**,
separate from `media.port` — HTTP reverse proxies do not forward WebTransport, so the port must
reach the node directly) accepts one session per browser at `https://host:port/aurix` and moves
**one sealed AURX packet per datagram**, both directions: a browser becomes a native session to
the router — per-speaker streams, opaque E2EE frames, `BitrateCommand`, heartbeats — with no SDP,
ICE, TURN or WebRTC. Ownership follows the QUIC rules (signed `SessionBind` first, newest bind
wins, `webtransport_bind_timeout_ms`, bounded `webtransport_queue_packets`,
`webtransport_max_connections`). Certificates: an operator PEM pair (`webtransport_cert_path` /
`webtransport_key_path`, DNS name in `webtransport_advertise`) or, by default and fine for a bare
IP, a node-generated short-lived ECDSA P-256 certificate (`webtransport_cert_days`, 1–14) rotated
at half its validity with both hashes advertised in `SessionInitAck.webtransport { urls,
cert_sha256 }` for the browser's `serverCertificateHashes` — no CA, no ACME. The Web SDK's
`transport: 'auto' | 'webrtc' | 'webtransport'` (default `auto`: WebTransport when the node
advertises it and the browser has WebTransport datagrams + WebCrypto + WebCodecs, WebRTC
otherwise or when every advertised URL fails; `webtransport` is strict, `webrtc` never tries)
runs Opus itself through WebCodecs with the full native parameter set (complexity, signal,
application, expected loss, FEC, DTX, CBR, bitrate — `webTransport.opus` overrides), feeds every
downlink SSRC into the same HRTF renderer as per-participant WebRTC tracks, decrypts E2EE frames
in the page and reports `client.mediaTransport` / the `mediaTransport` event /
`ClientStats.transport`. Metrics: `aurix_webtransport_connections`, `aurix_webtransport_sessions`,
`aurix_webtransport_handshakes_total{outcome}`, `aurix_webtransport_packets_total{direction,outcome}`,
`aurix_webtransport_cert_rotations_total{outcome}`.

### Large channels: listeners, stream caps and the native server mix

A channel with thousands of members costs what its *speakers* cost. `ChannelConfig.audience`
(`hide_listeners`, `mix_for_listeners`, `max_speakers`, `max_streams`) does three things. A
member whose grant says `speak: false` joins as a **listener** (`ChannelJoinAck.role`): the node
drops its audio, it takes no `max_speakers` slot, and with `hide_listeners` it is absent from
rosters and presence events while still counted in `ChannelJoinAck.participant_count`
(`hidden_listeners: true` tells the client the roster is partial). `max_speakers` caps who
speaks at once; `speaker_admission` says what a further speaker gets — `reject` (`CHANNEL_FULL`),
`wait` (joins as an effective listener, `waiting_to_speak`, admitted when a slot frees) or
`demote` (idle plain speakers yield their slot to members trying to speak; priority speakers,
moderators and administrators are never demoted; `RoleChanged` tells everyone). `max_streams` caps how many voices *each receiver* hears:
the ranking is receiver-specific — its mutes, blocks, volumes, focus, positional attenuation and
the sender-reported level — with sticky slots and stale-voice cleanup, so a cap of 4 means "the
4 voices this player should hear", not a channel-wide list; it applies before per-speaker
delivery and before mixing (`aurix_streams_capped_total`). Native sessions can ask for **one
server-mixed stereo stream per channel** (`SetDownlinkMode {mode: "mixed"}` →
`DownlinkModeChanged`; `SessionInitAck.downlink_mix`, `media.downlink_mix = true`): the node
decodes the selected speakers once, applies the receiver's gains and directions, and sends one
Opus stereo stream flagged `Mixed` under a stable synthetic SSRC — receivers with identical
preferences share a mixer, others get a private one (`MAX_MIXERS` 8192, idle mixers torn down
after 10 s). `Mixed | Pcmu` for PCMU sessions; E2EE frames cannot be mixed and keep arriving
as separate streams; recording, live streams, STT, safety and cascade tap the sources, never the
mix. `PATCH /v1/apps/{app_id}` raises `max_participants_per_channel` (up to 100 000). SDKs:
Web `channelInfo` / `canSpeakIn` (browsers already receive a mix), Unity `GetChannelInfo` /
`SetDownlinkModeAsync` / `StereoCodecFactory`, native `channel_info` / `set_downlink_mode`,
Unreal `GetChannelInfo` / `SetDownlinkMode`. Metrics: `aurix_downlink_mixers{kind}`,
`aurix_downlink_mix_frames_total{outcome}`.

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

Either terminate TLS at a reverse proxy (Caddy / Traefik / cloud LB — set `trusted_proxies` so
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
  addresses must be reachable between nodes on `media.port + 1` (the `media.external_ip` you
  register is what peers dial) — UDP normally; a peer that stops answering UDP probes is reached
  over **TCP on the same port** (`media.cascade_tcp_fallback`, same sealed envelopes, bounded
  queues, back to UDP as soon as it answers again).
* **Inter-regional backbone.** `media.cascade_topology = "region_tree"` (default) keeps
  in-region nodes on direct one-hop links but routes cross-region audio through one
  deterministically elected hub per region (origin → hub → hub → node, one copy per WAN link,
  hop-capped at 5 and never forwarded back to its ingress). Hubs are ranked by the **measured
  link table** every node publishes (`media_node_links`: per-peer RTT and UDP/TCP from 1 s
  probes; `GET /v1/nodes/links`, `aurix node links`): reach-everyone first, relay-only next,
  lowest RTT, then a channel hash; hubs that cannot reach each other are joined through a core
  hub and a region whose hosts cannot reach each other becomes a star around its hub.
  `media.cascade_relay_only = true` runs a node as a pure hub (no clients, `503` on `/ws`, never
  selected for failover) next to your backbone; `"mesh"` is the previous full mesh. See
  [Scaling](docs/src/operations/scaling.md#topology-mesh-or-region-tree).
* Put the API/WS behind a load balancer; UDP media must reach the node the session was created on
  (`media_addr` in `SessionInitAck` already points there).
* **Regions.** Label nodes with `server.region` and `server.location = { latitude, longitude }`,
  give each node a public hostname (`server.external_url` / `external_ws_url`, `wss://` in
  production) and clients pick the nearest node: `GET /v1/regions` / `GET /v1/me/regions` order
  regions by requested region → distance → load and hand out the least-loaded node's own
  `ws_url` plus a `probe_url`; the Web, Unity, native and Unreal SDKs probe the RTT and connect
  to the winner (`discoverRegions`, `RegionDiscovery.DiscoverAsync`, `aurix_regions_*` /
  `aurix::Regions`, `DiscoverRegions`). Direct node URLs are deliberate: a resume on the same
  node moves nothing. Nodes without an advertised `wss://` URL keep serving but are not offered.
* **Failover.** With Redis, every node mirrors its sessions (`cluster.session_mirror`, TTL
  `cluster.session_mirror_ttl_secs`) and advertises `cluster.failover_endpoints` peers per
  session. A client whose node died resumes on a peer with the same session id/SSRC (new media
  key/endpoint; the downlink sequence jumps forward so receivers' replay windows and jitter
  buffers carry on), ownership is fenced with an atomic Redis claim, a `SessionMigrated` fleet event updates
  rosters and cascade routes, and a node silent for `cluster.node_lost_after_secs` is reaped by
  the fleet (`node_lost`) while its mirrors stay usable. Redis Sentinel (`redis.sentinels` +
  `redis.sentinel_master`, master changes followed at runtime) and Redis Cluster
  (`redis.cluster` seed list, hash-tagged keys, sharded Pub/Sub with a classic fallback) are
  both supported. Details, PostgreSQL HA, outage behaviour:
  [docs/src/operations/high-availability.md](docs/src/operations/high-availability.md).
* **Kubernetes / cloud.** `deploy/helm/aurix` deploys a regional pool as a host-network
  `StatefulSet` (per-pod public IP for UDP media/TURN, per-pod hostname for discovery and resume,
  external PostgreSQL/Redis, secrets from an existing `Secret`); `deploy/terraform/aws` is a
  documented example of one region on EC2 + EIPs behind Caddy with RDS PostgreSQL, ElastiCache
  Redis, Route 53 and Secrets Manager. Both are linted/validated in CI (not applied).

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
  `aurix_api_requests_total{method,path,status}`, `aurix_turn_allocations`, `aurix_rate_limit_hits_total`, `aurix_rate_limit_scope_hits_total{scope,backend}`,
  `aurix_ws_sessions_detached` / `aurix_ws_sessions_resumed_total` (reconnects), …
* Grafana dashboards: `deploy/grafana/dashboards/aurix-overview.json`, `aurix-quality.json` (MOS
  percentiles/heatmap, bars, alerts, uplink loss/jitter, per-node outliers); Prometheus alert
  rules: `deploy/prometheus-alerts.yml`.
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

System dependencies for building: `pkg-config`, `libssl-dev`, `cmake` (Debian names); libopus 1.6
is compiled from bundled sources and linked statically, no system `libopus` is used.

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
| Unity / .NET (C#) | [`sdk/unity`](sdk/unity) | native AURX v2 over UDP (signed SessionBind, AES-256-CTR + HMAC per packet, replay window), WS control plane; capture DSP via the native core (AEC/NS/AGC) with a managed high-pass + AGC fallback; Unity WebGL players get the same C# API over the Web SDK (browser WebRTC) through `AurixWebGL.jslib` | `dotnet test` (incl. a scripted WebGL bridge) + headless two-client E2E (`Aurix.Demo`, real Opus via Concentus); Unity compile check with `UNITY_WEBGL` |
| Native core (Rust + C ABI) | [`crates/aurix-client`](crates/aurix-client) | same native AURX v2 path in Rust: Opus/VAD/jitter/mixer, capture DSP (high-pass, acoustic echo cancellation, RNNoise-derived noise suppression, AGC) and a voice-effects chain (pitch shift, ring modulator, host callback) on the uplink, reconnect + resume, all control-plane features; `libaurix_client` + `include/aurix_client.h` for Unreal, mobile and custom engines | unit + fake-server tests, C sample compiled/linked/run in CI, live two-client E2E (`cargo test -p aurix-client --test e2e_live`) |
| Unreal Engine 5.3+ (C++/Blueprint) | [`sdk/unreal`](sdk/unreal) | `AurixVoice` plugin over the `aurix-client` C ABI: `UAurixVoiceSubsystem` with typed Blueprint events, `AudioCapture` microphone bridge, procedural playback wave + per-participant sound waves for engine spatialization; `AurixVoiceSamples` module with Blueprint-spawnable lobby / proximity-voice components and a function library; Fab-ready layout (`FilterPlugin.ini`, icon, `Docs/`); static-Opus native library staged by `sdk/unreal/scripts/build_native.*` | native library builds (Linux/Win64/Mac) + ABI-reference test + engine-free descriptor/UHT-convention checks in CI; `RunUAT BuildPlugin` job gated on an Epic-linked GHCR token, so UHT/engine compile **not** run here (see the [README](sdk/unreal/README.md)) |
| Godot 4.3+ (GDExtension) | [`sdk/godot`](sdk/godot) | `aurix_voice` GDExtension over the same C ABI: `AurixVoiceClient` node (signals for every control-plane event, `AudioStreamMicrophone` capture through an `AudioEffectCapture` bus, mixed playback through `AudioStreamGenerator`), `AurixParticipantPlayer` (`AudioStreamPlayer3D` per speaker for engine spatialization), `AurixRegions` for RTT-ranked node discovery; static-Opus native library staged by `sdk/godot/scripts/build_native.sh` (Linux/Windows/macOS; Android/iOS recipes staged); Web export via `AurixWebVoiceClient` (GDScript over `JavaScriptBridge` + Web SDK, same signals, browser-owned audio, `scripts/build_web.sh`) | extension build + headless API smoke tests (native + Web) in CI; live two-client Godot E2E (`sdk/godot/tests/live.sh`) against a running node; Web export booted in headless Chromium against a live node (`tests/web/godot_web_e2e.py`); mobile slices not built in CI |

All SDKs authenticate with the per-user JWT from `POST /v1/tokens`; API keys stay on your backend.

## Game backend: server SDKs, token servers, CLI

| | Path | What |
|---|---|---|
| Server SDKs (Node, Python, Go, C#) | [`sdk/server`](sdk/server) | typed clients generated from `api/openapi.json` (`python3 tools/openapi-sdk/generate.py --check` guards drift) with hand-written transports: retries with `Retry-After`, structured errors, API-key / operator / bootstrap credentials, webhook signature verification, SSE iterator |
| Token servers | [`sdk/server/examples/token-server`](sdk/server/examples/token-server) | the same backend in all four languages: your game session → `POST /voice/token` → `POST /v1/tokens` with the API key → allowlisted `{token, user_id, expires_at, endpoint}` to the client; the key never reaches the client, identity is never taken from the request body |
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

## Limitations

* Native TLS uses rustls with PEM files; ACME/auto-renewal is left to your proxy.
* No SIP/PSTN gateway. Noise suppression runs on the client by default (native core / SDKs);
  the server-side option (`media.noise_suppression`) is mono speech only — E2EE frames and stereo
  channels are never cleaned, a channel that requires it forwards uncleaned frames when the
  node is off or full, and the re-encode is a second lossy Opus pass. No server-side echo
  cancellation or AGC.
* Text chat is deliberately "lite": channel/directed messages and typing, no attachments or
  threads. History, offline delivery of directed messages, read markers, edits / deletions,
  reactions and full-text search exist only with `chat.persist = true`, and offline replay is
  per user (read-marker driven), not an exactly-once per-device queue.
* STT/TTS, live translation and the content-safety classifier talk to HTTP servers you host
  (OpenAI-compatible, LibreTranslate-compatible); no speech, translation or moderation model
  ships with Aurix, transcripts and translations are not stored server-side, and translation is
  caption-first (seconds of provider latency; the spoken translation is a synthesized
  translator voice, not the speaker's).
* A live-stream consumer that stays away longer than `recording.live.outage_buffer_ms` (≤ 5 min)
  loses the stream; the mix of a channel is mono, decodes on the owner node and skips E2EE talkers
  (it never sees them in clear).
* Cascade plans on measured RTT and reachability, not bandwidth or loss: hubs per region
  (plus a core hub / in-region star where a link is blocked, ≤ 5 hops) are elected per channel
  from the link table, hub duty is spread by a channel hash rather than balanced by load, and
  the inter-node TCP fallback is plain TCP carrying the same `cascade_secret`-sealed envelopes
  (no TLS layer, TCP head-of-line blocking under loss).
* Cross-node failover needs Redis (session mirrors); Sentinel and Cluster are both supported,
  but sharded Pub/Sub needs Redis 7 (`redis.sharded_pubsub = false` on older clusters), and the
  chaos suite exercises the cluster on one host only.
* On WebRTC, browsers cannot set Opus complexity, signal mode, VBR mode or expected loss — only
  the bitrate ceiling, FEC, DTX, max bandwidth and CBR that WebRTC exposes; the native, Unity and
  Unreal SDKs have the full set, and browsers get it only on the WebTransport path (WebCodecs
  Opus), which needs a Chromium-based browser and the node's `webtransport_port` reachable over
  UDP directly (no reverse proxy) — everything else stays on WebRTC.
* PCMU is a per-session fallback for native AURX clients only (no PCMA, no WebRTC PCMU, no
  PCMU for `E2ee` frames); each PCMU session costs the node one Opus encoder plus one Opus
  decoder per speaker it hears.
* The blocked-UDP fallbacks for native clients are TCP — the dedicated TLS tunnel
  (`media.tls_tunnel_port`, pinned QUIC certificate, TLS-passthrough proxies only, no ACME) or
  the control WebSocket: head-of-line blocking under loss and a bounded per-connection downlink
  queue. QUIC shares the media UDP port and is blocked by the same firewalls; no TURN for native
  media, no QUIC for the Unity C# transport. Browsers have no TCP fallback for AURX — the
  WebTransport endpoint is HTTP/3 only, so a UDP-blocked browser lands on WebRTC over TURN;
  WebTransport sessions have no 0-RTT or migration.
* Server-side mixing for native clients bypasses E2EE frames (they stay per-speaker), costs the
  node one Opus decode per selected speaker plus one stereo encode per mixer, and is capped at
  `MAX_MIXERS` (8192) per node; speaker demotion (`speaker_admission = "demote"`) goes by
  silence and sender-reported level, not by server-side voice analysis, and the speaker count is
  per node plus what the cascade has propagated.
* The Unreal plugin has not been compiled against a real engine install yet (none is available
  in the development environment); the first build in your project is the verification step.
  The protocol is documented in `crates/aurix-common/src/protocol.rs`.
* No console SDKs (PlayStation/Xbox/Switch SDKs are under NDA) and no first-party Flutter /
  React Native packages: both are integration work on top of the native core's C ABI —
  [porting guide](docs/src/sdk/consoles.md), [mobile frameworks](docs/src/sdk/mobile-frameworks.md).
  The Godot Web export uses `AurixWebVoiceClient` (GDScript over the Web SDK, browser-owned
  audio) instead of the native extension; Godot Android/iOS slices are staged but not built in CI.
* Server SDKs (Node/Python/Go/C#) are generated from the OpenAPI contract but not published to
  registries; the token servers are examples of the credential boundary (their `/dev/login` is a
  development stand-in for your game's login), and the `aurix` CLI is a REST client — it never
  joins a channel or sends audio.

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
