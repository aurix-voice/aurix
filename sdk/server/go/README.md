# aurix (Go server SDK)

Server-side (game backend) SDK for the [Aurix](https://github.com/aurix-voice/aurix) voice platform
REST API: token issuance, channels, users, moderation, analytics, webhooks and the SSE event stream.
Standard library only, every endpoint typed from `api/openapi.json`.

```bash
go get github.com/aurix-voice/aurix/sdk/server/go@latest
```

## Issue a voice token

```go
import aurix "github.com/aurix-voice/aurix/sdk/server/go"

client, err := aurix.New(aurix.Options{
    BaseURL:     os.Getenv("AURIX_URL"),
    Credentials: aurix.Credentials{APIKey: os.Getenv("AURIX_API_KEY")},
})

res, err := client.IssueToken(ctx, aurix.GenerateTokenRequest{
    ExternalID:  player.ID,              // your stable account id
    DisplayName: player.Name,
    Channels: []aurix.ChannelGrant{{
        AdHoc: &aurix.AdHocChannel{Name: "match-" + matchID, ChannelType: "team"},
        Join: true, Speak: true, Receive: true,
    }},
    Region: "eu_west",                   // optional hint
})
// reply with res.Token and res.Endpoint — never the API key
```

Options: `HTTPClient` (15 s timeout by default), `MaxRetries` (2), `MaxBackoff` (5 s), `Headers`,
`UserAgent`. Webhook verification (`aurix.VerifyWebhookSignature(secret, header, body, nil)`) and the SSE
stream (`client.Events(ctx, aurix.EventStreamOptions{Types: ...}, func(ev aurix.SSEEvent) error { ... })`)
are in the same package.

## Credentials and errors

The client takes one credential: an **API key** of the application (`tokens:issue`, `channels:*`,
`moderation:*`… scopes as configured on the key), or an admin / bootstrap token for console-grade
calls. Keep it on your backend — players only ever receive the short-lived voice token.

Every method maps 1:1 to the REST API (`api/openapi.json`); `4xx/5xx` become a typed error with
`status`, `code` and the request id, and idempotent calls are retried on network errors,
`429` (honouring `Retry-After`) and `502/503/504`.

## Docs

- Server SDKs and token servers: https://github.com/aurix-voice/aurix/blob/main/docs/src/backend/server-sdks.md
- Token-server examples (Node / Python / Go / C#): https://github.com/aurix-voice/aurix/tree/main/sdk/server/examples/token-server
- REST API reference: https://github.com/aurix-voice/aurix/blob/main/api/openapi.json
- Changelog: https://github.com/aurix-voice/aurix/blob/main/CHANGELOG.md

Apache-2.0 — part of the open-source [Aurix voice platform](https://github.com/aurix-voice/aurix).
