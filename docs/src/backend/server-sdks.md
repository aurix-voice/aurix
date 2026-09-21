# Server SDKs and token servers

Aurix is a game-voice stack, so the integration point that matters is not the player's device
but **your game backend**: it decides who may speak where, mints the player tokens, reacts to
moderation events and reads usage. This chapter covers the typed server SDKs for that backend
(Node, Python, Go, C#), the reference token servers built on them, and the boundary they all
enforce — the application API key never leaves the server side.

```text
game client ──(your game session)──▶ your backend ──(X-API-Key)──▶ Aurix  POST /v1/tokens
game client ◀── { token, endpoint } ◀─────────────┘
game client ──(player token)──────▶ Aurix WebSocket + media  (any client SDK)
```

## The SDKs

| Language | Package | Where | Runtime |
|---|---|---|---|
| Node / TypeScript | `@aurix/server-sdk` | `sdk/server/node` | Node ≥ 18, `fetch`, zero dependencies |
| Python | `aurix-server-sdk` (`import aurix_server`) | `sdk/server/python` | Python ≥ 3.9, stdlib only, `py.typed`; sync `AurixClient` and `AsyncAurixClient` |
| Go | `github.com/aurix-voice/aurix/sdk/server/go` (`package aurix`) | `sdk/server/go` | Go ≥ 1.22, `net/http` only |
| C# | `Aurix.Server` | `sdk/server/csharp` | .NET 8, `HttpClient` + `System.Text.Json` |

All four are produced by the same generator from the single contract,
[`api/openapi.json`](../api/rest.md):

* **Generated** (`generated/`, `*_gen.go`): one type per schema, one method per operation
  named after its `operationId` — `issueToken` / `issue_token` / `IssueToken` /
  `IssueTokenAsync`, `listChannels`, `banUser`, `getAnalytics`, … — with typed path/query
  parameters and request bodies, enums as constants, schema descriptions as doc comments.
  `python3 tools/openapi-sdk/generate.py --check` fails in CI when these files are stale, so
  the SDKs cannot drift from the specification.
* **Hand-written** and stable: the HTTP transport (timeouts; retries with `Retry-After` on
  429 for every method and on network errors / 502–504 for idempotent ones), the error type
  (`AurixError` / `AurixException` / `*aurix.Error`: `status`, `code`, `message`,
  `requestId`, `retryAfter`, `isRateLimited`), credentials (`apiKey` → `X-API-Key`,
  `adminToken` / `playerToken` → `Authorization: Bearer`, `bootstrapToken` →
  `X-Bootstrap-Token`), [webhook signature verification](../api/webhooks-sse.md) and the SSE
  iterator.

The packages are complete (`package.json`, `pyproject.toml`, `go.mod`, `.csproj`) but **not
published** to npm / PyPI / NuGet / pkg.go.dev from this repository — consume them as path
dependencies or publish them under your organisation's name.

### Issuing a token

```ts
import { AurixClient } from "@aurix/server-sdk";

const aurix = new AurixClient({ baseUrl: process.env.AURIX_URL!, apiKey: process.env.AURIX_API_KEY! });
const res = await aurix.issueToken({
  external_id: player.id,                       // your stable account id
  display_name: player.name,
  channels: [{ ad_hoc: { name: `match-${matchId}`, channel_type: "team" }, join: true, speak: true, receive: true }],
  region: "eu_west",                            // optional hint; wins when the region has capacity
});
reply({ token: res.token, endpoint: res.endpoint });   // never the API key, never the raw response
```

```python
from aurix_server import AurixClient

aurix = AurixClient(os.environ["AURIX_URL"], api_key=os.environ["AURIX_API_KEY"])
res = aurix.issue_token({"external_id": player.id, "display_name": player.name,
                         "channels": [{"channel_id": channel_id, "join": True, "speak": True, "receive": True}]})
```

```go
client, err := aurix.New(aurix.Options{BaseURL: url, Credentials: aurix.Credentials{APIKey: apiKey}})
res, err := client.IssueToken(ctx, aurix.GenerateTokenRequest{ExternalID: player.ID, DisplayName: player.Name})
```

```csharp
var aurix = new AurixClient(new AurixClientOptions { BaseUrl = url, Credentials = new Credentials { ApiKey = apiKey } });
var res = await aurix.IssueTokenAsync(new GenerateTokenRequest { ExternalId = player.Id, DisplayName = player.Name });
```

`TokenResponse.endpoint` is `null` until at least one node advertises a public `ws_url`; fall
back to your configured signalling URL in that case. Grants are the backend's decision: a
player token lists exactly the channels (or ad-hoc channels created on first join) the player
may use, with `join` / `speak` / `receive` / `moderate` / `priority` flags — see
[Tenancy, credentials and permissions](../concepts/auth.md).

### Webhooks and the event stream

Verification helpers take the **raw** body bytes and the `X-Aurix-Signature` header
(`t=<unix>,v1=<hex HMAC-SHA256(secret, "<t>.<body>")>`), compare in constant time and reject
timestamps outside the tolerance (default 5 minutes):

| | verify + parse | sign (fixtures) |
|---|---|---|
| Node | `parseWebhook(secret, headers, rawBody)` → `IncomingWebhook`; `verifyWebhookSignature` | `signWebhook` |
| Python | `parse_webhook` / `verify_webhook_signature` | `sign_webhook` |
| Go | `aurix.ParseWebhook(secret, r.Header, body, nil)` / `VerifyWebhookSignature` | `SignWebhook` |
| C# | `Webhooks.Parse` / `Webhooks.Verify` | `Webhooks.Sign` |

The four implementations and the CLI's `aurix webhook verify` / `sign` agree on the shared
vectors in `sdk/server/vectors/webhook_signature.json`.

`GET /v1/events` is exposed as an iterator with automatic reconnect (`Last-Event-ID`) and a
`types` filter — `eventStream(client, {types})`, `event_stream(client, types=…)`,
`client.Events(ctx, opts, fn)`, `client.EventsAsync(options)`. On a `lagged` event fetch
`GET /v1/events/snapshot` and rebuild local state ([Webhooks and the event stream](../api/webhooks-sse.md)).

## Token servers

`sdk/server/examples/token-server/{node,python,go,csharp}` are complete, tested backends around
the call above — the same HTTP surface in every language:

| Route | Auth | Purpose |
|---|---|---|
| `GET /healthz` | none | liveness |
| `POST /dev/login` `{player_id, display_name}` → `{session}` | none — only with `ALLOW_DEV_LOGIN=1` | **development stand-in for your game login**; mints an HMAC-signed session |
| `POST /voice/token` `{match_id}` → `{token, user_id, expires_at, endpoint}` | `Authorization: Bearer <game session>` | Aurix token for the authenticated player, granted to the ad-hoc team channel `match-<match_id>` |

What they enforce, and what to keep when you copy the handler into your backend:

1. **The API key stays on the backend** — read from `AURIX_API_KEY_FILE` (mode 0600, preferred)
   or `AURIX_API_KEY`, sent only to Aurix, never written to the response or to the log. The
   tests capture the upstream request and the log output to prove it.
2. **Identity comes from your authentication.** `external_id` and `display_name` are taken from
   the verified game session; body fields with the same names are ignored. The single function
   to replace is `authenticatePlayer` (Node / Go), `authenticate_player` (Python),
   `GameSession.Authenticate` (C#).
3. **Grants are decided server-side.** `playerMayJoin` is where a real backend checks that the
   player belongs to that match (party, guild, raid, region, bans) before granting the channel.
4. **The client gets an allowlist**: `token`, `user_id`, `expires_at`, `endpoint {ws_url,
   region}` — new fields of `TokenResponse` do not leak by accident.
5. **Errors are generic towards the client** (`502` / `503 voice service unavailable`) and
   detailed in the server log (status, code, request id).

Configuration is environment-only (`AURIX_URL`, `AURIX_API_KEY[_FILE]`, `GAME_SESSION_SECRET`
≥ 32 chars, optional `AURIX_REGION`, `PORT`, `ALLOW_DEV_LOGIN`); startup refuses unsafe values.
Run instructions and the test plan are in `sdk/server/examples/token-server/README.md`.

```sh
SESSION=$(curl -s localhost:3000/dev/login -d '{"player_id":"p1","display_name":"Alice"}' | jq -r .session)
curl -s localhost:3000/voice/token -H "Authorization: Bearer $SESSION" -d '{"match_id":"m-42"}'
# {"token":"…","user_id":"…","expires_at":"…","endpoint":{"ws_url":"wss://…","region":"eu_west"}}
```

The token goes straight into a client SDK's `connect()`; `endpoint.ws_url` is the node to dial.

## Limitations

* Webhook and SSE payloads are typed only as far as the contract types them:
  `EventEnvelope { type, data }` with `data` an object — narrow it per `type` yourself.
* No client-side pagination helpers beyond the cursors the API returns.
* Player endpoints (`/v1/me/*`) are generated but require a player token; the server SDKs are
  not meant to act on a player's behalf.
* Not published to registries; no Java / PHP / Ruby / Rust clients — a new language is a new
  emitter over `tools/openapi-sdk/ir.py` (the Rust server itself has no need for one).
* The token servers are examples: no TLS termination, no client-facing rate limit, no
  metrics, and `/dev/login` is not an authentication system.
