# REST API and OpenAPI

The REST API is the control surface for your game backend and for operators. Its contract is an
**OpenAPI 3.1** document:

* in the repository: `api/openapi.json` (the single source of truth, embedded into the binary);
* on every running node: `GET /openapi.json` (no authentication);
* in this book: [browsable reference](reference.html) (Redoc) and [raw file](openapi.json).

Import the file into Postman / Insomnia / Stoplight, or generate a client:

```bash
curl -sO https://voice.example.com/openapi.json
npx @redocly/cli lint openapi.json --skip-rule no-unused-components   # what CI runs
npx @openapitools/openapi-generator-cli generate -i openapi.json -g typescript-fetch -o client/
```

## How the specification is organised

* **Tags** group operations by area: System, Admin, Applications, Tokens, Channels, Users, Chat,
  Speech, Moderation, Analytics, API keys, Audit, Recordings, Live audio streams, Webhooks,
  Events (SSE), Player.
* **Security schemes** — `ApiKeyHeader` (`X-API-Key`), `ApiKeyBearer` (`Authorization: Bearer
  aurx_…`), `PlayerToken`, `AdminToken`, `BootstrapToken` — are attached to each operation. See
  [Tenancy, credentials and permissions](../concepts/auth.md).
* `x-aurix-permissions` on an operation lists the API-key permission(s) it needs;
  `x-aurix-admin-role: superadmin` marks admin operations restricted to super-administrators.
* Every non-2xx response is the `Error` envelope; status-to-code mapping is in the
  specification's introduction and in [Errors](../concepts/auth.md#errors).
* Paginated lists take `page` (from 1) and `per_page` and return `{data, page, per_page, total}`.
* Identifiers are UUIDs, timestamps RFC 3339 UTC. Secrets (API keys, webhook secrets, TURN
  passwords) appear exactly once, in the response that creates them.

## Route map

| Area | Routes | Credential |
| --- | --- | --- |
| Health | `GET /health`, `GET /ready`, `GET /openapi.json` | none |
| Admin | `POST /admin/setup`, `POST /admin/login`, `GET /admin/me`, `POST /admin/admins`, `GET /admin/audit-log`, `POST /admin/retention/sweep` | admin JWT (`superadmin` where marked), bootstrap token for setup |
| Applications & nodes | `GET/POST /v1/apps`, `GET/DELETE /v1/apps/{app_id}`, `POST /v1/apps/{app_id}/rotate-key`, `GET /v1/nodes` | admin JWT |
| Tokens & TURN | `POST /v1/tokens`, `POST /v1/tokens/action`, `POST /v1/turn/credentials` | API key |
| Channels | `GET/POST /v1/channels`, `GET/DELETE /v1/channels/{id}`, `PUT /v1/channels/{id}/config`, `GET /v1/channels/{id}/participants`, `POST /v1/channels/{id}/tts`, `GET /v1/tts/voices` | API key |
| Chat | `GET/POST /v1/channels/{id}/messages`, `GET/POST /v1/users/{id}/messages` | API key |
| Users | `GET /v1/users`, `GET/DELETE /v1/users/{id}`, `GET /v1/users/{id}/export`, `POST /v1/users/{id}/unban`, `GET/POST /v1/users/{id}/blocks`, `DELETE /v1/users/{id}/blocks/{blocked_user_id}` | API key |
| Sessions | `GET /v1/sessions/{id}/stats` (node-local) | API key |
| Moderation | `POST /v1/moderation/{ban,mute,kick,mute-all,kick-all,report}`, `GET /v1/moderation/bans`, `POST …/bans/{id}/revoke`, `GET /v1/moderation/events`, `GET …/events/{id}`, `POST …/events/{id}/resolve` | API key |
| Recordings | `POST /v1/recordings/start`, `GET /v1/recordings`, `GET/DELETE /v1/recordings/{id}`, `POST …/stop`, `GET …/download` | API key |
| Live audio streams | `GET /v1/audio/streams`, `GET/POST /v1/channels/{id}/audio/streams`, `GET …/streams/pull` (WebSocket upgrade), `GET/DELETE …/streams/{stream_id}` | API key |
| Webhooks | `GET/POST /v1/webhooks`, `GET /v1/webhooks/events`, `GET/PATCH/DELETE /v1/webhooks/{id}`, `POST …/rotate-secret`, `POST …/test`, `POST …/resync`, `GET …/deliveries`, `GET …/deliveries/{id}`, `POST …/deliveries/{id}/retry` | API key |
| Events | `GET /v1/events` (SSE), `GET /v1/events/snapshot` | API key |
| Analytics, keys, audit | `GET /v1/analytics`, `GET/POST /v1/api-keys`, `DELETE /v1/api-keys/{id}`, `GET /v1/audit-log` | API key |
| Player | `GET /v1/me/turn-credentials`, `POST /v1/me/reports`, `POST /v1/me/recordings/{id}/consent`, `POST /v1/webrtc/offer` | player JWT |

The exact paths, parameters and schemas are in the specification; the table only orients you.
The contract test `crates/aurix-api/tests/openapi_contract.rs` fails the build if a router route
is missing from the specification or vice versa, if a documented permission is unknown to the
server, or if the versions drift.

## Conventions worth knowing

* **Node-local resources.** Session statistics and live audio streams live on the node hosting
  the session/channel. Behind a load balancer address that node directly (its address is part of
  the user's session information) or expect `404`/`409 CONFLICT` from other nodes.
* **Rate limits.** `429` carries `Retry-After`. Limits are per API key and per IP.
* **Idempotency.** `DELETE` on a missing resource is `404`; moderation actions on a user who is
  already in the requested state succeed without a second event.
* **Downloads.** `GET /v1/recordings/{id}` returns `download_url` only for S3 storage (15-minute
  pre-signed); with local storage stream the file through `GET /v1/recordings/{id}/download`.
* **CORS.** Allowed origins come from `server.cors_origins`; wildcard is refused in production.

## Regenerating the specification

The specification is maintained as an artefact in the repository. When you add or change a
route: update `api/openapi.json` (paths, schemas, `x-aurix-permissions`), run
`cargo test -p aurix-api --test openapi_contract`, `npx @redocly/cli lint api/openapi.json
--skip-rule no-unused-components`, and rebuild the book. CI performs the same checks.
