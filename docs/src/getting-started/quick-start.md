# Quick start (development)

Prerequisites: a Rust toolchain (1.88+), Docker, and the build dependencies `pkg-config`,
`libssl-dev`, `cmake` (Debian names; libopus is built from bundled sources).

## 1. Start the dependencies and the server

```bash
docker run -d --name aurix-pg -e POSTGRES_USER=aurix -e POSTGRES_PASSWORD=aurix -e POSTGRES_DB=aurix -p 127.0.0.1:5432:5432 postgres:16-alpine
docker run -d --name aurix-redis -p 127.0.0.1:6379:6379 redis:7-alpine

cargo run --bin aurix-server                # uses configs/default.toml (development mode)
```

The node listens on `:8080` (REST), `:8081` (WebSocket), `:10000/udp` (media), `:3478` (TURN) and
`:4040` (metrics). `GET /health` answers immediately; `GET /ready` additionally checks PostgreSQL
and Redis. Migrations are embedded and applied at start.

## 2. Bootstrap the first administrator

`POST /admin/setup` is allowed only while no admin exists, or with the configured
`AURIX__AUTH__ADMIN_BOOTSTRAP_TOKEN` sent as `X-Bootstrap-Token`:

```bash
curl -X POST localhost:8080/admin/setup -H 'content-type: application/json' \
  -d '{"email":"root@example.com","password":"<strong password>","display_name":"Root"}'
ADMIN=$(curl -s -X POST localhost:8080/admin/login -H 'content-type: application/json' \
  -d '{"email":"root@example.com","password":"<strong password>"}' | jq -r .token)

# create an app; the response contains the app's first API key (shown once)
curl -X POST localhost:8080/v1/apps -H "authorization: Bearer $ADMIN" -H 'content-type: application/json' -d '{"name":"MyGame"}'
```

## 3. Use the app's API key from your game backend

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

## 4. Connect a client

Hand the player JWT to the client and pick an SDK:

* Browser: [Web SDK](../sdk/web.md) — `npm run demo` in `sdk/web` opens a two-tab demo page.
* Unity: [Unity SDK](../sdk/unity.md) — import the *Voice quick start* sample and press Play.
* Unreal / custom engine: [Native core and Unreal SDK](../sdk/native.md).
* Headless: `dotnet run --project sdk/unity/DotNet~/Aurix.Demo` or
  `cargo test -p aurix-client --test e2e_live` drive two clients against a running node.

What happens on the wire is described in [Client flow](client-flow.md). For a production
deployment continue with [Deployment and configuration](../operations/deployment.md).
