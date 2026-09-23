#!/usr/bin/env bash
# Aurix soak harness: hours of real native clients on a two-node fleet under recurring chaos.
#
# Reuses the chaos topology (tools/chaos/run.sh: PostgreSQL + Redis Sentinel/Cluster in Docker,
# two aurix-server processes on loopback) and drives `aurix-soak` against it: bots built on the
# real client core talk in a few channels split across both nodes (so the audio crosses the
# cascade), refresh their tokens like a game backend would, and keep going while these hooks run
# in rotation every $AURIX_SOAK_CHAOS_EVERY:
#
#   node-kill-2        SIGKILL node 2 → registry marks it stale → its players fail over to
#                      node 1 (same session id / SSRC) → node 2 restarts and rejoins the fleet
#   redis-failover     SIGKILL the Redis master (Sentinel) or the event-slot master (Cluster);
#                      both nodes must follow the promotion and become /ready again
#   node-kill-1        the same for node 1 (players are now on both nodes again)
#   postgres-restart   stop/start PostgreSQL: nodes go not-ready and recover their pools
#   udp-blackhole-1    drop UDP to node 1's media port for $AURIX_SOAK_BLACKHOLE_SECS: its
#                      players fall back to QUIC/TLS/WS on their own, then re-probe UDP
#
# aurix-soak writes one JSONL row per interval (bots bound/joined/hearing, loss, RTT, MOS,
# reconnects, node RSS/fds/sessions, harness RSS) to $AURIX_SOAK_DIR/report.jsonl, a verdict to
# $AURIX_SOAK_DIR/summary.json, and exits non-zero when a budget is exceeded: steady-state
# hearing/loss/hard reconnects, recovery time after each hook, RSS/fd growth, leftover sessions.
#
# Usage:
#   tools/soak/run.sh                          # 2 h, 16 bots, hooks every 10 min
#   AURIX_SOAK_DURATION=48h AURIX_SOAK_CLIENTS=32 tools/soak/run.sh
#   AURIX_SOAK_DURATION=12m AURIX_SOAK_CHAOS_EVERY=90s AURIX_SOAK_WARMUP=60s tools/soak/run.sh
#   tools/soak/run.sh hook-node-kill 2         # one hook against a running soak topology
#   tools/soak/run.sh down
#
# Environment:
#   AURIX_SOAK_DURATION        total run time (default 2h)
#   AURIX_SOAK_CLIENTS         bots (default 16), AURIX_SOAK_CHANNELS (4), AURIX_SOAK_SPEAKERS (2)
#   AURIX_SOAK_INTERVAL        report interval (default 60s), AURIX_SOAK_WARMUP (3m)
#   AURIX_SOAK_CHAOS_EVERY     gap between hooks (default 10m), AURIX_SOAK_CHAOS_FIRST (5m),
#                              AURIX_SOAK_CHAOS_SETTLE (2m), AURIX_SOAK_RECOVER_WITHIN (120s)
#   AURIX_SOAK_HOOKS           comma-separated rotation (default
#                              node-kill-2,redis-failover,node-kill-1,postgres-restart,udp-blackhole-1,netem-2)
#   AURIX_SOAK_BLACKHOLE_SECS  UDP blackhole / netem length (default 40); both hooks are skipped
#                              without passwordless sudo (iptables / tc)
#   AURIX_SOAK_NETEM           netem parameters for the netem-N hook
#                              (default "loss 8% delay 40ms 15ms reorder 25% 50%")
#   AURIX_SOAK_TOKEN_TTL       node token TTL in seconds (default 900) — bots refresh at TTL/3
#   AURIX_SOAK_EXTRA_ARGS      extra aurix-soak flags (e.g. "--max-loss 1 --min-hearing 0.98")
#   AURIX_SOAK_DIR             state/report directory (default target/soak)
#   AURIX_SOAK_KEEP            1 = leave the topology running afterwards
#   AURIX_CHAOS_REDIS          sentinel (default) | cluster — same as the chaos harness
#   AURIX_CHAOS_BIN            aurix-server binary (default target/debug/aurix-server)
#   AURIX_SOAK_BIN             aurix-soak binary (default target/debug/aurix-soak)
set -euo pipefail

ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)
export AURIX_CHAOS_DIR="${AURIX_SOAK_DIR:-$ROOT/target/soak}"
# shellcheck source=../chaos/run.sh
source "$ROOT/tools/chaos/run.sh"

SOAK_BIN="${AURIX_SOAK_BIN:-$ROOT/target/debug/aurix-soak}"
DURATION="${AURIX_SOAK_DURATION:-2h}"
CLIENTS="${AURIX_SOAK_CLIENTS:-16}"
CHANNELS="${AURIX_SOAK_CHANNELS:-4}"
SPEAKERS="${AURIX_SOAK_SPEAKERS:-2}"
INTERVAL="${AURIX_SOAK_INTERVAL:-60s}"
WARMUP="${AURIX_SOAK_WARMUP:-3m}"
CHAOS_EVERY="${AURIX_SOAK_CHAOS_EVERY:-10m}"
CHAOS_FIRST="${AURIX_SOAK_CHAOS_FIRST:-5m}"
CHAOS_SETTLE="${AURIX_SOAK_CHAOS_SETTLE:-2m}"
RECOVER_WITHIN="${AURIX_SOAK_RECOVER_WITHIN:-120s}"
BLACKHOLE_SECS="${AURIX_SOAK_BLACKHOLE_SECS:-40}"
HOOKS="${AURIX_SOAK_HOOKS:-node-kill-2,redis-failover,node-kill-1,postgres-restart,udp-blackhole-1,netem-2}"
NETEM="${AURIX_SOAK_NETEM:-loss 8% delay 40ms 15ms reorder 25% 50%}"
TOKEN_TTL="${AURIX_SOAK_TOKEN_TTL:-900}"
MEDIA1=10110 MEDIA2=10120 TLS1=10443 TLS2=10453

# Soak nodes differ from the chaos nodes only where hours matter: a realistic resume window
# (chaos uses 4 s to make its tests fast), short tokens so refresh is exercised, and the
# dedicated TLS tunnel so the blackhole hook has every fallback path to choose from.
soak_node_env() { # <1|2>
  local tls; case $1 in 1) tls=$TLS1 ;; 2) tls=$TLS2 ;; *) fail "node $1" ;; esac
  NODE_EXTRA_ENV=(
    "AURIX__SERVER__SESSION_RESUME_GRACE_SECS=30"
    "AURIX__AUTH__TOKEN_TTL_SECS=$TOKEN_TTL"
    "AURIX__MEDIA__TLS_TUNNEL_PORT=$tls"
  )
}

start_soak_node() { soak_node_env "$1"; start_node "$1"; }

cmd_nodes() {
  start_soak_node 1
  start_soak_node 2
  wait_for "both nodes in the registry" 30 node_healthy_in_registry_or_bootstrap
}

slog() { printf '\033[1;35m[soak]\033[0m %s\n' "$*" >&2; }

node_id() { case $1 in 1) echo "$NODE1_ID" ;; 2) echo "$NODE2_ID" ;; *) fail "node $1" ;; esac; }
other_api() { case $1 in 1) echo "$API2" ;; 2) echo "$API1" ;; esac; }
other_metrics() { case $1 in 1) echo "$METRICS2" ;; 2) echo "$METRICS1" ;; esac; }
node_api() { case $1 in 1) echo "$API1" ;; 2) echo "$API2" ;; esac; }

registry_says() { # <api> <node-id> <true|false>
  local got
  got=$(curl -fsS -m 3 "$1/v1/nodes" -H "authorization: Bearer $(admin_token)" \
    | jq -r --arg id "$2" '.[] | select(.id == $id) | .healthy')
  [ "$got" = "$3" ]
}

# ── hooks (each leaves the fleet whole again before returning) ─────────────────────────────────

hook_node_kill() { # <1|2>
  local n=$1 id api metrics before
  id=$(node_id "$n"); api=$(other_api "$n"); metrics=$(other_metrics "$n")
  before=$(metric "$metrics" aurix_nodes_reaped_total)
  slog "SIGKILL node $n"
  kill_node "$n"
  wait_for "node $n marked unhealthy in the registry" 60 registry_says "$api" "$id" false
  wait_for "surviving node reaped node $n" 60 metric_at_least "$metrics" aurix_nodes_reaped_total $((before + 1))
  # Leave it dead long enough for the players to settle on the other node.
  sleep 15
  start_soak_node "$n"
  wait_for "node $n healthy in the registry again" 60 registry_says "$api" "$id" true
}

hook_redis_failover() {
  local f1 f2
  f1=$(metric "$METRICS1" aurix_redis_failovers_total)
  f2=$(metric "$METRICS2" aurix_redis_failovers_total)
  if [ "$REDIS_MODE" = cluster ]; then
    local old old_port
    old=$(cluster_event_master); old_port=${old##*:}
    slog "SIGKILL Redis Cluster event-slot master $old"
    docker kill --signal=KILL "$(container_for_port "$old_port")" >/dev/null
    wait_for "cluster to promote the replica" 60 cluster_event_master_changed_from "$old"
    wait_for "node 1 /ready" 60 ready "$API1"
    wait_for "node 2 /ready" 60 ready "$API2"
    docker start "$(container_for_port "$old_port")" >/dev/null
    wait_for "old master rejoined as replica" 60 cluster_is_replica "$old_port"
    wait_for "cluster_state:ok" 30 cluster_state_ok
    return
  fi
  local old_port old_container
  old_port=$(sentinel_master_port); old_container=$(container_for_port "$old_port")
  slog "SIGKILL Redis master 127.0.0.1:$old_port"
  docker kill --signal=KILL "$old_container" >/dev/null
  wait_for "Sentinel to elect a new master" 60 master_changed_from "$old_port"
  wait_for "node 1 to follow the switch" 60 metric_at_least "$METRICS1" aurix_redis_failovers_total $((f1 + 1))
  wait_for "node 2 to follow the switch" 60 metric_at_least "$METRICS2" aurix_redis_failovers_total $((f2 + 1))
  wait_for "node 1 /ready" 30 ready "$API1"
  wait_for "node 2 /ready" 30 ready "$API2"
  docker start "$old_container" >/dev/null
  wait_for "old master rejoined as replica" 60 redis_is_replica "$old_port"
}

hook_postgres_restart() {
  slog "stopping PostgreSQL"
  docker stop aurix-chaos-postgres >/dev/null
  wait_for "node 1 not-ready without PostgreSQL" 30 not_ready "$API1"
  wait_for "node 2 not-ready without PostgreSQL" 30 not_ready "$API2"
  sleep 10
  docker start aurix-chaos-postgres >/dev/null
  wait_for "PostgreSQL healthy" 60 postgres_healthy
  wait_for "node 1 /ready" 60 ready "$API1"
  wait_for "node 2 /ready" 60 ready "$API2"
}

hook_udp_blackhole() { # <1|2> <seconds>
  local n=$1 secs=$2 port
  case $n in 1) port=$MEDIA1 ;; 2) port=$MEDIA2 ;; *) fail "node $n" ;; esac
  local rule=(INPUT -p udp --dport "$port" -j DROP -m comment --comment aurix-soak)
  slog "dropping UDP to node $n media port $port for ${secs}s"
  sudo -n iptables -A "${rule[@]}"
  trap 'sudo -n iptables -D "${rule[@]}" 2>/dev/null || true' EXIT
  sleep "$secs"
  sudo -n iptables -D "${rule[@]}"
  trap - EXIT
  wait_for "node $n /ready" 30 ready "$(node_api "$n")"
}

hook_netem() { # <1|2> <seconds> <netem args…>
  local n=$1 secs=$2 port shape="$ROOT/tools/netem/shape.sh"
  shift 2
  case $n in 1) port=$MEDIA1 ;; 2) port=$MEDIA2 ;; *) fail "node $n" ;; esac
  slog "netem on node $n media port $port for ${secs}s: $*"
  sudo -n "$shape" apply "$port" --down "$*" --up "$*"
  trap 'sudo -n "$shape" clear || true' EXIT
  sleep "$secs"
  sudo -n "$shape" clear
  trap - EXIT
}

# ── run ────────────────────────────────────────────────────────────────────────────────────────

build_soak() {
  [ -x "$SOAK_BIN" ] || { slog "building aurix-soak"; (cd "$ROOT" && cargo build --locked --bin aurix-soak); }
}

cmd_soak() {
  local self="$ROOT/tools/soak/run.sh" hooks=() name
  for name in ${HOOKS//,/ }; do
    case $name in
      node-kill-1) hooks+=(--chaos "$name=$self hook-node-kill 1") ;;
      node-kill-2) hooks+=(--chaos "$name=$self hook-node-kill 2") ;;
      redis-failover) hooks+=(--chaos "$name=$self hook-redis-failover") ;;
      postgres-restart) hooks+=(--chaos "$name=$self hook-postgres-restart") ;;
      udp-blackhole-1 | udp-blackhole-2)
        if sudo -n iptables -L INPUT -n >/dev/null 2>&1; then
          hooks+=(--chaos "$name=$self hook-udp-blackhole ${name##*-} $BLACKHOLE_SECS")
        else
          slog "no passwordless sudo/iptables: skipping $name"
        fi ;;
      netem-1 | netem-2)
        if sudo -n tc qdisc show dev lo >/dev/null 2>&1; then
          hooks+=(--chaos "$name=$self hook-netem ${name##*-} $BLACKHOLE_SECS $NETEM")
        else
          slog "no passwordless sudo/tc: skipping $name"
        fi ;;
      *) fail "unknown hook '$name' in AURIX_SOAK_HOOKS" ;;
    esac
  done
  local extra=()
  # shellcheck disable=SC2206
  [ -z "${AURIX_SOAK_EXTRA_ARGS:-}" ] || extra=(${AURIX_SOAK_EXTRA_ARGS})
  rm -f "$STATE/report.jsonl" "$STATE/summary.json"
  slog "soak: $CLIENTS bots / $CHANNELS channels / $SPEAKERS speakers for $DURATION, hooks every $CHAOS_EVERY"
  AURIX_SOAK_API_KEY="$(cat "$STATE/api-key")" "$SOAK_BIN" \
    --api "$API1" --ws "$WS1" --metrics "$METRICS1" \
    --api "$API2" --ws "$WS2" --metrics "$METRICS2" \
    --duration "$DURATION" --interval "$INTERVAL" --warmup "$WARMUP" \
    --clients "$CLIENTS" --channels "$CHANNELS" --speakers "$SPEAKERS" \
    --chaos-every "$CHAOS_EVERY" --chaos-first-after "$CHAOS_FIRST" --chaos-settle "$CHAOS_SETTLE" \
    --recover-within "$RECOVER_WITHIN" --token-refresh "$((TOKEN_TTL / 3))s" \
    --report "$STATE/report.jsonl" --summary "$STATE/summary.json" \
    "${hooks[@]}" "${extra[@]}"
}

cmd_all() {
  cmd_up
  cmd_nodes
  cmd_bootstrap
  build_soak
  local rc=0
  cmd_soak || rc=$?
  if [ "$rc" -eq 0 ]; then slog "soak passed"; else slog "soak FAILED (exit $rc); see $STATE/report.jsonl and node logs"; fi
  [ "${AURIX_SOAK_KEEP:-0}" = 1 ] || cmd_down
  return "$rc"
}

if [ $# -eq 0 ]; then
  cmd_all
  exit $?
fi
case $1 in
  hook-node-kill) hook_node_kill "$2"; exit 0 ;;
  hook-redis-failover) hook_redis_failover; exit 0 ;;
  hook-postgres-restart) hook_postgres_restart; exit 0 ;;
  hook-udp-blackhole) hook_udp_blackhole "$2" "$3"; exit 0 ;;
  hook-netem) shift; hook_netem "$@"; exit 0 ;;
esac
for cmd in "$@"; do
  case $cmd in
    all) cmd_all ;;
    up) cmd_up ;;
    nodes) cmd_nodes ;;
    bootstrap) cmd_bootstrap ;;
    soak) build_soak; cmd_soak ;;
    down) cmd_down ;;
    *) fail "unknown command '$cmd' (all|up|nodes|bootstrap|soak|down|hook-node-kill N|hook-redis-failover|hook-postgres-restart|hook-udp-blackhole N SECS|hook-netem N SECS ARGS…)" ;;
  esac
done
