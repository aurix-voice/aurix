#!/usr/bin/env bash
# Aurix chaos / HA harness.
#
# Brings up PostgreSQL + a Redis master/replica pair watched by three Sentinels (Docker, host
# network), starts two Aurix nodes as plain processes against them, provisions an admin and two
# tenants, and then breaks things one at a time while the repository's live E2E tests
# (crates/aurix-server/tests/e2e_live.rs) prove the fleet keeps working:
#
#   baseline          two nodes, auto-cascade audio + fleet-wide rate limits (sanity)
#   doctor            `aurix doctor` with node 1's environment: running node, bound listeners,
#                     Redis in the configured mode, PostgreSQL + migrations, QUIC handshake
#   node-kill         SIGKILL node 2 mid-session: registry marks it stale, the reaper closes its
#                     rows, the player resumes on node 1 with the same session id / SSRC, node 2
#                     is restarted and rejoins the fleet
#   redis-failover    SIGKILL the Redis master: Sentinel promotes the replica, both nodes follow
#                     the switch (aurix_redis_failovers_total), become /ready again and the
#                     cross-node paths (Pub/Sub cascade, fleet rate limits) work on the new master;
#                     the old master returns as a replica
#   postgres-restart  stop/start PostgreSQL: both nodes go not-ready, recover their pools, and a
#                     write-heavy scenario (session resume, persistent cross-mute) passes
#   isolation         tenant / session isolation after all of the above (second API key, admin)
#
# With AURIX_CHAOS_REDIS=cluster the Sentinel pair is replaced by a six-node Redis Cluster
# (docker-compose.cluster.yml, three masters + one replica each) and `redis-failover` becomes a
# shard failover: the control-plane live tests (crates/aurix-control/tests/redis_live.rs) prove
# the hash-tagged session mirrors, fleet limiters and sharded Pub/Sub on a real cluster and kill
# the master holding the event slot mid-test; then the harness kills the (new) event-slot master
# under the running nodes, which must go not-ready, re-attach to the promoted replica and pass
# the cross-node scenarios again before the dead master rejoins as a replica.
#
# Usage:
#   tools/chaos/run.sh              # everything: up, nodes, all scenarios, down
#   tools/chaos/run.sh up nodes     # just the topology (then poke at it yourself)
#   tools/chaos/run.sh node-kill    # one scenario against an already running topology
#   tools/chaos/run.sh down
#   AURIX_CHAOS_REDIS=cluster tools/chaos/run.sh
#
# Environment:
#   AURIX_CHAOS_BIN   aurix-server binary (default target/debug/aurix-server; built if missing)
#   AURIX_CHAOS_CLI   aurix CLI binary for the doctor scenario (default target/debug/aurix)
#   AURIX_CHAOS_DIR   state directory: logs, pids, credentials (default target/chaos)
#   AURIX_CHAOS_KEEP  1 = leave the topology running after `all`
#   AURIX_CHAOS_REDIS sentinel (default) | cluster
#
# tools/soak/run.sh sources this file for the topology and the failure primitives (nothing runs
# on `source`; the dispatch at the bottom only fires when executed directly). NODE_EXTRA_ENV
# may hold additional `AURIX__…=…` assignments for every node started by start_node.
#
# All secrets used here (bootstrap token, cascade secret, Postgres password) are throw-away,
# loopback-only values that exist only for the lifetime of the run; the API keys the harness
# creates are written to $AURIX_CHAOS_DIR with mode 0600 and never printed.
set -euo pipefail

ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)
HERE="$ROOT/tools/chaos"
STATE="${AURIX_CHAOS_DIR:-$ROOT/target/chaos}"
BIN="${AURIX_CHAOS_BIN:-$ROOT/target/debug/aurix-server}"
REDIS_MODE="${AURIX_CHAOS_REDIS:-sentinel}"
case $REDIS_MODE in
  sentinel) COMPOSE=(docker compose -f "$HERE/docker-compose.yml") ;;
  cluster) COMPOSE=(docker compose -f "$HERE/docker-compose.cluster.yml") ;;
  *) echo "AURIX_CHAOS_REDIS must be sentinel or cluster" >&2; exit 1 ;;
esac

PG_URL="postgres://aurix:aurix-chaos@127.0.0.1:5433/aurix"
SENTINELS="redis://127.0.0.1:26380,redis://127.0.0.1:26381,redis://127.0.0.1:26382"
SENTINEL_CLI=(docker exec aurix-chaos-sentinel-1 redis-cli -p 26380)
CLUSTER_PORTS=(7000 7001 7002 7003 7004 7005)
CLUSTER_SEEDS="redis://127.0.0.1:7000,redis://127.0.0.1:7001,redis://127.0.0.1:7002"
EVENT_CHANNEL='{aurix}:events'

API1=http://127.0.0.1:8180  WS1=ws://127.0.0.1:8181  METRICS1=http://127.0.0.1:9180/metrics
API2=http://127.0.0.1:8190  WS2=ws://127.0.0.1:8191  METRICS2=http://127.0.0.1:9190/metrics
NODE1_ID=0193c000-0000-7000-8000-000000000001
NODE2_ID=0193c000-0000-7000-8000-000000000002
BOOTSTRAP_TOKEN=chaos-only-bootstrap-token-0123456789
CASCADE_SECRET=chaos-only-cascade-secret-0123456789abcdef
ADMIN_EMAIL=chaos@example.com
ADMIN_PASSWORD=chaos-admin-password-0123456789
REPORTS_PER_MINUTE=3
E2E_EXTRA_ENV=()
NODE_EXTRA_ENV=()

log()  { printf '\033[1;36m[chaos]\033[0m %s\n' "$*" >&2; }
fail() { printf '\033[1;31m[chaos] FAIL:\033[0m %s\n' "$*" >&2; exit 1; }

# ── helpers ──────────────────────────────────────────────────────────────────────────────────

wait_for() { # <description> <timeout-seconds> <command...>
  local what=$1 timeout=$2; shift 2
  local deadline=$((SECONDS + timeout))
  until "$@"; do
    (( SECONDS < deadline )) || fail "timed out after ${timeout}s waiting for: $what"
    sleep 1
  done
}

ready() { curl -fsS -o /dev/null -m 2 "$1/ready" 2>/dev/null; }
not_ready() { ! ready "$1"; }

metric() { # <metrics-url> <metric-name>  → integer value (0 if absent)
  curl -fsS -m 2 "$1" 2>/dev/null | awk -v m="$2" '$1 == m { print int($2); found = 1 } END { if (!found) print 0 }'
}

metric_at_least() { [ "$(metric "$1" "$2")" -ge "$3" ]; }

sentinel_master_port() {
  "${SENTINEL_CLI[@]}" SENTINEL get-master-addr-by-name aurix | sed -n 2p
}

sentinel_has_master() { [ -n "$(sentinel_master_port 2>/dev/null)" ]; }

master_changed_from() { [ "$(sentinel_master_port)" != "$1" ]; }

redis_role() { # <port>  (asked through a Sentinel container, which is never the one killed)
  docker exec aurix-chaos-sentinel-1 redis-cli -p "$1" ROLE 2>/dev/null | head -1
}

redis_is_replica() { [ "$(redis_role "$1")" = "slave" ]; }

postgres_healthy() {
  [ "$(docker inspect -f '{{.State.Health.Status}}' aurix-chaos-postgres 2>/dev/null)" = healthy ]
}

container_for_port() {
  case $1 in
    6380) echo aurix-chaos-redis-a ;;
    6381) echo aurix-chaos-redis-b ;;
    700[0-5]) echo "aurix-chaos-redis-$(( $1 - 7000 ))" ;;
    *) fail "unknown Redis port $1" ;;
  esac
}

# ── Redis Cluster helpers (host:port everywhere, as CLUSTER SLOTS reports them) ──────────────

cluster_cli() { # <port> <redis-cli args...>  — through that node's own container
  local port=$1; shift
  docker exec "$(container_for_port "$port")" redis-cli -p "$port" "$@"
}

cluster_live_port() { # first cluster node that answers (never assume a particular one is up)
  local p
  for p in "${CLUSTER_PORTS[@]}"; do
    [ "$(cluster_cli "$p" PING 2>/dev/null | tr -d '\r')" = PONG ] && { echo "$p"; return; }
  done
  return 1
}

cluster_state_ok() {
  local p; p=$(cluster_live_port) || return 1
  [ "$(cluster_cli "$p" CLUSTER INFO 2>/dev/null | tr -d '\r' | awk -F: '$1 == "cluster_state" { print $2 }')" = ok ]
}

cluster_role() { # <port>
  cluster_cli "$1" ROLE 2>/dev/null | head -1 | tr -d '\r'
}

cluster_is_replica() { [ "$(cluster_role "$1")" = "slave" ]; }

# The master serving the slot of the cross-node event channel: the shard whose death cuts the
# fleet's Pub/Sub until its replica is promoted.
cluster_event_master() {
  local p slot
  p=$(cluster_live_port) || return 1
  slot=$(cluster_cli "$p" CLUSTER KEYSLOT "$EVENT_CHANNEL" | tr -d '\r')
  # CLUSTER SLOTS entries are [start, end, master[ip, port, id], replica...].
  cluster_cli "$p" --json CLUSTER SLOTS \
    | jq -r --argjson slot "$slot" \
        '.[] | select(.[0] <= $slot and .[1] >= $slot) | .[2] | "\(.[0]):\(.[1])"'
}

cluster_event_master_changed_from() { [ "$(cluster_event_master)" != "$1" ]; }

cluster_create() {
  local p nodes=()
  for p in "${CLUSTER_PORTS[@]}"; do nodes+=("127.0.0.1:$p"); done
  cluster_cli 7000 --cluster create "${nodes[@]}" --cluster-replicas 1 --cluster-yes >/dev/null
}

# Hooks the control-plane failover test calls (AURIX_E2E_REDIS_CLUSTER_KILL / _START <host:port>).
hook_kill_redis() { docker kill --signal=KILL "$(container_for_port "${1##*:}")" >/dev/null; }
hook_start_redis() { docker start "$(container_for_port "${1##*:}")" >/dev/null; }

pid_file() { echo "$STATE/node$1.pid"; }

node_alive() { # <n>
  local f; f=$(pid_file "$1")
  [ -f "$f" ] && kill -0 "$(cat "$f")" 2>/dev/null
}

admin_token() { cat "$STATE/admin-token"; }

node_healthy_in_registry() { # <node-id> <true|false>
  local got
  got=$(curl -fsS -m 3 "$API1/v1/nodes" -H "authorization: Bearer $(admin_token)" \
    | jq -r --arg id "$1" '.[] | select(.id == $id) | .healthy')
  [ "$got" = "$2" ]
}

e2e() { # <test-name...>  — runs the named live E2E tests against the chaos fleet
  log "e2e: $*"
  (
    cd "$ROOT"
    env AURIX_E2E_API="$API1" AURIX_E2E_WS="$WS1" \
        AURIX_E2E_API2="$API2" AURIX_E2E_WS2="$WS2" \
        AURIX_E2E_API_KEY="$(cat "$STATE/api-key")" \
        AURIX_E2E_API_KEY2="$(cat "$STATE/api-key2")" \
        AURIX_E2E_ADMIN_TOKEN="$(admin_token)" \
        AURIX_E2E_REPORTS_PER_MINUTE="$REPORTS_PER_MINUTE" \
        AURIX_E2E_MAX_CHANNELS=4 \
        "${E2E_EXTRA_ENV[@]}" \
        cargo test --locked -p aurix-server --test e2e_live -- --ignored --test-threads=1 --exact "$@"
  )
}

# ── topology ─────────────────────────────────────────────────────────────────────────────────

cmd_up() {
  mkdir -p "$STATE"
  chmod 700 "$STATE"
  if [ "$REDIS_MODE" = cluster ]; then
    log "starting PostgreSQL and six Redis Cluster nodes"
    "${COMPOSE[@]}" up -d --wait
    if ! cluster_state_ok; then
      log "bootstrapping the cluster (3 masters, 1 replica each)"
      cluster_create
    fi
    wait_for "cluster_state:ok" 60 cluster_state_ok
    log "Redis Cluster is up; event channel on $(cluster_event_master)"
    return
  fi
  log "starting PostgreSQL, Redis master/replica and 3 Sentinels"
  "${COMPOSE[@]}" up -d --wait
  wait_for "Sentinel master" 30 sentinel_has_master
  wait_for "replica synced" 30 redis_is_replica 6381
  log "Redis master is 127.0.0.1:$(sentinel_master_port), replica on 6381"
}

redis_env() { # env assignments for the configured Redis backend
  if [ "$REDIS_MODE" = cluster ]; then
    echo "AURIX__REDIS__CLUSTER=$CLUSTER_SEEDS"
  else
    echo "AURIX__REDIS__URL=redis://127.0.0.1:6380"
    echo "AURIX__REDIS__SENTINELS=$SENTINELS"
    echo "AURIX__REDIS__SENTINEL_MASTER=aurix"
  fi
}

start_node() { # <1|2>
  local n=$1 api ws media metrics id
  case $n in
    1) api=8180 ws=8181 media=10110 metrics=9180 id=$NODE1_ID ;;
    2) api=8190 ws=8191 media=10120 metrics=9190 id=$NODE2_ID ;;
    *) fail "node $n" ;;
  esac
  node_alive "$n" && { log "node $n already running"; return; }
  [ -x "$BIN" ] || { log "building aurix-server"; (cd "$ROOT" && cargo build --locked --bin aurix-server); }
  log "starting node $n (api $api, ws $ws, media $media)"
  mkdir -p "$STATE/recordings-$n"
  local redis_env=()
  mapfile -t redis_env < <(redis_env)
  (
    cd "$ROOT"
    env AURIX__DATABASE__URL="$PG_URL" \
        AURIX__DATABASE__CONNECT_TIMEOUT_SECS=5 \
        "${redis_env[@]}" \
        AURIX__SERVER__NODE_ID="$id" \
        AURIX__SERVER__API_PORT="$api" AURIX__SERVER__WS_PORT="$ws" \
        AURIX__SERVER__EXTERNAL_URL="http://127.0.0.1:$api" \
        AURIX__SERVER__EXTERNAL_WS_URL="ws://127.0.0.1:$ws/ws" \
        AURIX__SERVER__SESSION_RESUME_GRACE_SECS=4 \
        AURIX__MEDIA__PORT="$media" AURIX__MEDIA__EXTERNAL_IP=127.0.0.1 \
        AURIX__MEDIA__CASCADE_SECRET="$CASCADE_SECRET" \
        AURIX__MEDIA__CASCADE_DISCOVERY_INTERVAL_MS=2000 \
        AURIX__MEDIA__MAX_CHANNELS_PER_SESSION=4 \
        AURIX__QUALITY__PERSIST_INTERVAL_SECS=10 \
        AURIX__METRICS__PORT="$metrics" \
        AURIX__CLUSTER__NODE_LOST_AFTER_SECS=10 \
        AURIX__TURN__ENABLED=false \
        AURIX__RECORDING__ENABLED=true AURIX__RECORDING__STORAGE_PATH="$STATE/recordings-$n" \
        AURIX__CHAT__PERSIST=true \
        AURIX__RATE_LIMITING__ENABLED=true \
        AURIX__RATE_LIMITING__REQUESTS_PER_SECOND=500 AURIX__RATE_LIMITING__BURST_SIZE=1000 \
        AURIX__RATE_LIMITING__REPORTS_PER_MINUTE=$REPORTS_PER_MINUTE \
        AURIX__AUTH__ADMIN_BOOTSTRAP_TOKEN="$BOOTSTRAP_TOKEN" \
        AURIX__TRACING__LOG_FORMAT=text \
        "${NODE_EXTRA_ENV[@]}" \
        nohup "$BIN" >>"$STATE/node$n.log" 2>&1 &
    echo $! >"$(pid_file "$n")"
  )
  wait_for "node $n /ready" 60 ready "http://127.0.0.1:$api"
}

kill_node() { # <1|2>  — SIGKILL, no graceful shutdown: the fleet must notice on its own
  local f; f=$(pid_file "$1")
  if [ -f "$f" ]; then
    kill -9 "$(cat "$f")" 2>/dev/null || true
    rm -f "$f"
  fi
}

cmd_nodes() {
  start_node 1
  start_node 2
  wait_for "both nodes in the registry" 30 node_healthy_in_registry_or_bootstrap
}

node_healthy_in_registry_or_bootstrap() {
  [ -f "$STATE/admin-token" ] || return 0
  node_healthy_in_registry "$NODE1_ID" true && node_healthy_in_registry "$NODE2_ID" true
}

cmd_bootstrap() {
  [ -f "$STATE/api-key2" ] && { log "tenants already provisioned"; return; }
  log "bootstrapping admin and two tenants"
  local token key key2
  curl -fsS -o /dev/null -X POST "$API1/admin/setup" -H 'content-type: application/json' \
    -H "x-bootstrap-token: $BOOTSTRAP_TOKEN" \
    -d "{\"email\":\"$ADMIN_EMAIL\",\"password\":\"$ADMIN_PASSWORD\",\"display_name\":\"Chaos\"}"
  token=$(curl -fsS -X POST "$API1/admin/login" -H 'content-type: application/json' \
    -d "{\"email\":\"$ADMIN_EMAIL\",\"password\":\"$ADMIN_PASSWORD\"}" | jq -r .token)
  key=$(curl -fsS -X POST "$API1/v1/apps" -H "authorization: Bearer $token" \
    -H 'content-type: application/json' -d '{"name":"chaos"}' | jq -r .api_key)
  key2=$(curl -fsS -X POST "$API2/v1/apps" -H "authorization: Bearer $token" \
    -H 'content-type: application/json' -d '{"name":"chaos-tenant2"}' | jq -r .api_key)
  [ -n "$token" ] && [ -n "$key" ] && [ -n "$key2" ] || fail "bootstrap returned empty credentials"
  umask 077
  printf '%s' "$token" >"$STATE/admin-token"
  printf '%s' "$key" >"$STATE/api-key"
  printf '%s' "$key2" >"$STATE/api-key2"
  wait_for "both nodes healthy in the registry" 30 node_healthy_in_registry_or_bootstrap
}

cmd_down() {
  log "stopping nodes and backing services"
  kill_node 1; kill_node 2
  "${COMPOSE[@]}" down -v --remove-orphans >/dev/null 2>&1 || true
  rm -f "$STATE/admin-token" "$STATE/api-key" "$STATE/api-key2"
}

# `aurix doctor` with node 1's listener-shaping environment: it must recognise the running
# node, see every configured listener bound, PING Redis in the configured mode (sentinel or
# cluster), reach PostgreSQL with current migrations and handshake QUIC — without printing
# the database password.
cmd_doctor() {
  local cli="${AURIX_CHAOS_CLI:-$ROOT/target/debug/aurix}" report="$STATE/doctor.json"
  [ -x "$cli" ] || { log "building aurix CLI"; (cd "$ROOT" && cargo build --locked --bin aurix); }
  log "scenario: aurix doctor against node 1 ($REDIS_MODE)"
  local redis_env=()
  mapfile -t redis_env < <(redis_env)
  (
    cd "$ROOT"
    export AURIX__DATABASE__URL="$PG_URL" "${redis_env[@]}" \
      AURIX__SERVER__NODE_ID="$NODE1_ID" \
      AURIX__SERVER__API_PORT=8180 AURIX__SERVER__WS_PORT=8181 \
      AURIX__SERVER__EXTERNAL_URL="http://127.0.0.1:8180" \
      AURIX__SERVER__EXTERNAL_WS_URL="ws://127.0.0.1:8181/ws" \
      AURIX__MEDIA__PORT=10110 AURIX__MEDIA__EXTERNAL_IP=127.0.0.1 \
      AURIX__MEDIA__CASCADE_SECRET="$CASCADE_SECRET" \
      AURIX__METRICS__PORT=9180 AURIX__TURN__ENABLED=false
    "$cli" doctor
    "$cli" -o json doctor >"$report"
  )
  jq -e '.ok and .mode == "running"' "$report" >/dev/null || fail "doctor did not pass against the running node"
  local id
  for id in node port.api port.ws port.media port.cascade port.cascade_tcp redis postgres migrations probe.quic; do
    jq -e --arg id "$id" '.checks[] | select(.id == $id) | .status == "ok"' "$report" >/dev/null \
      || fail "doctor check $id is not ok: $(jq -c --arg id "$id" '.checks[] | select(.id == $id)' "$report")"
  done
  jq -e --arg mode "$REDIS_MODE" '.checks[] | select(.id == "redis") | .details.mode == $mode' "$report" >/dev/null \
    || fail "doctor reported the wrong Redis mode: $(jq -c '.checks[] | select(.id == "redis")' "$report")"
  local pg_password="${PG_URL#*://*:}"; pg_password="${pg_password%%@*}"
  if [ -n "$pg_password" ] && grep -qF "$pg_password" "$report"; then
    fail "doctor leaked the PostgreSQL password into its report"
  fi
  log "doctor: $(jq -r '"\(.counts.ok) ok, \(.counts.warn) warn, \(.counts.fail) fail; redis \(.checks[] | select(.id == "redis") | .summary)"' "$report")"
}

# ── scenarios ────────────────────────────────────────────────────────────────────────────────

cmd_baseline() {
  log "scenario: baseline (two nodes, cascade + fleet rate limits)"
  e2e two_nodes_auto_cascade_relays_audio fleet_rate_limits_hold_across_nodes
}

# Hooks the failover test calls in the middle of a session (AURIX_E2E_NODE2_STOP / _START).
hook_stop_node2() {
  kill_node 2
  # Registry-level stale detection: the fixed 30 s heartbeat timeout flips `healthy` before the
  # reaper (cluster.node_lost_after_secs = 10 here) closes the node's sessions.
  wait_for "node 2 marked unhealthy in the registry" 60 node_healthy_in_registry "$NODE2_ID" false
  wait_for "reaper closed node 2 (aurix_nodes_reaped_total on node 1)" 60 \
    metric_at_least "$METRICS1" aurix_nodes_reaped_total $(( ${AURIX_CHAOS_REAPED_BEFORE:-0} + 1 ))
}

hook_start_node2() {
  start_node 2
  wait_for "node 2 healthy in the registry again" 30 node_healthy_in_registry "$NODE2_ID" true
}

cmd_node_kill() {
  log "scenario: node kill (SIGKILL node 2 mid-session, resume on node 1, restart)"
  local before; before=$(metric "$METRICS1" aurix_nodes_reaped_total)
  E2E_EXTRA_ENV=(
    "AURIX_E2E_NODE2_STOP=$HERE/run.sh hook-stop-node2"
    "AURIX_E2E_NODE2_START=$HERE/run.sh hook-start-node2"
    "AURIX_CHAOS_REAPED_BEFORE=$before"
  )
  e2e two_nodes_session_failover_resumes_on_the_other_node
  E2E_EXTRA_ENV=()
  [ "$(metric "$METRICS1" aurix_nodes_reaped_total)" -gt "$before" ] || fail "node 1 never reaped node 2"
  node_alive 2 || fail "node 2 was not restarted by the hook"
  # And the fleet is whole again: cross-node audio flows through the restarted node.
  e2e two_nodes_auto_cascade_relays_audio
}

cmd_redis_failover() {
  if [ "$REDIS_MODE" = cluster ]; then
    cmd_cluster_failover
    return
  fi
  log "scenario: Redis Sentinel failover (SIGKILL the master)"
  local old_port old_container f1 f2
  old_port=$(sentinel_master_port)
  old_container=$(container_for_port "$old_port")
  f1=$(metric "$METRICS1" aurix_redis_failovers_total)
  f2=$(metric "$METRICS2" aurix_redis_failovers_total)
  log "killing Redis master 127.0.0.1:$old_port ($old_container)"
  docker kill --signal=KILL "$old_container" >/dev/null
  wait_for "Sentinel to elect a new master" 60 master_changed_from "$old_port"
  log "new master is 127.0.0.1:$(sentinel_master_port)"
  wait_for "node 1 to follow the switch" 45 metric_at_least "$METRICS1" aurix_redis_failovers_total $((f1 + 1))
  wait_for "node 2 to follow the switch" 45 metric_at_least "$METRICS2" aurix_redis_failovers_total $((f2 + 1))
  wait_for "node 1 /ready" 30 ready "$API1"
  wait_for "node 2 /ready" 30 ready "$API2"
  # Cross-node Pub/Sub (cascade discovery, roster events) and the Redis-backed fleet rate
  # limiter must work against the promoted replica.
  e2e two_nodes_auto_cascade_relays_audio fleet_rate_limits_hold_across_nodes
  log "bringing the old master back; Sentinel must demote it to a replica"
  docker start "$old_container" >/dev/null
  wait_for "old master rejoined as replica" 60 redis_is_replica "$old_port"
  [ "$(sentinel_master_port)" != "$old_port" ] || fail "old master took the role back"
}

cmd_cluster_failover() {
  log "scenario: Redis Cluster shard failover"
  # 1. Control plane on a real cluster: same-slot Lua scripts, fleet limiters, sharded Pub/Sub,
  #    and the event-slot master killed mid-test (the test drives the kill/start hooks itself).
  log "control-plane live tests against the cluster (incl. shard kill)"
  (
    cd "$ROOT"
    env AURIX_E2E_REDIS_CLUSTER="$CLUSTER_SEEDS" \
        AURIX_E2E_REDIS_CLUSTER_KILL="$HERE/run.sh hook-kill-redis" \
        AURIX_E2E_REDIS_CLUSTER_START="$HERE/run.sh hook-start-redis" \
        cargo test --locked -p aurix-control --test redis_live -- --ignored --test-threads=1
  )
  wait_for "cluster_state:ok after the control-plane run" 60 cluster_state_ok
  # 2. The same failover under the running fleet.
  local old old_port
  old=$(cluster_event_master); old_port=${old##*:}
  log "killing the event-slot master $old ($(container_for_port "$old_port"))"
  docker kill --signal=KILL "$(container_for_port "$old_port")" >/dev/null
  wait_for "cluster to promote the replica" 60 cluster_event_master_changed_from "$old"
  log "event slot moved to $(cluster_event_master)"
  wait_for "node 1 /ready" 45 ready "$API1"
  wait_for "node 2 /ready" 45 ready "$API2"
  # Cross-node Pub/Sub (cascade discovery, roster events) and the Redis-backed fleet rate
  # limiter must work with the slot on the promoted replica.
  e2e two_nodes_auto_cascade_relays_audio fleet_rate_limits_hold_across_nodes
  log "bringing $old back; it must rejoin as a replica"
  docker start "$(container_for_port "$old_port")" >/dev/null
  wait_for "old master rejoined as replica" 60 cluster_is_replica "$old_port"
  [ "$(cluster_event_master)" != "$old" ] || fail "old master took the slot back"
  wait_for "cluster_state:ok" 30 cluster_state_ok
}

cmd_postgres_restart() {
  log "scenario: PostgreSQL restart"
  docker stop aurix-chaos-postgres >/dev/null
  # Readiness is a live `SELECT 1`: both nodes must go not-ready while Postgres is down.
  wait_for "node 1 not-ready without PostgreSQL" 20 not_ready "$API1"
  wait_for "node 2 not-ready without PostgreSQL" 20 not_ready "$API2"
  docker start aurix-chaos-postgres >/dev/null
  wait_for "PostgreSQL healthy" 60 postgres_healthy
  wait_for "node 1 /ready" 60 ready "$API1"
  wait_for "node 2 /ready" 60 ready "$API2"
  # Pools must have recovered: resume (session rows), persistent cross-mute (user_blocks) and
  # the tenant checks inside it all write to PostgreSQL.
  e2e session_resume_after_ws_drop local_mute_volume_and_persistent_cross_mute
}

cmd_isolation() {
  log "scenario: tenant / session isolation after the chaos"
  e2e local_mute_volume_and_persistent_cross_mute \
      webhooks_and_sse_deliver_signed_tenant_scoped_events \
      user_erasure_export_and_stale_token_rejection
}

cmd_all() {
  cmd_up
  cmd_nodes
  cmd_bootstrap
  cmd_baseline
  cmd_doctor
  cmd_node_kill
  cmd_redis_failover
  cmd_postgres_restart
  cmd_isolation
  log "all chaos scenarios passed"
  [ "${AURIX_CHAOS_KEEP:-0}" = 1 ] || cmd_down
}

# ── dispatch ─────────────────────────────────────────────────────────────────────────────────

[ "${BASH_SOURCE[0]}" = "$0" ] || return 0

if [ $# -eq 0 ]; then
  cmd_all
  exit 0
fi
case $1 in
  hook-kill-redis) hook_kill_redis "$2"; exit 0 ;;
  hook-start-redis) hook_start_redis "$2"; exit 0 ;;
esac
for cmd in "$@"; do
  case $cmd in
    all) cmd_all ;;
    up) cmd_up ;;
    nodes) cmd_nodes ;;
    bootstrap) cmd_bootstrap ;;
    baseline) cmd_baseline ;;
    doctor) cmd_doctor ;;
    node-kill) cmd_node_kill ;;
    redis-failover) cmd_redis_failover ;;
    postgres-restart) cmd_postgres_restart ;;
    isolation) cmd_isolation ;;
    down) cmd_down ;;
    hook-stop-node2) hook_stop_node2 ;;
    hook-start-node2) hook_start_node2 ;;
    *) fail "unknown command '$cmd' (all|up|nodes|bootstrap|baseline|doctor|node-kill|redis-failover|postgres-restart|isolation|down)" ;;
  esac
done
