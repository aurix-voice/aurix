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

## Node maintenance (drain)

`POST /v1/nodes/{id}/drain` (admin JWT, `nodes:drain`, optional `{"reason": "…"}` up to 512
characters) puts a node into **operator drain**; `POST /v1/nodes/{id}/undrain` lifts it. The
state lives in `media_nodes` (`draining`, `drain_reason`, `draining_since`, `drained_by`), is
returned as `MediaNode.drain {reason, since, by}`, survives heartbeats and restarts, and is
applied by the drained node on its next heartbeat (`media.heartbeat_interval_ms`, 5 s) — the
node that took the request applies it immediately. Both transitions are audited
(`node_drained` / `node_undrained` with the actor).

A draining node keeps `healthy` and `relay_only` exactly as they are; only admission changes:

* it is skipped by node selection, region discovery (`GET /v1/regions`, `/v1/me/regions`) and
  the failover list handed to new sessions on other nodes;
* its `/ws` answers `503` to fresh sessions **and** to cross-node takeovers — clients move on
  to the next failover endpoint, so a drain never strands a player;
* sessions already on it stay, keep their audio and can still **resume** there; they leave
  when the players do (or on `SIGTERM`, which closes them with `server_shutdown`).

`aurix node drain <id> --reason "kernel update"` / `aurix node undrain <id>` do the same from
the [CLI](../backend/cli.md). Combine with a rolling restart: drain, wait for
`active_participants` to reach zero (or a deadline), restart, undrain.

## Where a session lives

A session is anchored to the node that accepted its **WebSocket**: `SessionInitAck.media_addr`
points to that node's `media.external_ip:media.port`, its media key lives in that node's SFU, and
node-local resources (WebRTC peer connection, recordings, per-session REST stats) are served
there. Consequences:

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
  for a session hosted elsewhere and a recording follows the participant's node. `GET
  /v1/channels/{id}/participants` reports `live_on_this_node`, `GET /v1/nodes` gives each node's
  address — an operator tool retries against the right node or relies on webhooks/SSE, which
  are fleet-wide. Live audio streams are the exception: a stream is owned by the node that
  opened it (the cascade relays the channel's audio there), and every node lists, inspects,
  stops (`202` when forwarded to the owner) and resumes (`pull?resume=<id>`) any stream of the
  tenant ([Recordings and live streams](../features/recordings.md)).

## Cross-node events

Channel/participant events (join, leave, mute, ban, kick, block, chat, transcripts, energy,
webhook-subscription invalidation, `user.deleted`, `SessionMigrated`) are published on Redis pub/sub with the
origin node id; a node never re-applies its own events. Redis also holds one-time token claims
(`jti`), session→node mapping and session mirrors for failover, node liveness beacons, global
mutes and distributed rate limits. Without Redis a single node works; a fleet does not. Redis
Sentinel and Redis Cluster are both supported — see [High availability](high-availability.md#redis).

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
| `e2ee` | user | `e2ee_messages_per_minute` (3000) `E2eeHello` / `E2eeSenderKey` relays — a rotation costs one message per peer in the channel |
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
`media.cascade_discovery = false` switches to a fully static full mesh. Hand-configured peers
always receive every channel the node hosts, whatever the topology below says.

### Topology: mesh or region tree

`media.cascade_topology` decides how the hosting nodes of a channel are connected:

* `mesh` — every hosting node sends its participants' audio directly to every other hosting
  node (one hop). Simple and lowest-latency, but a channel with `n` hosting nodes spread over
  regions puts `n − 1` copies of every stream on the WAN, and **all** nodes need direct UDP
  reachability to each other.
* `region_tree` (default) — hosting nodes in the same region still talk directly, but traffic
  between regions goes through exactly one **hub per region**: origin → own region's hub →
  remote region's hub → hosting nodes there (typically 2–3 node-to-node hops; hub to hub is a
  single WAN hop). Each inter-regional link carries one copy of each stream no matter how many
  nodes host the channel in the destination region, and only the hubs need cross-region
  reachability on `media.port + 1` — in-region nodes only need to reach each other and their
  hub. Where a pair of nodes cannot talk directly the tree grows a level (below).

Hubs are elected **per channel** and deterministically from the same registry *and link*
snapshot every node sees ([measured links](#measured-links-rtt-and-the-tcp-fallback)):
candidates that reach every node they would have to talk to first, then the region's
relay-only nodes, then the lowest summed RTT towards the region's hosts and the other regions
(in 25 ms buckets, TCP-only links count +100 ms, unmeasured links 150 ms), tie-broken by a
channel/node hash so hub duty spreads across a region instead of pinning to one node. Two
things make the tree deeper than one hub per region when the links demand it:

* **Core hub.** When two regions' hubs do not reach each other — or reach each other slower
  than through a third hub by more than 30 ms — that direction goes through a *core* hub (the
  hub that reaches every hub with the lowest detour), one more level: host → hub → core hub →
  hub → host.
* **In-region star.** When two hosting nodes of the same region do not reach each other, that
  region's in-region traffic goes through its hub as well instead of the direct one-hop link.

There is no coordination protocol; every node plans only its own part of the tree (whom it
sends its participants' audio to, and which ingress peer's packets it forwards where), and a
hub forwards a packet only to the edges of that ingress — never back to where it came from.
Re-forwarded envelopes carry a hop byte ([`RelayHop`](../api/aurx.md#flags)) and a node never
forwards an envelope whose count reached the cap (`MAX_RELAY_HOPS` = 5), so a stale or
inconsistent plan during a reconciliation window cannot loop traffic; the per-peer anti-replay
window still applies at every hop and
the envelope is re-sealed hop by hop under the same `cascade_secret` (original sender, SSRC,
audio level and E2EE payload are preserved; receiver preferences are still applied only on the
node that hosts the receiver). A mesh node and a tree node can share a cluster while you roll
the setting out: envelopes without the hop byte are accepted and delivered locally, just never
re-forwarded.

**Relay-only hubs.** A node started with `media.cascade_relay_only = true` (requires
`cascade_secret`, `cascade_discovery` and `region_tree`) hosts no clients: it registers with
no `ws_url` and `capacity = 0` (`relay_only: true` in `GET /v1/nodes`), `/ws` answers `503`,
region discovery and failover never offer it, and it is the preferred hub of its region for
every channel that has participants there. It sizes like a packet forwarder (no Opus, no mixing)
and is the piece you place next to your inter-regional backbone; a region without one simply
elects one of its hosting nodes. When a hub disappears from the healthy registry (missed
heartbeats, `NodeHealthChanged`), every node re-plans on its next pass — the periodic interval
or the event fast path — and a new hub is elected; until then cross-region audio for the
affected channels is lost, in-region audio is unaffected.

`aurix_cascade_forwarded_total{role="origin"|"hub"|"hop_limit"}` counts envelopes this node
sent as an origin, re-forwarded as a hub, or refused to re-forward because the hop cap was
reached (`hop_limit` staying at zero is the healthy state); `aurix_cascade_hub_channels` is the
number of channels this node currently hubs.

### Measured links, RTT and the TCP fallback

Every node pings each registry peer on the cascade port every
`media.cascade_probe_interval_ms` (1000) — a sealed `Heartbeat` ping/pong inside the same
`cascade_secret` envelope as media — and keeps a smoothed RTT per peer and transport. The
result is published to `media_node_links` (one row per `(node, peer)`: `transport`, `rtt_ms`,
`measured_at`) on every reconciliation pass and read back as the fleet-wide link matrix the
planner above ranks hubs with. Rows older than three passes plus the probe window are ignored;
a link is *confirmed* when either side measured it (the worse direction wins), *unconfirmed*
when both nodes publish tables but neither heard the other (blocked — the planner routes
around it), and *unknown* when a node has not published yet (a node predating link probes, or
just started — treated as an average 150 ms link so mixed fleets keep working).
`GET /v1/nodes/links` (`nodes:read`) / `aurix node links` show the table.

When a peer misses three UDP probes in a row and `media.cascade_tcp_fallback = true` (default),
the node dials the peer's cascade port over **TCP** (same port number, length-prefixed frames of
the very same sealed envelopes — nothing is decrypted or re-keyed, so client E2EE and the
relay authentication are untouched) and moves that peer's envelopes onto the TCP link while
UDP keeps being probed; as soon as UDP answers again the link switches back and the idle TCP
connection is closed after 30 s. Inbound TCP connections must open with a sealed `Hello` from an
allowed peer within 5 s (bounded to 1024 connections, 60 s idle), every frame passes the same
per-peer anti-replay window as UDP, and each outbound link has a bounded queue (256 frames):
a stalled peer drops its own audio (`aurix_cascade_tcp_dropped_total`) and never blocks the
SFU. `aurix_cascade_links{transport="udp"|"tcp"|"unconfirmed"}` counts peers by the transport
currently used towards them — `tcp` staying at zero is the healthy state; a non-zero value
names a firewall between two nodes that should be opened for UDP, the fallback is there so audio
keeps flowing meanwhile (TCP head-of-line blocking under loss, like the client tunnel). The
link table feeds the planner too: a TCP-only pair is ranked 100 ms worse, so a hub whose UDP is
blocked loses the election to one that is reachable, and a channel takes the detour through a
core hub or an in-region star rather than a blocked direct link.

Not covered: the planner knows RTT and reachability, not link bandwidth or loss, and it plans
per channel — there is no fleet-wide balancing of hub duty by load beyond the hash spread.

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
frame) and by whatever the node decodes. Measured on one 8 vCPU host with the
[load generator](development.md#reference-runs-160) at 1000 sessions / 100 channels × 10 /
2 speakers (10k frames/s in, 90k out), all 100 % delivered unless noted:

| path | server CPU | RSS | note |
|---|---|---|---|
| UDP | 0.43 core | 159 MiB | baseline |
| QUIC / TLS tunnel / WebTransport / WS tunnel | 0.80 / 0.60 / 0.85 / 0.53 core | 165–171 MiB | encrypted transports cost 1.4–2× UDP for the same frames |
| + server noise suppression (200 uplinks) | 3.50 cores | 235 MiB | ~15 ms CPU per second of audio per denoised uplink |
| server mix, audience shape (100 shared + 200 private mixers) | 4.97 cores | 230 MiB | 99.98 % delivered; a team shape (1000 private mixers) does not fit this host |
| server mix + noise suppression | 7.73 cores | 307 MiB | 93.6 % delivered — saturated, over the limit |
| cascade, 2 nodes, UDP or TCP fallback | 0.25 core per node | 69–274 MiB | half the fan-out per node; the forwarding hop itself adds no measurable CPU |

Rule of thumb from those runs: the per-stream path is cheap (~5–9 µs of CPU per delivered frame,
network-bound long before CPU-bound), each denoised uplink costs ~1.5 % of a core, each mixer
roughly one decoder per speaker plus one encoder per downlink. Keep the node's sum well below its
core count and give listeners `speak: false` grants so a mixed channel needs one shared mixer,
not one per member. Tune `media.rx_workers` (UDP receive workers), the kernel socket buffers
(`net.core.rmem_max` / `wmem_max` ≥ 4 MiB, watch `Udp: RcvbufErrors` in `/proc/net/snmp`) and
`ulimit -n` on the host. `media.max_participants_per_node` / `max_channels_per_node` cap a node
and feed the load factor used by the registry (`available` = healthy and below 90 % of capacity).
