# Token server examples (Node, Python, Go, C#)

The one backend service every Aurix integration needs: the game client asks **your** server
for a voice token, your server calls `POST /v1/tokens` with the application API key, and the
client only ever receives the player token plus the endpoint to connect to.

```text
game client ──(your game session)──▶ POST /voice/token ──▶ token server ──(X-API-Key)──▶ Aurix POST /v1/tokens
game client ◀── { token, user_id, expires_at, endpoint } ◀────────────────────────────────┘
```

Four implementations with the same HTTP surface, the same security boundary and the same test
plan, each using the server SDK of its language ([`sdk/server`](../../README.md)):

| | Entry point | Framework | Tests |
|---|---|---|---|
| [`node`](node) | `token_server.mjs` | `node:http` (no framework) | `npm test` (`node --test`) |
| [`python`](python) | `token_server.py` | `http.server` (stdlib) | `pytest`, `mypy --strict`, `ruff` |
| [`go`](go) | `main.go` | `net/http` | `go test` |
| [`csharp`](csharp) | `TokenServer/Program.cs`, `TokenService.cs` | ASP.NET Core minimal API (.NET 8) | `dotnet test` (`WebApplicationFactory`) |

## HTTP surface

| Route | Auth | Purpose |
|---|---|---|
| `GET /healthz` | none | liveness |
| `POST /dev/login` `{player_id, display_name}` → `{session}` | none, **only with `ALLOW_DEV_LOGIN=1`** | stand-in for your game login; mints an HMAC-signed development session |
| `POST /voice/token` `{match_id}` → `{token, user_id, expires_at, endpoint}` | `Authorization: Bearer <game session>` | issues the Aurix player token for the authenticated player, granted to the ad-hoc team channel `match-<match_id>` |

`match_id` must match `[A-Za-z0-9_-]{1,64}`; bodies over 4 KiB are rejected. Upstream failures
are mapped to a generic `502 voice service unavailable` (`503` when Aurix rate-limits or is
unreachable); the Aurix error body, request id and code go to the server log only.

## Configuration (environment, backend only)

| Variable | Meaning |
|---|---|
| `AURIX_URL` | node HTTP origin (default `http://localhost:8080`) |
| `AURIX_API_KEY_FILE` / `AURIX_API_KEY` | application API key with `tokens:issue` — a file (mode 0600) is preferred over an inline variable; **never ship this to a client build** |
| `GAME_SESSION_SECRET` | ≥ 32 characters; signs the development sessions. Irrelevant once you plug in your own auth |
| `AURIX_REGION` | optional region hint passed to `POST /v1/tokens` (`us_east`, `eu_west`, …) |
| `PORT` | listen port (default `3000`; C#: `ASPNETCORE_URLS`) |
| `ALLOW_DEV_LOGIN` | `1` enables `/dev/login`. Leave unset in production |

Startup refuses to run without an API key, with a short session secret or with an unknown
region.

## What the examples get right (and what you must keep when adapting them)

1. **The API key never leaves the backend.** It is read from the environment, sent only to
   Aurix as `X-API-Key`, and never written to the response, the client payload or the logs.
   The tests assert this by capturing the upstream request and the log output.
2. **Identity comes from your authentication, not from the request.** `external_id` and
   `display_name` are taken from the verified game session; anything the client puts in the
   body is ignored. Replace `authenticatePlayer` / `authenticate_player` / `AuthenticatePlayer`
   / `GameSession.Authenticate` with your real session or JWT validation — that function is the
   only place to change.
3. **Grants are decided by the backend.** The player receives `join + speak + receive` for the
   team channel of the match they asked for; `playerMayJoin` is where a real backend checks
   that the player is actually in that match (party, guild, raid, region, ban status).
4. **The client receives an allowlist**, not the upstream response: `token`, `user_id`,
   `expires_at`, `endpoint {ws_url, region}` (or `null`). New fields in `TokenResponse` do not
   leak by accident.
5. **Errors are generic towards the client**, detailed towards the operator.

## Run

```sh
# Node
cd node && npm ci
AURIX_API_KEY_FILE=~/.aurix-dev/api-key GAME_SESSION_SECRET=$(openssl rand -hex 32) ALLOW_DEV_LOGIN=1 npm start

# Python (the SDK is a path dependency: put it on PYTHONPATH or `pip install ../../../python`)
cd python && PYTHONPATH=../../../python \
AURIX_API_KEY_FILE=~/.aurix-dev/api-key GAME_SESSION_SECRET=$(openssl rand -hex 32) ALLOW_DEV_LOGIN=1 python3 token_server.py

# Go
cd go && AURIX_API_KEY_FILE=~/.aurix-dev/api-key GAME_SESSION_SECRET=$(openssl rand -hex 32) ALLOW_DEV_LOGIN=1 go run .

# C#
cd csharp && AURIX_API_KEY_FILE=~/.aurix-dev/api-key GAME_SESSION_SECRET=$(openssl rand -hex 32) ALLOW_DEV_LOGIN=1 \
ASPNETCORE_URLS=http://127.0.0.1:3000 dotnet run --project TokenServer
```

Then, as the "game client":

```sh
SESSION=$(curl -s localhost:3000/dev/login -d '{"player_id":"p1","display_name":"Alice"}' | jq -r .session)
curl -s localhost:3000/voice/token -H "Authorization: Bearer $SESSION" -d '{"match_id":"m-42"}'
# → {"token":"…","user_id":"…","expires_at":"…","endpoint":{"region":"…","ws_url":"wss://…"}}
```

The returned `token` goes straight into any client SDK (`connect(token)`), `endpoint.ws_url`
is the node to connect to (or `null` when no node advertises a public URL yet — fall back to
your configured signalling URL).

## Tests

```sh
(cd node && npm ci && npm test)
(cd python && python3 -m pytest -q && PYTHONPATH=../../../python python3 -m mypy . && python3 -m ruff check .)
(cd go && gofmt -l . && go vet ./... && go test ./...)
(cd csharp && dotnet test -c Release)
```

Every suite starts a fake Aurix node in-process and checks: unsafe configuration is refused;
forged and expired sessions are rejected before any upstream call; the upstream request carries
the API key, the session's identity and the expected grants; the client response is exactly the
allowlist; spoofed body fields are ignored; bad `match_id` is refused; upstream errors become
generic client errors; nothing in the logs contains the API key.

## Not covered

These are examples, not a product: no TLS termination, no rate limiting towards the game
client, no metrics, no real login. Put them behind your existing API gateway or copy the
`/voice/token` handler into your backend.
