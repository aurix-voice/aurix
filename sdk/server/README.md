# Aurix server SDKs (game backend)

Typed clients for the Aurix control-plane REST API — the calls a **game backend** makes:
token issuance, channels, users, moderation, recordings, analytics, webhooks and the SSE event
stream. Nothing here runs on a player's device; the API key these clients carry must never
leave your servers (see [Token servers](examples/token-server/README.md)).

| Language | Package / module | Path | Runtime | Transport |
|---|---|---|---|---|
| Node / TypeScript | `@aurix/server-sdk` | [`node`](node) | Node ≥ 18 (global `fetch`) | `fetch`, no dependencies |
| Python | `aurix-server-sdk` → `aurix_server` | [`python`](python) | Python ≥ 3.9, `py.typed` | `urllib` (sync), `asyncio` wrapper (`AsyncAurixClient`), no dependencies |
| Go | `github.com/aurix-voice/aurix/sdk/server/go` (package `aurix`) | [`go`](go) | Go ≥ 1.22 | `net/http`, no dependencies |
| C# | `Aurix.Server` | [`csharp`](csharp) | .NET 8 | `HttpClient`, `System.Text.Json`, no dependencies |

The packages are **not published** to npm / PyPI / pkg.go.dev / NuGet by this repository; use
them from a path dependency (`file:`, `replace`, `ProjectReference`, `pip install -e`) or publish
them under your own name — the metadata (`package.json`, `pyproject.toml`, `go.mod`, `.csproj`)
is complete.

## What is generated and what is not

`python3 tools/openapi-sdk/generate.py` reads [`api/openapi.json`](../../api/openapi.json) and
writes **only** the `generated/` parts (`*_gen.go` for Go):

* one type per schema (records / `TypedDict`s / structs), `x-…` enums as constants;
* one method per operation, named after its `operationId` (`issueToken`, `issue_token`,
  `IssueToken`, `IssueTokenAsync`), with path/query parameters and the typed request body;
* the schema descriptions as doc comments.

Hand-written and stable across regenerations: HTTP transport (timeouts, retries with
`Retry-After` — idempotent methods on network errors / 502-504, every method on 429), error
mapping (`AurixError` / `AurixException` / `*aurix.Error` with `status`, `code`, `message`,
`requestId`, `retryAfter`), credentials (`apiKey` → `X-API-Key`, `adminToken` /
`playerToken` → `Authorization: Bearer`, `bootstrapToken` → `X-Bootstrap-Token`), webhook
signature verification, SSE consumption.

`generate.py --check` fails when the committed output is stale; CI runs it, so a change to the
contract without regenerating does not pass. Do not edit generated files by hand.

## Issue a token (the one call every backend makes)

```ts
import { AurixClient } from "@aurix/server-sdk";
const aurix = new AurixClient({ baseUrl: process.env.AURIX_URL!, apiKey: process.env.AURIX_API_KEY! });
const res = await aurix.issueToken({
  external_id: player.id, display_name: player.name,
  channels: [{ ad_hoc: { name: `match-${matchId}`, channel_type: "team" }, join: true, speak: true, receive: true }],
  region: "eu_west",
});
// hand res.token and res.endpoint?.ws_url to the game client — nothing else
```

```python
from aurix_server import AurixClient
aurix = AurixClient(os.environ["AURIX_URL"], api_key=os.environ["AURIX_API_KEY"])
res = aurix.issue_token({"external_id": player.id, "display_name": player.name,
                         "channels": [{"channel_id": channel_id, "join": True, "speak": True, "receive": True}]})
```

```go
client, _ := aurix.New(aurix.Options{BaseURL: os.Getenv("AURIX_URL"), Credentials: aurix.Credentials{APIKey: os.Getenv("AURIX_API_KEY")}})
res, err := client.IssueToken(ctx, aurix.GenerateTokenRequest{ExternalID: player.ID, DisplayName: player.Name})
```

```csharp
var aurix = new AurixClient(new AurixClientOptions { BaseUrl = url, Credentials = new Credentials { ApiKey = apiKey } });
var res = await aurix.IssueTokenAsync(new GenerateTokenRequest { ExternalId = player.Id, DisplayName = player.Name });
```

Complete, tested backends around this call: [`examples/token-server`](examples/token-server).

## Webhooks

Every delivery carries `X-Aurix-Signature: t=<unix>,v1=<hex HMAC-SHA256(secret, "<t>.<raw body>")>`
plus `X-Aurix-Event`, `X-Aurix-Webhook-Id`, `X-Aurix-Delivery-Id`, `X-Aurix-Attempt`. Verify
against the **raw** body bytes before parsing:

| | verify + parse | sign (fixtures / tests) |
|---|---|---|
| Node | `parseWebhook(secret, headers, rawBody)` → `IncomingWebhook` (throws `WebhookVerificationError`), `verifyWebhookSignature` | `signWebhook` |
| Python | `parse_webhook(secret, headers, raw_body)` / `verify_webhook_signature` | `sign_webhook` |
| Go | `aurix.ParseWebhook(secret, r.Header, body, nil)` / `VerifyWebhookSignature` | `SignWebhook` |
| C# | `Webhooks.Parse(secret, headers, rawBody)` (throws `WebhookSignatureException`) / `Webhooks.Verify` | `Webhooks.Sign` |

Comparison is constant-time, the timestamp must be within the tolerance (default 5 minutes),
and all four implementations plus the `aurix webhook verify` / `sign` CLI commands agree on
[`vectors/webhook_signature.json`](vectors/webhook_signature.json).

## Event stream (SSE)

`GET /v1/events` as a typed iterator with automatic reconnect (`Last-Event-ID`) and a `types`
filter: `eventStream(client, { types: [...] })` (Node), `event_stream(client, types=[...])`
(Python), `client.Events(ctx, opts, fn)` (Go), `client.EventsAsync(options)` (C#). A `lagged`
event means the server dropped events for this consumer — fetch `GET /v1/events/snapshot` to
resynchronise.

## Tests

Each SDK is tested against an in-process fake node (auth headers, path/query encoding, error
envelope, retries, `Retry-After`, SSE reconnect, webhook vectors); the examples are tested the
same way. Live checks against a running node are done through the `aurix` CLI
(`crates/aurix-cli`), which embeds the same contract.

```sh
python3 tools/openapi-sdk/generate.py --check
(cd sdk/server/node && npm ci && npm run check && npm test)
(cd sdk/server/python && python3 -m mypy aurix_server tests && python3 -m pytest -q && python3 -m ruff check .)
(cd sdk/server/go && gofmt -l . && go vet ./... && go test ./...)
(cd sdk/server/csharp && dotnet test -c Release)
```

## Limitations

* Generated from the contract, so **everything the contract does not say is not typed**:
  webhook / SSE payloads are `EventEnvelope { type, data: object }` — narrow `data` yourself.
* No pagination helpers beyond the cursors the API returns (`next_before` / `next_after`).
* Player-facing endpoints (`/v1/me/*`) are generated too but need a player token; the server
  SDKs are not meant to impersonate players.
* Not published to package registries; no Java / PHP / Ruby clients — the generator's IR
  (`tools/openapi-sdk/ir.py`) is the place to add one.
