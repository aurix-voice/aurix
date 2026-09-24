#!/usr/bin/env bash
# Runs the whole Rooms demo on one host: PostgreSQL + Redis (Docker), an Aurix node, the Rooms
# backend serving the built frontend, the bot, Caddy as the single origin and — optionally — an
# HTTP tunnel for a public HTTPS address. Everything lives under $ROOMS_STATE (default
# ~/.aurix-rooms): generated secrets, the API key, pid files and logs. Nothing is written to the
# repository.
#
#   deploy/run.sh up          start (or re-attach to) every component, bootstrap the node once
#   deploy/run.sh down        stop the processes this script started (databases stay up)
#   deploy/run.sh status      health of every component
#   deploy/run.sh logs <name> tail a component log: node | rooms | bot | ngrok | tunnel
#
# Environment:
#   PUBLIC_ORIGIN   https://<host> the browser will use (default http://127.0.0.1:8000)
#   NGROK_DOMAIN    start `ngrok http` on this reserved domain (needs an authenticated ngrok agent);
#                   PUBLIC_ORIGIN defaults to https://$NGROK_DOMAIN
#   QUICK_TUNNEL    localhost.run | cloudflared — an account-less tunnel with a random host that
#                   changes on every start (ssh -R to localhost.run, or a cloudflared quick tunnel;
#                   localhost.run also rotates the host of a running tunnel). The tunnel comes up
#                   first and its current host is PUBLIC_ORIGIN; the backend derives the browser's
#                   API/WS URLs from the forwarded request, so a rotated host keeps working without
#                   a restart — `status` prints the host that is live right now.
#   ROOMS_TRANSPORT websocket | auto | webrtc | webtransport (default: websocket when a tunnel is
#                   in front — HTTP tunnels carry no UDP — otherwise auto)
#   AURIX_BIN / BOT_BIN   binaries (default target/release/…; built when missing)
#   ROOMS_ASSETS    playlist directory (default $ROOMS_STATE/assets, fetched when missing)
#   ROOMS_PROXY_PORT (8000) ROOMS_PORT (8090) AURIX_API_PORT (8080) AURIX_WS_PORT (8081)
set -euo pipefail

ROOT=$(cd "$(dirname "$0")/../../.." && pwd)
HERE=$ROOT/examples/rooms
STATE=${ROOMS_STATE:-$HOME/.aurix-rooms}
AURIX_BIN=${AURIX_BIN:-$ROOT/target/release/aurix-server}
BOT_BIN=${BOT_BIN:-$ROOT/target/release/aurix-rooms-bot}
ROOMS_ASSETS=${ROOMS_ASSETS:-$STATE/assets}
ROOMS_PROXY_PORT=${ROOMS_PROXY_PORT:-8000}
ROOMS_PORT=${ROOMS_PORT:-8090}
AURIX_API_PORT=${AURIX_API_PORT:-8080}
AURIX_WS_PORT=${AURIX_WS_PORT:-8081}
AURIX_MEDIA_PORT=${AURIX_MEDIA_PORT:-4000}
PG_URL=${PG_URL:-postgres://aurix:aurix@127.0.0.1:5432/aurix}
REDIS_URL=${REDIS_URL:-redis://127.0.0.1:6379}
NGROK_DOMAIN=${NGROK_DOMAIN:-}
QUICK_TUNNEL=${QUICK_TUNNEL:-}
if [ -n "$NGROK_DOMAIN" ]; then
  PUBLIC_ORIGIN=${PUBLIC_ORIGIN:-https://$NGROK_DOMAIN}
  ROOMS_TRANSPORT=${ROOMS_TRANSPORT:-websocket}
elif [ -n "$QUICK_TUNNEL" ]; then
  PUBLIC_ORIGIN=${PUBLIC_ORIGIN:-}
  ROOMS_TRANSPORT=${ROOMS_TRANSPORT:-websocket}
else
  PUBLIC_ORIGIN=${PUBLIC_ORIGIN:-http://127.0.0.1:$ROOMS_PROXY_PORT}
  ROOMS_TRANSPORT=${ROOMS_TRANSPORT:-auto}
fi
API=http://127.0.0.1:$AURIX_API_PORT
ROOMS=http://127.0.0.1:$ROOMS_PORT
CADDY_CONTAINER=aurix-rooms-caddy

log() { printf '\033[1m[rooms]\033[0m %s\n' "$*" >&2; }
fail() { log "error: $*"; exit 1; }
need() { command -v "$1" >/dev/null || fail "$1 is required"; }

pid_alive() { [ -f "$STATE/$1.pid" ] && kill -0 "$(cat "$STATE/$1.pid")" 2>/dev/null; }
wait_for() { # <label> <seconds> <cmd...>
  local label=$1 secs=$2; shift 2
  for _ in $(seq 1 "$((secs * 2))"); do "$@" >/dev/null 2>&1 && return 0; sleep 0.5; done
  fail "timed out waiting for $label"
}
secret() { # <name> [bytes] — generated once, 0600
  local f=$STATE/$1
  [ -s "$f" ] || { umask 077; openssl rand -hex "${2:-32}" | tr -d '\n' >"$f"; }
  cat "$f"
}

ensure_container() { # <name> <image> <port-map> [args...]
  local name=$1 image=$2 ports=$3; shift 3
  if docker inspect -f '{{.State.Running}}' "$name" 2>/dev/null | grep -q true; then return; fi
  if docker inspect "$name" >/dev/null 2>&1; then docker start "$name" >/dev/null; return; fi
  log "starting $name ($image)"
  docker run -d --name "$name" --restart unless-stopped -p "$ports" "$@" "$image" >/dev/null
}

cmd_databases() {
  need docker
  ensure_container aurix-pg postgres:16-alpine 127.0.0.1:5432:5432 \
    -e POSTGRES_USER=aurix -e POSTGRES_PASSWORD=aurix -e POSTGRES_DB=aurix
  ensure_container aurix-redis redis:7-alpine 127.0.0.1:6379:6379
  wait_for PostgreSQL 60 docker exec aurix-pg pg_isready -U aurix
  wait_for Redis 30 docker exec aurix-redis redis-cli ping
}

cmd_node() {
  [ -n "$PUBLIC_ORIGIN" ] || fail "PUBLIC_ORIGIN is not known yet"
  pid_alive node && { log "node already running"; return; }
  [ -x "$AURIX_BIN" ] || { log "building aurix-server"; (cd "$ROOT" && cargo build --release --locked --bin aurix-server); }
  log "starting aurix-server (api $AURIX_API_PORT, ws $AURIX_WS_PORT)"
  local jwt bootstrap
  jwt=$(secret jwt-secret) bootstrap=$(secret bootstrap-token 24)
  (
    cd "$ROOT"
    env AURIX__DATABASE__URL="$PG_URL" \
        AURIX__REDIS__URL="$REDIS_URL" \
        AURIX__SERVER__API_PORT="$AURIX_API_PORT" AURIX__SERVER__WS_PORT="$AURIX_WS_PORT" \
        AURIX__SERVER__EXTERNAL_URL="$PUBLIC_ORIGIN" \
        AURIX__SERVER__EXTERNAL_WS_URL="${PUBLIC_ORIGIN/http/ws}/ws" \
        AURIX__SERVER__CORS_ORIGINS="$PUBLIC_ORIGIN" \
        AURIX__SERVER__TRUSTED_PROXIES="127.0.0.1/32" \
        AURIX__AUTH__JWT_SECRET="$jwt" \
        AURIX__AUTH__ADMIN_BOOTSTRAP_TOKEN="$bootstrap" \
        AURIX__MEDIA__PORT="$AURIX_MEDIA_PORT" AURIX__MEDIA__EXTERNAL_IP=127.0.0.1 \
        AURIX__MEDIA__MEDIA_TUNNEL=true \
        AURIX__TURN__ENABLED=false \
        AURIX__CHAT__PERSIST=true \
        AURIX__TRACING__LOG_FORMAT=text \
        nohup "$AURIX_BIN" >>"$STATE/node.log" 2>&1 &
    echo $! >"$STATE/node.pid"
  )
  wait_for "node /ready" 60 curl -fsS "$API/ready"
}

cmd_bootstrap() {
  [ -s "$STATE/api-key" ] && return
  need curl; need jq
  log "bootstrapping admin + app"
  local password token key
  password=$(secret admin-password 16)
  curl -fsS -o /dev/null -X POST "$API/admin/setup" -H 'content-type: application/json' \
    -H "x-bootstrap-token: $(secret bootstrap-token 24)" \
    -d "{\"email\":\"rooms@localhost\",\"password\":\"$password\",\"display_name\":\"Rooms\"}" ||
    log "admin/setup refused (already bootstrapped?) — trying login"
  token=$(curl -fsS -X POST "$API/admin/login" -H 'content-type: application/json' \
    -d "{\"email\":\"rooms@localhost\",\"password\":\"$password\"}" | jq -r .token)
  key=$(curl -fsS -X POST "$API/v1/apps" -H "authorization: Bearer $token" \
    -H 'content-type: application/json' -d '{"name":"rooms"}' | jq -r .api_key)
  [ -n "$key" ] && [ "$key" != null ] || fail "app creation returned no API key"
  (umask 077; printf '%s' "$key" >"$STATE/api-key")
}

cmd_frontend() {
  [ -f "$HERE/web/dist/index.html" ] && [ "${REBUILD:-0}" = 0 ] && return
  need npm
  log "building frontend"
  (cd "$HERE/web" && npm ci --no-audit --no-fund && npm run build) >>"$STATE/frontend-build.log" 2>&1 ||
    fail "frontend build failed, see $STATE/frontend-build.log"
}

cmd_rooms() {
  pid_alive rooms && { log "rooms backend already running"; return; }
  need node
  [ -d "$HERE/server/node_modules" ] || (cd "$HERE/server" && npm ci --no-audit --no-fund >>"$STATE/frontend-build.log" 2>&1)
  log "starting rooms backend ($ROOMS_PORT, transport $ROOMS_TRANSPORT)"
  mkdir -p "$STATE/data"
  local public_api= public_ws=
  if [ -z "$QUICK_TUNNEL" ]; then
    public_api=$PUBLIC_ORIGIN public_ws=${PUBLIC_ORIGIN/http/ws}/ws
  fi
  (
    cd "$HERE/server"
    env PORT="$ROOMS_PORT" HOST=127.0.0.1 \
        AURIX_URL="$API" AURIX_WS_URL="ws://127.0.0.1:$AURIX_WS_PORT/ws" \
        AURIX_API_KEY_FILE="$STATE/api-key" \
        ROOMS_BOT_TOKEN="$(secret bot-token)" \
        ROOMS_TRANSPORT="$ROOMS_TRANSPORT" \
        ROOMS_PUBLIC_API_URL="$public_api" ROOMS_PUBLIC_WS_URL="$public_ws" \
        ROOMS_STATE_FILE="$STATE/data/rooms.json" \
        ROOMS_STATIC_DIR="$HERE/web/dist" \
        nohup node server.mjs >>"$STATE/rooms.log" 2>&1 &
    echo $! >"$STATE/rooms.pid"
  )
  wait_for "rooms /healthz" 30 curl -fsS "$ROOMS/healthz"
}

cmd_caddy() {
  need docker
  if docker inspect -f '{{.State.Running}}' $CADDY_CONTAINER 2>/dev/null | grep -q true; then
    log "caddy already running"; return
  fi
  docker rm -f $CADDY_CONTAINER >/dev/null 2>&1 || true
  log "starting caddy (:$ROOMS_PROXY_PORT)"
  docker run -d --name $CADDY_CONTAINER --restart unless-stopped --network host \
    -e ROOMS_PROXY_PORT="$ROOMS_PROXY_PORT" \
    -e AURIX_UPSTREAM="127.0.0.1:$AURIX_API_PORT" -e AURIX_WS_UPSTREAM="127.0.0.1:$AURIX_WS_PORT" \
    -e ROOMS_UPSTREAM="127.0.0.1:$ROOMS_PORT" \
    -v "$HERE/deploy/Caddyfile:/etc/caddy/Caddyfile:ro" caddy:2-alpine >/dev/null
  wait_for "caddy" 30 curl -fsS "http://127.0.0.1:$ROOMS_PROXY_PORT/healthz"
}

cmd_bot() {
  pid_alive bot && { log "bot already running"; return; }
  [ -x "$BOT_BIN" ] || { log "building aurix-rooms-bot"; (cd "$ROOT" && cargo build --release --locked --bin aurix-rooms-bot); }
  [ -f "$ROOMS_ASSETS/playlist.json" ] || { log "fetching playlist assets"; "$HERE/bot/fetch-assets.sh" "$ROOMS_ASSETS" >>"$STATE/bot.log" 2>&1; }
  log "starting bot"
  env ROOMS_URL="$ROOMS" ROOMS_BOT_TOKEN="$(secret bot-token)" ROOMS_BOT_ASSETS="$ROOMS_ASSETS" \
    nohup "$BOT_BIN" >>"$STATE/bot.log" 2>&1 &
  echo $! >"$STATE/bot.pid"
}

cmd_ngrok() {
  [ -n "$NGROK_DOMAIN" ] || return 0
  pid_alive ngrok && { log "ngrok already running"; return; }
  need ngrok
  log "starting ngrok → https://$NGROK_DOMAIN"
  nohup ngrok http "$ROOMS_PROXY_PORT" --url "https://$NGROK_DOMAIN" --log stdout --log-format logfmt \
    >>"$STATE/ngrok.log" 2>&1 &
  echo $! >"$STATE/ngrok.pid"
  wait_for "tunnel" 30 curl -fsS -o /dev/null "https://$NGROK_DOMAIN/healthz"
}

# Quick tunnels get a fresh host on every start (localhost.run also re-announces a new one on a
# running tunnel); it is only known from the agent's own output.
start_quick_tunnel() {
  case "$QUICK_TUNNEL" in
    localhost.run)
      need ssh
      nohup ssh -T -o StrictHostKeyChecking=accept-new -o ServerAliveInterval=30 \
        -o ExitOnForwardFailure=yes -R "80:127.0.0.1:$ROOMS_PROXY_PORT" nokey@localhost.run \
        >>"$STATE/tunnel.log" 2>&1 </dev/null &
      echo $! >"$STATE/tunnel.pid"
      ;;
    cloudflared)
      need cloudflared
      nohup cloudflared tunnel --url "http://127.0.0.1:$ROOMS_PROXY_PORT" --no-autoupdate \
        >>"$STATE/tunnel.log" 2>&1 &
      echo $! >"$STATE/tunnel.pid"
      ;;
    *) fail "QUICK_TUNNEL must be localhost.run or cloudflared" ;;
  esac
}

# The most recently announced public host of the running quick tunnel, empty when none yet.
tunnel_host() {
  local pattern
  case "$QUICK_TUNNEL" in
    localhost.run) pattern='https://[a-z0-9]*\.lhr\.life' ;;
    cloudflared) pattern='https://[a-z0-9-]*\.trycloudflare\.com' ;;
    *) return 0 ;;
  esac
  [ -f "$STATE/tunnel.log" ] || return 0
  sed 's/\x1b\[[0-9;]*m//g' "$STATE/tunnel.log" | grep -a -o "$pattern" | tail -n 1 || true
}

cmd_tunnel() {
  [ -n "$QUICK_TUNNEL" ] || return 0
  local origin_file=$STATE/public-origin host=
  if ! pid_alive tunnel; then
    log "starting $QUICK_TUNNEL quick tunnel"
    : >"$STATE/tunnel.log"
    start_quick_tunnel
  fi
  for _ in $(seq 1 60); do
    host=$(tunnel_host)
    [ -n "$host" ] && break
    sleep 0.5
  done
  [ -n "$host" ] || fail "$QUICK_TUNNEL printed no public host, see $STATE/tunnel.log"
  printf '%s' "$host" >"$origin_file"
  [ -n "$PUBLIC_ORIGIN" ] || PUBLIC_ORIGIN=$host
  log "tunnel → $host"
}

cmd_up() {
  mkdir -p "$STATE"
  cmd_databases
  cmd_tunnel
  cmd_node
  cmd_bootstrap
  cmd_frontend
  cmd_rooms
  cmd_caddy
  cmd_bot
  cmd_ngrok
  if [ -n "$QUICK_TUNNEL" ]; then
    wait_for "tunnel" 60 curl -fsS -o /dev/null "$PUBLIC_ORIGIN/healthz"
  fi
  log "up — $PUBLIC_ORIGIN"
}

cmd_down() {
  for p in ngrok tunnel bot rooms node; do
    if [ -f "$STATE/$p.pid" ]; then kill "$(cat "$STATE/$p.pid")" 2>/dev/null || true; rm -f "$STATE/$p.pid"; fi
  done
  docker rm -f $CADDY_CONTAINER >/dev/null 2>&1 || true
  log "stopped (PostgreSQL/Redis containers left running)"
}

cmd_status() {
  for p in node rooms bot ngrok tunnel; do
    if pid_alive "$p"; then echo "$p: running (pid $(cat "$STATE/$p.pid"))"; else echo "$p: stopped"; fi
  done
  echo "caddy: $(docker inspect -f '{{.State.Status}}' $CADDY_CONTAINER 2>/dev/null || echo stopped)"
  curl -fsS "$API/health" >/dev/null 2>&1 && echo "node /health: ok" || echo "node /health: FAIL"
  curl -fsS "$ROOMS/healthz" 2>/dev/null && echo || echo "rooms /healthz: FAIL"
  curl -fsS "$ROOMS/api/rooms/lounge" 2>/dev/null | jq -c '{participants, bot: (.bot // null | if . then {track: .track.title, kind: .track.kind, positionMs} else null end)}' 2>/dev/null || echo "lounge: unavailable"
  [ -n "$NGROK_DOMAIN" ] && { curl -fsS -o /dev/null "https://$NGROK_DOMAIN/healthz" && echo "public https://$NGROK_DOMAIN: ok" || echo "public: FAIL"; }
  if pid_alive tunnel; then
    local origin; origin=$(tunnel_host)
    [ -n "$origin" ] && printf '%s' "$origin" >"$STATE/public-origin"
    [ -n "$origin" ] || origin=$(cat "$STATE/public-origin" 2>/dev/null || true)
    [ -n "$origin" ] && { curl -fsS -o /dev/null "$origin/healthz" && echo "public $origin: ok" || echo "public $origin: FAIL"; }
  fi
  true
}

cmd_logs() { tail -n "${LINES:-80}" -f "$STATE/${1:?node|rooms|bot|ngrok|tunnel}.log"; }

case "${1:-}" in
  up) cmd_up ;;
  down) cmd_down ;;
  status) cmd_status ;;
  logs) cmd_logs "${2:-}" ;;
  *) sed -n '2,22p' "$0"; exit 1 ;;
esac
