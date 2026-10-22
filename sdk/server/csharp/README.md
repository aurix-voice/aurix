# Aurix.Server

Server-side (game backend) SDK for the [Aurix](https://github.com/aurix-voice/aurix) voice platform
REST API: token issuance, channels, users, moderation, analytics, webhooks and the SSE event stream.
.NET 8, `HttpClient`-based, no third-party dependencies, every endpoint typed from `api/openapi.json`.

```bash
dotnet add package Aurix.Server
```

## Issue a voice token

```csharp
using Aurix.Server;

var aurix = new AurixClient(new AurixClientOptions
{
    BaseUrl = Environment.GetEnvironmentVariable("AURIX_URL")!,
    Credentials = new Credentials { ApiKey = Environment.GetEnvironmentVariable("AURIX_API_KEY") },
});

var res = await aurix.IssueTokenAsync(new GenerateTokenRequest
{
    ExternalId = player.Id,               // your stable account id
    DisplayName = player.Name,
    Channels = new[] { new ChannelGrant { AdHoc = new AdHocChannel { Name = $"match-{matchId}", ChannelType = "team" }, Join = true, Speak = true, Receive = true } },
    Region = "eu_west",                   // optional hint
});
return new { res.Token, res.Endpoint };    // never the API key
```

Options: `Timeout` (15 s), `MaxRetries` (2), `MaxBackoff` (5 s), `UserAgent`, extra headers; pass a
`CancellationToken` to any call.

## Webhooks and events

```csharp
var evt = Webhooks.Parse(secret, headers, rawBody);                        // HMAC + timestamp check, typed event
await foreach (var ev in aurix.EventsAsync(new EventStreamOptions { Types = new[] { "participant.joined" } }, ct))
    Console.WriteLine($"{ev.Type} {ev.Data}");
```

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
