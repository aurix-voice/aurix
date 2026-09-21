# Webhooks and the event stream

Everything that happens in a tenant is published as a `ServerEvent`. Your game server can consume
those events two ways — **signed webhooks** (durable, at-least-once, retried) and a
**server-sent-event stream** (live, best-effort). Both are scoped to the application of the API
key; other tenants' events are never visible.

`GET /v1/webhooks/events` returns the catalogue of public event types the running server can
emit:

| Family | Events |
| --- | --- |
| channels | `channel.created`, `channel.destroyed`, `channel.config_updated`, `channel.activated`, `channel.deactivated`, `channel.energy`, `channel.transcript` |
| participants | `participant.joined`, `participant.left`, `participant.muted`, `participant.unmuted`, `participant.priority_changed`, `participant.kicked`, `participant.speaking`, `participant.typing` |
| users / moderation | `user.banned`, `user.deleted`, `user.block_changed`, `moderation.event`, `safety.incident`, `safety.risk_changed` |
| recordings / streams | `recording.started`, `recording.stopped`, `recording.consent_required`, `recording.processed`, `audio_stream.started`, `audio_stream.stopped` |
| chat / speech | `chat.message`, `chat.message_updated` (stored chat: `{message}` after an edit or a deletion — a tombstone has `deleted_at`), `chat.reaction` (`{message_id, user_id, reaction, added, count}`), `chat.read_marker` (stored chat: `{marker}` whenever a user's position moves), `tts.status` |
| quality | `quality.alert` (`metric` `packet_loss` / `uplink_packet_loss` per period, `mos` once per debounced episode), `quality.recovered` (`mos` only, after the hysteresis — [Network quality](../features/quality.md)) |
| synthetic | `webhook.test`, `webhook.resync` (produced by the webhook service itself) |

`channel.activated` / `channel.deactivated` fire when a channel gets its first participant /
loses its last one — including channels left behind by a crashed node once its sessions
expire. `channel.config_updated` carries the full new `config` after `PUT /v1/channels/{id}/config`
(it is also how the other nodes learn about the edit). The high-frequency types `participant.typing`, `participant.speaking` and
`channel.energy` are delivered only when a subscription or SSE filter names them explicitly;
`"*"` does not include them.

## Event envelope

Every webhook body and every SSE `data:` line is the same JSON object:

```json
{
  "id": "0192a4b6-1c2e-8f3a-9d4e-5f6a7b8c9d0e",
  "type": "participant.joined",
  "app_id": "6e1d…",
  "created_at": "2025-01-27T18:26:40.123Z",
  "data": { "channel_id": "…", "user_id": "…", "session_id": "…", "display_name": "Alice", "ssrc": 1839201, "timestamp": "2025-01-27T18:26:40.123Z" }
}
```

`id` is derived from the event content and stays the same across retries and across nodes, so
deduplicate on it. `data` is the event-specific payload; the exact fields per type are in the
OpenAPI description of `EventEnvelope` and in the event definitions
(`crates/aurix-control/src/event_bus.rs`).

## Webhooks

```bash
curl -X POST localhost:8080/v1/webhooks -H "x-api-key: $KEY" -H 'content-type: application/json' \
  -d '{"url":"https://game.example.com/aurix","events":["participant.joined","participant.left","user.banned"]}'
# -> {"id":"…","url":"…","events":[…],"enabled":true,"secret":"…", …}
```

* The `secret` is returned only by the create and `POST /v1/webhooks/{id}/rotate-secret`
  responses; rotation is immediate (deliveries already in flight were signed with the old
  secret, so switch your verifier before rotating and accept both for a moment).
* `events` is up to 64 types or `["*"]`; `enabled: false` pauses the subscription (queued
  deliveries fail with `subscription disabled`, new events are not queued). Up to
  `webhooks.max_subscriptions_per_app` (20) subscriptions per application.
* URLs must be `https://` and must not resolve to loopback, private or link-local addresses in
  production (`webhooks.require_https` / `webhooks.allow_private_urls` default to the production
  flag). The host is resolved and pinned per attempt and redirects are not followed.
* Deliveries live in PostgreSQL (`webhook_deliveries`) and are leased by every node with
  `FOR UPDATE SKIP LOCKED`, so a node restart loses nothing. A `2xx` marks the row delivered;
  any other status, a timeout (`webhooks.timeout_ms`, 5 s) or a connection error schedules the
  next attempt after `webhooks.retry_delays_secs` — by default 5 s, 30 s, 2 min, 10 min,
  30 min, 1 h, 2 h, then the delivery is `failed`. Attempts reuse the same delivery id and
  body.
* If more than `webhooks.max_pending_per_subscription` (10 000) deliveries are waiting for one
  endpoint, new events for it are dropped — the endpoint is considered dead; use `resync`
  once it is back. Delivered/failed rows are kept `webhooks.retention_hours` (72) for
  `GET /v1/webhooks/{id}/deliveries` and `POST /v1/webhooks/{id}/deliveries/{delivery_id}/retry`.
* `POST /v1/webhooks/{id}/test` queues a `webhook.test` event (even for a disabled
  subscription); `POST /v1/webhooks/{id}/resync` queues a `webhook.resync` event whose `data` is
  the current state of the application — every active channel with its participants
  (`user_id`, `display_name`, `session_id`, `ssrc`, `role`, `is_muted`, `is_server_muted`,
  `joined_at`) and `snapshot_at` — so a freshly (re)subscribed game server can rebuild its view.
* Subscriptions are cached per node for 10 s; cache invalidation is broadcast over Redis when a
  subscription changes, so events from players on other nodes reach a new subscription within
  that window.

### Request format

```http
POST /aurix HTTP/1.1
Content-Type: application/json
User-Agent: aurix-webhooks/1.0.0
X-Aurix-Event: participant.joined
X-Aurix-Webhook-Id: 3b9f…
X-Aurix-Delivery-Id: 2f7c…           # stable across retries
X-Aurix-Attempt: 1
X-Aurix-Signature: t=1738000000,v1=9d0e…

{"id":"…","type":"participant.joined","app_id":"…","created_at":"…","data":{…}}
```

### Verifying the signature

```
v1 = hex( HMAC-SHA256( secret, "<t>" + "." + <raw request body> ) )
```

Compute the MAC over the **raw** body bytes (not a re-serialised JSON object), compare with
`v1` in constant time and reject timestamps `t` older than a few minutes (5 min is a good
tolerance) to stop replays.

```python
import hmac, hashlib, time

def verify(secret: str, header: str, body: bytes, tolerance=300) -> bool:
    parts = dict(kv.split("=", 1) for kv in header.split(","))
    if abs(time.time() - int(parts["t"])) > tolerance:
        return False
    mac = hmac.new(secret.encode(), parts["t"].encode() + b"." + body, hashlib.sha256).hexdigest()
    return hmac.compare_digest(mac, parts["v1"])
```

The Rust reference is `aurix_control::webhooks::verify_signature`; the live E2E test
(`crates/aurix-server/tests/e2e_live.rs`) runs a receiver that verifies every delivery with it.

## Server-sent events

```bash
curl -N "localhost:8080/v1/events?types=participant.joined,participant.left" -H "x-api-key: $KEY"
```

`GET /v1/events` (`events:read`) is a `text/event-stream` of the application's events as they
happen on any node (events replicate over the Redis bus, so it does not matter which node the
request lands on):

* the first frame is `event: stream.open` with `{"app_id": …, "filter": [...]|null}`;
* every event is `id: <event id>`, `event: <type>`, `data: <envelope JSON>`;
* `types=` filters by exact type (comma-separated); typing / speaking / energy events are sent
  only when listed;
* a `: keepalive` comment is written every `webhooks.sse_keepalive_secs` (15 s) so proxies and
  load balancers keep the connection open — configure their idle timeouts above that;
* if the consumer falls behind the per-connection buffer, the server drops the backlog and
  sends `event: lagged` with `{"dropped": n}` — fetch `GET /v1/events/snapshot` (the same
  channels/participants document as a webhook resync) and continue reading the live stream;
* the stream ends when the connection drops or the node drains for a restart; reconnect with
  backoff and fetch the snapshot again. There is no replay of missed events: Aurix keeps no
  per-tenant event log, so after any gap the snapshot is the source of truth.

Open streams are counted in `aurix_event_stream_clients`; webhook outcomes in
`aurix_webhook_deliveries_total{result}` and `aurix_webhook_deliveries_leased`.

## Choosing

| | Webhooks | SSE |
| --- | --- | --- |
| delivery | at-least-once, durable, retried | best-effort, live |
| latency | seconds (poll interval + HTTP) | milliseconds |
| needs | public HTTPS endpoint | outbound HTTP from your server |
| best for | bans, reports, recordings, billing, audit mirrors | match servers reacting to joins/leaves, live operator views |

Most integrations use both: SSE for reactive game logic, webhooks for anything that must not be
lost. Neither replaces the control WebSocket for the players themselves.
