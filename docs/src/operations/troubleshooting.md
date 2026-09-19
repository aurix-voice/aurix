# Troubleshooting

Start with `GET /ready`, the node's logs (`tracing.log_level = debug` or
`RUST_LOG=aurix_ws=debug,aurix_media=debug,info`) and `/metrics`. Every REST/WebSocket error
carries a stable `code` — the table at the end maps codes to causes.

## The server does not start

| symptom | cause / fix |
|---|---|
| `auth.jwt_secret must be at least 32 bytes in production`, `… is a placeholder value` | generate a real secret (`openssl rand -base64 48`) or configure `auth.jwt_public_key_path` |
| `server.cors_origins must not contain '*' in production` | list the exact origins of your web builds |
| `media.external_ip must be set in production` | the public IP clients send UDP to (`AURIX_PUBLIC_IP` in Compose) |
| `database.url uses the development default credentials` | set real PostgreSQL credentials |
| `Database schema is behind the compiled migrations` | `database.run_migrations = true` or run `aurix-server --migrate-only` once |
| `retention.tombstones_days` rejected | it must cover the longest token lifetime (`auth.token_ttl_secs`, action TTLs) |
| `stt.enabled requires stt.endpoint` / `tts.default_voice must be one of tts.voices` | complete the `[stt]` / `[tts]` blocks or disable them |

## Clients cannot connect

| symptom | cause / fix |
|---|---|
| WebSocket upgrade → `401 TOKEN_INVALID` / `TOKEN_EXPIRED` | token minted with a different `jwt_secret`, wrong algorithm, or expired (`token_ttl_secs`, default 1 h). Check the node time. |
| `401 ACTION_TOKEN_REQUIRED` | `auth.require_action_tokens = true`: WebSocket login and `ChannelJoin` need one-time `login` / `join` tokens (`POST /v1/tokens/action`) |
| `401 TOKEN_REUSED` | an action token was presented twice (replay, or two clients sharing one token). Mint one per use. |
| `403 USER_BANNED` | active ban (account / device / IP) — `GET /v1/moderation/bans` |
| `503 MEDIA_NODE_UNAVAILABLE` | no healthy node in the registry or the node is above 90 % capacity — check `GET /v1/nodes` and heartbeats |
| browser: CORS error | origin missing in `server.cors_origins`; secure context (`https://` or `localhost`) required for microphone and WebRTC |
| roster arrives but **no audio** (native) | UDP `media.port` not reachable at `media.external_ip`; firewall/NAT; `SessionBind` never acknowledged → `heartbeats_lost` grows, `aurix_packets_received_total` flat |
| native: `bad_auth` counter grows | wrong media key (client bound with a stale session), clock skew beyond the replay window, or a middlebox rewriting packets; the server counts them in `aurix_packets_dropped_total` |
| browser: ICE fails / `connecting` forever | UDP blocked → enable TURN (`turn.enabled`, relay range open) and issue `GET /v1/me/turn-credentials`; behind a proxy set `turn.external_ip` |
| WebRTC connects, no sound | autoplay policy — attach the remote stream after a user gesture (`attachAudioOutput`), check `setOutputMuted` / device selection |

## Reconnects and resume

| symptom | cause / fix |
|---|---|
| `recovered {resumed: false}` every time | resume landed on another node (enable client-IP affinity for `/ws` on the balancer) or the grace window is too short (`server.session_resume_grace_secs`, default 30) |
| `failedToRecover` | token expired during the outage (wire `refreshToken` / `TokenRefresher`), user banned/erased, or the node was shut down (`SessionClose {reason: "server_shutdown"}` is final — open a fresh session) |
| channels missing after a fresh session (native `REJOIN_FAILED`, Unity `OnServerError`, Web `error`) | join tokens are one-time; with `require_action_tokens` supply a `joinToken` / `JoinTokenProvider` callback so the SDK can mint a new one for the automatic re-join |
| `aurix_ws_sessions_detached` stays high | clients drop without resuming — mobile background, aggressive NAT timeouts; see [Unity mobile](../sdk/unity.md#ios--android-notes) |

## Audio quality

| symptom | cause / fix |
|---|---|
| bars drop, `BitrateCommand` to 32 / 16 kbit/s | client-reported loss > 10 % / 20 % or jitter > 50 ms; look at the client's `getStats()` vs. `GET /v1/sessions/{id}/stats` to see which direction is bad ([Network quality](../features/quality.md)) |
| `quality.alert {metric: "uplink_packet_loss"}` | packets from the client lost before the node — client uplink or the host's UDP receive buffer (`Udp: RcvbufErrors` in `/proc/net/snmp`; raise `net.core.rmem_max`, `media.rx_workers`) |
| robotic / choppy audio for one peer | that peer's uplink; `frames_lost` / `underruns` in the receiver's stats per stream |
| positional channel silent | both listener and speaker must have sent a `PositionUpdate`; check `positional_config` (`near_distance`, `far_distance`, `max_radius`) and that the channel is `positional` |
| a participant is silent for one player only | receiver-local mute, volume 0, block, `TransmissionMode` / focus — `ReceiverPreferences` events show the state |
| speaking indicator never lights | frames below `media.speaking_energy_threshold` (0.01); VAD gate on the client too strict |

## Features returning errors

| code | meaning |
|---|---|
| `AUTH_FAILED` / `AUTH_DENIED` | missing/invalid credential / credential lacks the permission or targets another tenant |
| `TOKEN_INVALID`, `TOKEN_EXPIRED`, `TOKEN_REUSED`, `ACTION_TOKEN_REQUIRED` | JWT / action-token problems (above) |
| `CHANNEL_NOT_FOUND`, `USER_NOT_FOUND`, `SESSION_NOT_FOUND`, `NOT_FOUND` | unknown id, other tenant, or (sessions/streams) hosted on another node |
| `CHANNEL_FULL`, `CHANNEL_LIMIT_EXCEEDED` | `max_participants` reached / `media.max_channels_per_session` or positional-channel limit |
| `USER_BANNED`, `USER_MUTED`, `USER_OFFLINE` | moderation state; direct chat to a user without a live session |
| `CHAT_DISABLED`, `MESSAGE_BLOCKED`, `VALIDATION_ERROR` | chat off, filter webhook blocked the text (fail-closed by default), size/field validation |
| `RATE_LIMIT_EXCEEDED` | per-IP / per-key / per-session limits (`[rate_limiting]`, `[chat]`, `[tts]`) |
| `INVALID_CONFIG` | feature disabled on the node (recording, live streams, STT/TTS) or misconfigured request against it |
| `CONFLICT` | retention sweep already running, live stream on another node, duplicate resource |
| `MEDIA_NODE_UNAVAILABLE`, `TIMEOUT`, `STT_ERROR`, `TTS_ERROR`, `TTS_DISABLED` | node capacity, provider timeouts or failures (details are sanitised; see the node log) |
| `RECORDING_ERROR`, `STUN_TURN_ERROR`, `ENCRYPTION_ERROR`, `CODEC_ERROR`, `TRANSPORT_ERROR` | subsystem failures — the log line has the cause |
| `DB_ERROR`, `REDIS_ERROR`, `INTERNAL_ERROR` | infrastructure; `GET /ready` will usually be failing too |
| `NOT_IMPLEMENTED` | feature reserved in the API but not available in this build |

## Webhooks and SSE

* Deliveries stuck in `pending` / `failed`: `GET /v1/webhooks/{id}/deliveries` shows the last
  status and response; `POST /v1/webhooks/{id}/test` sends a synthetic event. In production the
  endpoint must be `https://` and public (`webhooks.require_https`, `allow_private_urls`).
* Signature mismatch: verify over `"<t>.<raw body>"` with the secret shown once at creation
  (`rotate-secret` if lost); allow a few minutes of clock skew on `t`.
* SSE clients receive `lagged`: the consumer was too slow; call `GET /v1/events/snapshot` and
  resubscribe ([Webhooks and SSE](../api/webhooks-sse.md)).
