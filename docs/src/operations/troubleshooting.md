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
| `recovered {resumed: false}` every time | the grace window is too short (`server.session_resume_grace_secs`, default 30), or the resume lands on another node while session mirrors are off (`cluster.session_mirror`, needs Redis) — enable them or add client-IP affinity for `/ws` on the balancer |
| `recovered {resumed: true, migrated: true}` on every reconnect | the balancer spreads `/ws` across nodes: takeovers work but rebind media each time; hand out per-node URLs (region discovery) or add client-IP affinity |
| takeover refused after a node crash (`aurix_ws_takeovers_refused_total{reason}`) | `not_mirrored`: the mirror expired (`cluster.session_mirror_ttl_secs`) or Redis lost it; `denied`: token/tenant mismatch (client presented an old resume token); `raced`: two reconnects at once — the loser gets a fresh session; `redis`: Redis unreachable ([High availability](high-availability.md)) |
| `SessionInitAck.failover` is empty | other nodes have no `server.external_ws_url` (`wss://` in production) or `cluster.failover_endpoints = 0` |
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
| one participant sounds narrowband / telephone-like to everyone | that participant negotiated the G.711 fallback (`aurix_g711_sessions` > 0); expected — the node upsamples 8 kHz μ-law into the Opus stream ([codecs](../features/channels.md#codecs-opus-and-the-pcmu-fallback)) |
| PCMU/PCMA client hears nothing while Opus clients do | frames sent before `AudioCodecChanged` arrived, frames flagged with the other law, or `Pcmu`/`Pcma` frames with a length other than 80/160/320/480 bytes (`aurix_g711_frames_total{outcome="error"}`); in an E2EE channel the peers' SDKs must be able to decode G.711 (all current ones do) |
| native/Unity/Unreal player connects but never gets `MediaBound`; browsers work | UDP is blocked on their network. With `media.media_tunnel = true` (default) an `Auto` client falls back to the WebSocket tunnel by itself (`MediaBound.transport = "tunnel"`, `OnMediaPathChanged`); if the SDK is pinned to `UdpOnly` or the node runs `media_tunnel = false`, open `media.port`/UDP or enable the tunnel ([tunnel](../api/aurx.md#tunnel-aurx-over-the-control-websocket)) |
| a native player on `MediaBound.transport = "udp"` although the node runs `media.quic = true` | the client is pinned (`UdpOnly`, `quic = false`) or its QUIC bind got no answer — a middlebox that passes plain UDP but drops QUIC (`aurix_quic_handshakes_total{outcome="failed"}` stays flat while the client retries). Nothing breaks: `Auto` remembers the failed bind for `udp_reprobe_interval` and stays on UDP ([QUIC](../api/aurx.md#quic-aurx-datagrams-with-0-rtt-resume-and-connection-migration)) |
| a QUIC player drops to the tunnel after every network change | the game does not call `network_changed()` (native / Unreal `NetworkChanged()` / Godot), so the connection times out and heartbeats fall back; or `media.quic_migration = false`, which makes the node drop migrating connections on purpose |
| tunnelled player hears bursts / stutter, `aurix_tunnel_packets_total{direction="downlink",outcome="dropped"}` grows | TCP head-of-line blocking on that player's connection; the node drops only that receiver's queue (`media.tunnel_queue_packets`). Nothing to fix server-side — check why UDP is blocked for them, the SDK re-probes it every `udp_reprobe_interval` |

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
| `NOISE_SUPPRESSION_UNAVAILABLE` | `SetNoiseSuppression { enabled: true }` on a node with `media.noise_suppression.enabled = false`, or with all `max_sessions` slots busy (`aurix_noise_suppression_sessions`); the SDKs keep the preference and retry it after a reconnect ([server-side noise suppression](../features/channels.md#server-side-noise-suppression)) |
| `CODEC_NOT_AVAILABLE` | `SetAudioCodec` for a codec the node does not allow: `media.pcmu_fallback = false`, or the session is WebRTC (browsers negotiate Opus in SDP) |
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
