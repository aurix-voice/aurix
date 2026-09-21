# Quick start: two players talking in five minutes

Prerequisites: a Rust toolchain (1.88+), Docker, Node 20+ (for the browser demo), and the
build dependencies `pkg-config`, `libssl-dev`, `cmake` (Debian names; libopus is built from
bundled sources). Everything below runs on one machine, on loopback, in development mode — the
[production path](../operations/deployment.md) adds TLS, a public IP, TURN and real secrets.

## 1. Start PostgreSQL, Redis and a node (≈ 2 min, mostly compile time)

```bash
docker run -d --name aurix-pg -e POSTGRES_USER=aurix -e POSTGRES_PASSWORD=aurix -e POSTGRES_DB=aurix -p 127.0.0.1:5432:5432 postgres:16-alpine
docker run -d --name aurix-redis -p 127.0.0.1:6379:6379 redis:7-alpine

cargo build --locked --bin aurix-server --bin aurix   # the node and the CLI
aurix=target/debug/aurix

mkdir -p ~/.config/aurix && (umask 077; openssl rand -hex 24 > ~/.config/aurix/bootstrap)   # for /admin/setup
AURIX__AUTH__ADMIN_BOOTSTRAP_TOKEN=$(cat ~/.config/aurix/bootstrap) target/debug/aurix-server &
$aurix ready                                           # ready once migrations ran
```

The node listens on `:8080` (REST), `:8081` (WebSocket), `:10000/udp` (media), `:3478` (TURN) and
`:4040` (metrics). `GET /health` answers immediately; `GET /ready` additionally checks PostgreSQL
and Redis. Migrations are embedded and applied at start.

## 2. First administrator and your first app (30 s)

`POST /admin/setup` is open while no administrator exists; afterwards only the bootstrap token
above unlocks it (the CLI always sends it). The CLI never prints a credential: the operator
token is saved into the profile and the app's API key — shown exactly once by the server —
goes straight into a `0600` file.

```bash
$aurix config init --name dev --server http://localhost:8080 \
    --api-key-file ~/.config/aurix/mygame.api-key --set-default   # the file appears in a moment
$aurix admin setup --email root@example.com --display-name Root \
    --bootstrap-token-file ~/.config/aurix/bootstrap             # password from stdin
$aurix admin login --email root@example.com --save                # operator JWT → profile, 0600
$aurix app create --name MyGame --key-out ~/.config/aurix/mygame.api-key
```

## 3. A channel and two player tokens — this is what your game backend does (30 s)

```bash
CH=$($aurix channel create --name lobby --type positional --field id)
ALICE=$($aurix token issue --external-id steam:1 --display-name Alice --channel "$CH" --field token)
BOB=$($aurix token issue --external-id steam:2 --display-name Bob   --channel "$CH" --field token)
```

The API key stays on the machine that ran these commands. Players only ever receive the
short-lived JWT (`$ALICE`, `$BOB`) — the grant inside it says which channels they may join and
whether they may speak and receive (`--channel ID:flags`, default `jsr`). That boundary is the
whole security model of a voice deployment; [Client flow](client-flow.md) and
[Tenancy, credentials and permissions](../concepts/auth.md) spell it out.

## 4. Hear them (1 min)

Browser, no engine needed:

```bash
cd sdk/web && npm ci && npm run demo          # http://localhost:5173, two tabs
```

Paste `$ALICE` in one tab and `$BOB` in the other, enter the channel id, *Connect*, *Join*,
talk. The demo page shows the roster, speaking indicators, chat, device pickers, mic gain and
a mic-test (echo) channel; positional attenuation is driven by `updatePosition()` from your
game code, not by the page. Headless alternative, from the repository root, against the same
node:

```bash
AURIX_E2E_API_KEY=$(cat ~/.config/aurix/mygame.api-key) \
  cargo test -p aurix-client --test e2e_live -- --test-threads=1
```

Then pick the SDK for your engine: [Web](../sdk/web.md), [Unity](../sdk/unity.md) (import the
*Voice quick start* sample and press Play), [Unreal / native C ABI](../sdk/native.md),
[Godot](../sdk/godot.md). Every one of them takes the JWT and the WebSocket URL from step 3 and
nothing else.

## 5. See what happened

```bash
$aurix channel participants "$CH"        # roster with speaking / muted flags
$aurix user session <session_id>         # live MOS, RTT, loss, jitter of one player
$aurix events tail --count 10            # participant.joined / left / quality.alert … (SSE)
curl -s localhost:4040/metrics | grep aurix_active_sessions
```

## Without the CLI: the same flow in `curl`

Everything the CLI does is a call in `api/openapi.json`; the `curl` equivalents are useful when
wiring the same steps into your own backend.

```bash
curl -X POST localhost:8080/admin/setup -H 'content-type: application/json' \
  -d '{"email":"root@example.com","password":"<strong password>","display_name":"Root"}'
ADMIN=$(curl -s -X POST localhost:8080/admin/login -H 'content-type: application/json' \
  -d '{"email":"root@example.com","password":"<strong password>"}' | jq -r .token)

# create an app; the response contains the app's first API key (shown once)
curl -X POST localhost:8080/v1/apps -H "authorization: Bearer $ADMIN" -H 'content-type: application/json' -d '{"name":"MyGame"}'
```

Everything after that is done with the API key (`X-API-Key: aurx_…` or `Authorization: Bearer aurx_…`):

```bash
# channel
curl -X POST localhost:8080/v1/channels -H "x-api-key: $KEY" -H 'content-type: application/json' \
  -d '{"name":"lobby","config":{"channel_type":"positional","max_participants":64}}'

# player token (short-lived JWT, scoped to channels + roles)
curl -X POST localhost:8080/v1/tokens -H "x-api-key: $KEY" -H 'content-type: application/json' \
  -d '{"external_id":"steam:7656119","display_name":"Alice",
       "channels":[{"channel_id":"<channel uuid>","speak":true,"receive":true}]}'

# one-time action token (login | join | kick | mute | unmute), single use, 90 s
curl -X POST localhost:8080/v1/tokens/action -H "x-api-key: $KEY" -H 'content-type: application/json' \
  -d '{"action":"join","external_id":"steam:7656119","channel_id":"<channel uuid>","speak":true}'
# kick/mute/unmute additionally need moderation:write, a target and the acting user:
#   {"action":"kick","user_id":"<moderator uuid>","channel_id":"<channel>","target_user_id":"<player>"}

# ad-hoc channel: no POST /v1/channels needed - the grant names the channel and it is created
# on the first join (and removed when the last participant leaves). The id is derived from the
# name, so the response tells you the channel_id up front and every token for the same name
# lands in the same channel. Works in both /v1/tokens and /v1/tokens/action (join).
curl -X POST localhost:8080/v1/tokens -H "x-api-key: $KEY" -H 'content-type: application/json' \
  -d '{"external_id":"steam:7656119","display_name":"Alice",
       "channels":[{"ad_hoc":{"name":"match-8f3a","channel_type":"team","max_participants":10}}]}'

# channel-wide moderation: everyone currently present except the listed users
curl -X POST localhost:8080/v1/moderation/mute-all -H "x-api-key: $KEY" -H 'content-type: application/json' \
  -d '{"channel_id":"<channel uuid>","muted":true,"except":["<game master uuid>"]}'
curl -X POST localhost:8080/v1/moderation/kick-all -H "x-api-key: $KEY" -H 'content-type: application/json' \
  -d '{"channel_id":"<channel uuid>","reason":"round over"}'
```

## Next

* What happens on the wire: [Client flow](client-flow.md).
* Coming from Vivox, Agora or Photon Voice: [Migration guides](../migration/README.md).
* Production: [Deployment and configuration](../operations/deployment.md) — one node with
  `docker compose`, then [High availability](../operations/high-availability.md) and
  [Scaling out](../operations/scaling.md).
* Ready-made token servers in Node, Python, Go and C#:
  [Server SDKs and token servers](../backend/server-sdks.md).
