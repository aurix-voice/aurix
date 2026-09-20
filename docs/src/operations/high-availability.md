# High availability

Aurix keeps every durable fact in **PostgreSQL**, coordination state in **Redis**, and live
media (sockets, SFU sessions, jitter/replay state, cascade relays) in the memory of the node a
session is connected to. High availability is therefore three independent questions:

1. Does a player survive the loss of **their node**? — yes, by *cross-node failover*.
2. Does the fleet survive the loss of **Redis**? — degraded but up; sessions keep talking.
3. Does the fleet survive the loss of **PostgreSQL**? — existing sessions keep talking; new
   work fails until the database is back.

## Cross-node session failover

With `cluster.session_mirror = true` (default, needs Redis) every node writes a small **mirror**
of each session to Redis: session id, user, SSRC, resume-token hash (SHA-256 only — the token
itself never leaves the client), channels with roles, local mutes/gains/blocks, transmission
mode and focus, codec, downlink mode, transcript opt-out and the last downlink audio sequence.
The mirror is rewritten after every control message that changes it and refreshed periodically;
it lives `cluster.session_mirror_ttl_secs` (default 180) after the node stops refreshing it.

Each `SessionInitAck` also carries `failover`: up to `cluster.failover_endpoints` (default 3)
public `wss://` URLs of other healthy nodes, same region first, least loaded first. Only nodes
with `server.external_ws_url` are advertised.

When a node dies (or a client loses it for any reason) the SDKs rotate endpoints on every
reconnect attempt: attempt 1 goes to the current node, 2 to `failover[0]`, 3 to `failover[1]`,
…, then back to the current node, all with the usual backoff. A resume that lands on a node
that does not host the session is a **takeover**:

* the JWT is validated as for any connect and the resume token is compared (constant-time)
  against the mirrored hash; tenant and user must match the mirror;
* ownership moves with an atomic compare-and-set in Redis (Lua): the winner among concurrent
  reconnects becomes the owner, a loser falls back to a fresh session; a former owner that
  comes back can no longer write the session (fenced) and drops its stale copy;
* the SFU adopts the session with the **same session id and SSRC** but a **new media key and
  media endpoint**; channels, roles, mutes, gains, blocks, focus, codec, downlink mode and
  preferences are restored from the mirror, the database rows are moved to the new node
  (`sessions.media_node_id`, memberships stay open), and `SessionMigrated` is published so
  the other nodes update rosters and cascade routes without emitting participant-left/joined;
* the downlink audio sequence continues **above** the mirrored one (a jump of 65 536), so
  receivers' anti-replay windows keep accepting the stream and jitter buffers resynchronise on
  the gap instead of discarding it as old;
* the client sees `SessionInitAck {resumed: true, migrated: true, media_addr, media_key}` and
  the SDKs rebind media transparently (`endpointChanged` / `OnEndpointChanged` /
  `AURIX_EVENT_ENDPOINT_CHANGED` fires before `recovered`).

Measured locally the whole path (detect → rotate → take over → media on the new node) is about
one second on top of the reconnect backoff; see the Unity demo (`--scenario failover`, Bob on
the second node via `--ws-b`) and the two-node
`two_nodes_session_failover_resumes_on_the_other_node` E2E in
`crates/aurix-server/tests/e2e_live.rs`. Refusals are counted in
`aurix_ws_takeovers_refused_total{reason}` (`not_mirrored`, `denied`, `raced`, `redis`, …),
successes in `aurix_ws_sessions_migrated_total`.

If a takeover cannot complete (mirror expired, token mismatch, a downstream failure while
restoring) the ownership is handed back where possible and the client falls through to the
ordinary **fresh session** path with automatic re-join (`recovered {resumed: false}`), exactly
like a resume that expired on a single node.

### Lost nodes

A node whose registry heartbeat is older than `cluster.node_lost_after_secs` (default 30) is
treated as lost. One node claims the reaper role in Redis and closes the dead node's sessions
and memberships in the database with reason `node_lost`, telling every roster; cascade stops
relaying to it. Mirrors are **not** deleted by the reaper — a player whose node died can still
take the session elsewhere until the mirror TTL runs out; the takeover simply re-opens the
membership (peers then see a leave followed by a join instead of seamless continuity). Choose
`node_lost_after_secs` shorter than `session_mirror_ttl_secs`.

Graceful shutdown (`SIGTERM`) is different on purpose: the node sends
`SessionClose {reason: "server_shutdown"}` and deletes its rows, so SDKs open a fresh session
on the next endpoint instead of attempting a resume — see [Upgrades](deployment.md#upgrades).

### Load balancers and DNS

Session resume prefers the node that hosts the session; a balancer that spreads `/ws` across
nodes turns every reconnect into a takeover (works, but costs a media rebind and a topology
update). Recommended layout: one public hostname per node (`server.external_ws_url`), region
discovery / `POST /v1/tokens` to pick the first node, and the `failover` list for the rest.
With a shared balancer keep client-IP affinity on `/ws`.

## Redis

Redis holds only ephemeral state: cross-node events (Pub/Sub), session locators and mirrors,
node liveness beacons, one-time token claims, fleet-wide API rate limits, global mutes and
participant counters. It needs **no backup**; after a full loss clients reconnect and the state
rebuilds. Every command runs under a 2 s timeout so a hung Redis cannot stall a handler.

### Direct mode

```toml
[redis]
url = "redis://:password@redis.internal:6379/0"   # rediss:// for TLS
pool_size = 20
```

Use this with a single Redis or with a **managed HA endpoint** whose address does not change on
failover (ElastiCache primary endpoint, Memorystore, a keepalived VIP). The connection manager
reconnects with backoff when the endpoint drops, Pub/Sub re-subscribes on reconnect and
`/ready` pings Redis.

### Sentinel mode

```toml
[redis]
url = "redis://:password@ignored:6379/0"      # credentials/db/TLS options only; host is ignored
sentinels = ["redis://sentinel-1:26379", "redis://sentinel-2:26379", "redis://sentinel-3:26379"]
sentinel_master = "aurix"
pool_size = 20
```

The node resolves the current master through the sentinels at start (and refuses to start
without a master), then polls the sentinels every 5 s. When the master changes it swaps its
connection handle, bumps an internal generation so Pub/Sub re-subscribes on the new master, and
increments `aurix_redis_failovers_total`. Writes issued during the promotion window fail with a
timeout and the affected handler reports an error; mirrors are refreshed on the next cycle.
Both `sentinels` and `sentinel_master` must be set together (config validation rejects one
without the other).

### Redis Cluster

**Not supported.** The node uses a non-cluster client (no `MOVED`/`ASK` handling), the
ownership compare-and-set touches `session:{id}:node` and `session:{id}:mirror` in one Lua
script without hash tags (a cluster answers `CROSSSLOT`), and the event bus uses classic
Pub/Sub. Use Sentinel or a managed HA endpoint; cluster mode is not exercised by the test
suite.

### Behaviour while Redis is down

| function | during the outage |
|---|---|
| Live media, mutes, positional audio, chat on one node | unaffected (in memory) |
| Cross-node presence/chat/moderation events | not delivered; the cascade topology is re-derived from PostgreSQL every few seconds so relays keep working, rosters catch up on the next join or reconnect |
| Session mirrors / takeover | writes fail (logged); a reconnect that reaches another node in the window becomes a fresh session with automatic re-join |
| One-time tokens (`require_action_tokens`, join/resume tokens) | fail closed — the request is refused rather than replayed |
| API rate limits | fall back to the per-node limiter |
| `/ready` | fails, so a balancer stops sending *new* connections to the node; existing sessions stay |

## PostgreSQL

PostgreSQL is the source of truth for apps, users, channels and their configuration, sessions
and memberships (for rosters across nodes and for recovery), moderation, recordings, webhooks,
audit and analytics. It does **not** hold live media state: sockets, media keys, SFU routing,
jitter/replay windows and cascade relays exist only on the hosting node, which is why failover
needs the Redis mirror and cannot be done from the database alone.

### Topology

Run a primary with at least one streaming replica and a single **failover endpoint** in front
of it (Patroni + HAProxy/VIP, Stolon, pgBouncer in transaction mode, or a managed offering —
RDS Multi-AZ, Cloud SQL HA, Azure Flexible Server). Point `database.url` at that endpoint; all
nodes are equal, none needs the replica directly.

```toml
[database]
url = "postgres://aurix:password@pg-primary.internal:5432/aurix?sslmode=verify-full"
max_connections = 50
min_connections = 2
connect_timeout_secs = 5
idle_timeout_secs = 300
run_migrations = true
```

`max_connections × nodes` must fit the server's `max_connections` (or the pooler's). Connections
are checked out per query with `connect_timeout_secs` as the acquire budget; broken connections
are discarded and re-established transparently, so a primary switch shows up as a burst of
errors on in-flight statements followed by normal operation once the endpoint points at the new
primary. Aurix keeps no session-level state on the connection (no `LISTEN`, no temp tables);
the retention sweep takes a `pg_try_advisory_lock` for one batch and releases it. sqlx uses
protocol-level prepared statements, so a pooler in transaction mode must support them
(pgBouncer ≥ 1.21 with `max_prepared_statements`); otherwise use session mode.

### Migrations

Migrations are embedded, additive, and run inside sqlx's migration lock, so several nodes
starting at once apply them exactly once. A node whose schema is behind its binary refuses to
start. For controlled roll-outs set `run_migrations = false` and run
`aurix-server --migrate-only` from a job before deploying the new image
([Helm](deployment.md#kubernetes-helm) ships that job). Never run migrations against a replica.

### Consistency

* A session row is created when the WebSocket is accepted and closed (with a reason) when it
  ends; a membership row is opened on join and closed on leave, kick, node loss or shutdown.
  Cross-node rosters and cascade routes are derived from these rows plus Redis events, so a
  crashed node leaves them consistent within `node_lost_after_secs`.
* Multi-row changes (join with limits, moderation actions, user erasure, retention) run in
  transactions on the primary; Aurix never reads from replicas, so there is no read-your-writes
  problem. If you add read replicas for your own reporting, expect replication lag there only.
* Tombstones written by user erasure are part of the schema — a restored backup still rejects
  tokens issued before the erasure.

### Behaviour while PostgreSQL is down

| function | during the outage |
|---|---|
| Live media, mutes, positional audio, chat in already-joined channels | unaffected |
| New connections, joins, channel/config/moderation REST calls | fail with a database error (`500`) after `connect_timeout_secs` |
| Same-node resume | works (no database write on the hot path) |
| Cross-node takeover | refused when the row cannot be re-homed; ownership is handed back and the client gets a fresh session once the database answers again |
| Node registry heartbeat | fails, so registry views (`GET /v1/nodes`, region discovery) go stale; the reaper additionally checks the Redis liveness beacon, so a database-only outage reaps nobody |
| `/ready` | fails |

### Backups and PITR

Take a daily base backup plus continuous WAL archiving (pgBackRest, WAL-G, or the managed
provider's PITR) so you can restore to a point in time; `pg_dump` alone is sufficient only for
small deployments (see [Backups and observability](observability.md)). Recordings and the
recording encryption key are backed up separately. Before restoring, stop all nodes: the
session/membership rows in a backup are stale by definition and are cleaned up as nodes start
(`recover_node_state`) — no manual cleanup is needed, but nodes that are still running would
write into the old primary.

## Health and alerts

* `GET /health` — process up; `GET /ready` — PostgreSQL and Redis reachable from this node.
* `aurix_redis_failovers_total` and `aurix_nodes_reaped_total` rising,
  `aurix_ws_takeovers_refused_total` growing faster than `aurix_ws_sessions_migrated_total`,
  and `aurix_ws_sessions_detached` staying high are the signals worth paging on.
* Test the whole path before you need it: kill one node under load and confirm the players on
  it show `recovered {resumed: true, migrated: true}` within a few seconds, and that
  `GET /v1/nodes` marks the node unhealthy and later lost.
