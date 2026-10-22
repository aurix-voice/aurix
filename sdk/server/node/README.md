# @aurix/server-sdk

Server-side (game backend) SDK for the [Aurix](https://github.com/aurix-voice/aurix) voice platform
REST API: token issuance, channels, users, moderation, analytics, webhooks and the SSE event stream.
TypeScript, ES2020, no runtime dependencies (uses the global `fetch`; Node 18+). Generated from
`api/openapi.json`, so every endpoint is typed.

```bash
npm install @aurix/server-sdk
```

## Issue a voice token (the one call every game needs)

```ts
import { AurixClient } from "@aurix/server-sdk";

const aurix = new AurixClient({ baseUrl: process.env.AURIX_URL!, apiKey: process.env.AURIX_API_KEY! });

const res = await aurix.issueToken({
  external_id: player.id,                       // your stable account id (Steam ID, account UUID…)
  display_name: player.name,
  channels: [{ ad_hoc: { name: `match-${matchId}`, channel_type: "team" }, join: true, speak: true, receive: true }],
  region: "eu_west",                            // optional hint; the server picks the nearest healthy node
});
reply({ token: res.token, endpoint: res.endpoint });   // never the API key, never the raw response
```

The client SDK (Unity / Unreal / Godot / Web / native) connects with `token` to `endpoint.ws_url`.

## Webhooks and events

```ts
import { parseWebhook, eventStream } from "@aurix/server-sdk";

// Express/Fastify handler: verifies the HMAC signature + timestamp, then returns the typed event
const event = parseWebhook(process.env.AURIX_WEBHOOK_SECRET!, req.headers, rawBody);

// or subscribe to the live SSE stream (participant joins/leaves, moderation, quality alerts)
for await (const ev of eventStream(aurix, { types: ["participant.joined", "participant.left"] })) {
  console.log(ev.type, ev.data);
}
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
