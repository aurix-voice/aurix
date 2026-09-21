# Security model

Mechanisms, by layer. Who they defend against — and who they do not — is in the
[threat model](threat-model.md); how to report a hole is in `SECURITY.md`.

## Media

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
* SFU-to-SFU cascade wraps packets in a `Relay` envelope encrypted and authenticated with keys
  derived from `media.cascade_secret`, with a per-peer anti-replay window; the client's plaintext
  is never on the wire between nodes and unknown source addresses are dropped.
* Channels with `e2ee: true` are **end-to-end encrypted**: every member seals its Opus frames
  with a per-sender group key (AES-256-CTR + HMAC-SHA256, keys wrapped per peer with X25519 →
  HKDF-SHA256 and exchanged over the control plane, rotated on every join/leave) that the node
  never holds. The node authenticates the participants, relays the wrapped keys only between
  members of the same channel and forwards ciphertext; it cannot mix, record, transcribe,
  translate, classify, live-stream or add directional metadata to those frames, and sessions
  that cannot encrypt are refused (`E2EE_REQUIRED`) rather than served plaintext. Identity keys
  are vouched for by the node — against a malicious operator, applications compare key
  fingerprints out of band. See [End-to-end encryption](../features/e2ee.md).

## Credentials

* Tenant identity always comes from the validated API key / JWT — never from request bodies.
  Cross-tenant resources answer `404` as if they did not exist.
* API keys are compared in constant time; only the hash is stored; the plaintext is shown once.
  A key can only mint keys with a subset of its own permissions.
* Player JWTs are short-lived and scoped to channel grants (`speak`/`receive`, ad-hoc grants).
* **One-time action tokens** (`POST /v1/tokens/action`): short-lived (90 s by default) JWTs with a
  unique `jti` that authorise exactly one `login`, `join`, `kick`, `mute` or `unmute`. The first
  presentation claims the `jti` atomically in Redis (`SET NX EX`, key scoped to the tenant); a
  replay — on any node — is refused with `TOKEN_REUSED`. Without Redis the claim registry is
  node-local. `AURIX__AUTH__REQUIRE_ACTION_TOKENS=true` makes them mandatory for opening a
  session and joining a channel, so a leaked player JWT can no longer be used to log in or join.
* Resume tokens are one-time, rotated on every ack, bound to the user/app and revoked when the
  server closes the session.
* Any token minted before a user's erasure tombstone is refused on WebSocket and REST alike.
* Admin passwords are Argon2-hashed; `/admin/setup` locks itself after the first admin unless a
  bootstrap token is configured.

## Network services

* TURN requires MESSAGE-INTEGRITY with long-term credentials derived from the API-issued
  time-limited username/password (HMAC-SHA1 shared secret, RFC 5389 §15.4); nonces, allocation
  ownership, permissions and channel bindings are enforced. No open relay.
* Webhook, chat-filter and live-stream push URLs are validated against SSRF: `https://`/`wss://`
  required in production, private/loopback/link-local addresses refused, DNS resolved and pinned
  per delivery, redirects not followed, credentials in URLs rejected. Secrets and custom headers
  are never echoed back by the API or events.
* Rate limits are per IP and per key (Redis-backed when available); `X-Forwarded-For` is only
  honoured from `server.trusted_proxies`.

## Operational hygiene

* Public API errors never leak internal details; details are logged at `debug`.
* Production mode (`AURIX__SERVER__ENVIRONMENT=production`) refuses to start with wildcard CORS,
  placeholder/short secrets, dev DB credentials or a missing public media IP.
* Every privileged action (admin login, app/key changes, bans, kicks, recording access, erasure,
  exports, retention sweeps) lands in the audit log.
* The container runs as uid 10001 with a read-only root filesystem and no capabilities; secrets
  come from the environment only.
