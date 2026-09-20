# Scaling out

Aurix scales horizontally: run N identical `aurix-server` nodes against one PostgreSQL and one
Redis. There is no coordinator process — every node is API + control plane + SFU, and the
database is the registry.

## Fleet registry

Each node registers itself in `media_nodes` (`server.node_id` or a generated UUID, `region`,
`media.external_ip`, media/API/cascade ports, capacity, load) and heartbeats. Nodes silent for
**30 s** are marked unhealthy; rows are forgotten after **24 h**. `GET /v1/nodes` (admin) lists
the fleet with load, CPU, memory and bandwidth. When a node goes away its sessions are cleaned
up (`recover_node_state`) and channels that only lived there emit `channel.deactivated`.

## Where a session lives

A session is anchored to the node that accepted its **WebSocket**: `SessionInitAck.media_addr`
points to that node's `media.external_ip:media.port`, its media key lives in that node's SFU, and
node-local resources (WebRTC peer connection, recordings, live audio streams, per-session REST
stats) are served there. Consequences:

* Put the REST API and the WebSocket behind an ordinary L7 load balancer — no sticky sessions
  are required for the API. A WebSocket stays on the node it landed on; a **resume** is
  cheapest on the same node (nothing moves), so keep client-IP affinity on the balancer for
  `/ws` or hand out per-node URLs. A resume that lands on another node is a **takeover**: with
  Redis session mirrors (default) the session moves there with the same id and SSRC and a new
  media key/endpoint (`recovered {resumed: true, migrated: true}`); without mirrors it becomes
  a fresh session (`recovered {resumed: false}`) — see
  [High availability](high-availability.md#cross-node-session-failover). The SDKs handle all
  three.
* UDP media must reach the node directly (`media.external_ip`), not through the balancer.
* REST calls that touch live media are node-local: `GET /v1/sessions/{id}/stats` answers `404`
  for a session hosted elsewhere, live-stream routes answer `409` when the channel's media is
  on another node, and a recording follows the participant's node. `GET
  /v1/channels/{id}/participants` reports `live_on_this_node`, `GET /v1/nodes` gives each node's
  address — an operator tool retries against the right node or relies on webhooks/SSE, which
  are fleet-wide.

## Cross-node events

Channel/participant events (join, leave, mute, ban, kick, block, chat, transcripts, energy,
webhook-subscription invalidation, `user.deleted`, `SessionMigrated`) are published on Redis pub/sub with the
origin node id; a node never re-applies its own events. Redis also holds one-time token claims
(`jti`), session→node mapping and session mirrors for failover, node liveness beacons, global
mutes and distributed rate limits. Without Redis a single node works; a fleet does not. Redis
Sentinel is supported, Redis Cluster is not — see [High availability](high-availability.md#redis).

## Fleet-wide rate limits

`[rate_limiting]` protects every abusable entry point with a token bucket keyed by the caller,
and with Redis (`fleet = true`, the default) **the bucket is one per fleet**, not one per node: a
client that spreads its requests over ten nodes gets the same budget as one that talks to a
single node. The bucket is advanced atomically in a Lua script with the Redis clock, so every
node takes the same decision and the limit does not depend on node clocks.

| scope | subject | limit |
|---|---|---|
| `api_ip` | client IP (after `server.trusted_proxies`) | `requests_per_second` sustained, `burst_size` burst |
| `api_key` | API key | the key's own `rate_limit` (requests/minute; default 6000 at creation, `0` = unlimited; `PATCH /v1/api-keys/{key_id}` changes it live) — off with `per_key = false` |
| `connect` | user | `connects_per_minute` control-WebSocket connections (60) |
| `join` | user | `channel_joins_per_minute` (30) |
| `block` | user | `block_changes_per_minute` (60) |
| `report` | user | `reports_per_minute` (10) |
| `admin_login` | client IP | `admin_login_per_minute` (10; a setup attempt costs two) |

Per-minute limits allow the whole minute as a burst and refill continuously. REST callers get
`429 RATE_LIMIT_EXCEEDED` with `Retry-After`; WebSocket clients get an `Error` with the same
code (or `429` on the upgrade for `connect`). Chat flood control (`[chat]`) and TTS queue
limits stay per session — sessions never span nodes.

When Redis is unreachable the node falls back to its own buckets (same limits, per node) and
counts `aurix_rate_limit_backend_errors_total`; set `fail_closed = true` to refuse instead.
`aurix_rate_limit_scope_hits_total{scope,backend}` shows which scopes are throttling and whether
the decision was `fleet` or `local`.

## Cascade (SFU-to-SFU relay)

Channels whose members sit on different nodes are relayed **automatically**:

1. Set the same `media.cascade_secret` (≥ 16 chars) on every node. Nothing else is required.
2. Every node advertises its cascade UDP port (`media.port + 1`) in `media_nodes`.
3. Every `media.cascade_discovery_interval_ms` (3 s) and immediately after a remote join/leave a
   node reconciles the topology from the database: accepted peers are the healthy nodes, and
   each channel is forwarded only to nodes that actually hold live memberships for it.
4. Packets travel inside a `Relay` envelope encrypted and authenticated with keys derived from
   `cascade_secret` (the client's plaintext is never on the wire between nodes) and pass a
   per-peer anti-replay window; unknown source addresses are dropped. Receiver preferences
   (mute / volume / block / focus) are applied on the receiving node, so they work across
   nodes; echo channels are never relayed.

`media.cascade_peers` is an optional static allow-list (nodes outside the registry), and
`media.cascade_discovery = false` switches to a fully static full mesh. Cascade is a one-hop
mesh between the nodes hosting a channel — there are no relay trees — and it needs node-to-node
reachability on `media.port + 1`/UDP.

## Regions

`server.region` (`us_east`, `us_west`, `eu_west`, `eu_central`, `asia_pacific`, `south_america`,
`australia`, `middle_east`, `africa`) is stored per node and reported in `GET /v1/nodes`. Nodes
also advertise their public endpoints (`server.external_ws_url`, or `wss://…/ws` derived from
`server.external_url`; the REST base URL; optional `server.location` coordinates), and the
registry turns that into **region discovery**:

* `GET /v1/regions` (API key) and `GET /v1/me/regions` (player JWT) return one entry per region
  that has a healthy, non-saturated node with a public WebSocket URL: the least-loaded node's
  `ws_url`, a `probe_url` (`<api_url>/health`), the node's coordinates, `nodes` available,
  `load_factor` and, when the caller sent `latitude`/`longitude`, `distance_km`. Order:
  requested `region` first, then distance, then load. Nodes without an advertised `wss://` URL
  (plain `http` `external_url` in production, or none) still serve traffic — they are simply
  not offered.
* `POST /v1/tokens` accepts the same `region` / `location` hints and returns the chosen
  `endpoint`, so a backend that knows where the player is can hand the SDK a node directly.
* SDKs ([Web](../sdk/web.md#region-selection), [Unity](../sdk/unity.md#region-selection),
  [native / Unreal](../sdk/native.md#region-selection)) probe each `probe_url` over HTTP and
  rank by measured RTT, so a mis-set `location` or a routing detour is corrected client-side.

Discovery returns **node** URLs rather than a regional balancer on purpose: sessions are
node-local (above), so the client should connect where it will reconnect; the node then
advertises up to `cluster.failover_endpoints` peers in `SessionInitAck.failover` for the case
where it disappears. Put a per-node
DNS name and TLS certificate in front of every node (the Helm chart and Terraform example do
this) and keep the shared hostname for the REST API. Cross-region channels still work through
cascade — with WAN latency between the nodes, so let players of one party land in one region.

## Capacity

A node's cost is dominated by the SFU fan-out (`speakers × (members − 1)` sealed packets per
frame). Reference numbers from the [load test](development.md#load-testing): 1000 sessions /
100 channels / 2 speakers → 90k pps out at ~0.4 core; 2000 sessions / 4 speakers → 360k pps at
~1.4 cores, 70 MB RSS. Tune `media.rx_workers` (UDP receive workers), the kernel receive buffers
(`net.core.rmem_max`, watch `Udp: RcvbufErrors` in `/proc/net/snmp`) and `ulimit -n` on the
host. `media.max_participants_per_node` / `max_channels_per_node` cap a node and feed the load
factor used by the registry (`available` = healthy and below 90 % of capacity).
