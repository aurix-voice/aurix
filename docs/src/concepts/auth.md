# Tenancy, credentials and permissions

## Tenancy

An **application** (`POST /v1/apps`, admin only) is the tenant. Users, channels, sessions,
recordings, bans, blocks, webhooks, API keys, analytics and audit entries belong to exactly one
app. Every REST call and every WebSocket session is scoped to the app of the credential that
authenticated it; nothing in a request body can change that scope, and a resource of another app
is reported as `404 NOT_FOUND`.

## Credentials

| Credential | How it is sent | Who holds it | Used for |
| --- | --- | --- | --- |
| **API key** `aurx_…` | `X-API-Key: aurx_…` or `Authorization: Bearer aurx_…` | your game backend / game server | everything under `/v1/*` except the player routes |
| **Player JWT** | `Authorization: Bearer <jwt>` (REST), `bearer.<jwt>` sub-protocol or header (WebSocket) | the game client | opening a session, `/v1/me/*`, `/v1/webrtc/offer` |
| **Action token** | same places as the player JWT | the game client, one action | `login`, `join`, `kick`, `mute`, `unmute` — single use |
| **Admin JWT** | `Authorization: Bearer <jwt>` from `POST /admin/login` or the SSO callback | operators | `/admin/*`, `/v1/apps*`, `/v1/nodes` |
| **Bootstrap token** | `X-Bootstrap-Token` | the operator installing the system | `POST /admin/setup` after the first admin exists |

The OpenAPI document declares these as the security schemes `ApiKeyHeader`, `ApiKeyBearer`,
`PlayerToken`, `AdminToken` and `BootstrapToken`.

## Permissions

An API key carries a list of permissions. `*` grants everything; a key can only mint keys
(`POST /v1/api-keys`, `keys:manage`) with a subset of its own permissions. Each key also has its
own request budget (`rate_limit`, requests per minute across the whole fleet; `0` = unlimited),
enforced on top of the per-IP limit — see
[Fleet-wide rate limits](../operations/scaling.md#fleet-wide-rate-limits).

| Permission | Grants |
| --- | --- |
| `tokens:issue` | `POST /v1/tokens`, `POST /v1/tokens/action` |
| `turn:issue` | `POST /v1/turn/credentials` |
| `channels:read` / `channels:write` | channel listing, participants, session stats, TTS voices — create, configure, delete |
| `users:read` / `users:write` | user search, profiles, block lists — add / remove blocks |
| `users:erase` / `users:export` | `DELETE /v1/users/:id` — `GET /v1/users/:id/export` |
| `moderation:read` / `moderation:write` | bans and moderation events — ban, revoke, unban, mute, kick, mute-all, kick-all, report, resolve; `kick`/`mute`/`unmute` action tokens |
| `recordings:read` / `recordings:write` | list / download — start, stop, delete |
| `audio_streams:read` / `audio_streams:write` | list / inspect live streams — pull, push, stop |
| `chat:read` / `chat:write` | stored message history — system / directed messages |
| `tts:write` | `POST /v1/channels/:id/tts` |
| `keys:manage` | API key CRUD |
| `analytics:read` | `GET /v1/analytics*` — series, channel usage, quota state, export ([Usage analytics](../operations/usage-analytics.md)) |
| `audit:read` | `GET /v1/audit-log` |
| `webhooks:read` / `webhooks:write` | subscriptions, deliveries, event catalogue — create, update, rotate, test, resync, retry |
| `events:read` | `GET /v1/events`, `GET /v1/events/snapshot` (and the event catalogue) |

In the OpenAPI document every operation lists its requirement in the `x-aurix-permissions`
extension; administrator operations carry the [admin permission](#administrators) they need in
`x-aurix-admin-permission`. A contract test in `crates/aurix-api/tests/openapi_contract.rs`
keeps the router, the permission checks in the handlers and the specification in sync.

## Administrators

Administrators are not tenant-scoped: they manage applications, nodes and each other. Every
account has one of four **roles**; a role grants a fixed set of **admin permissions** and each
`/admin/*`, `/v1/apps*` and `/v1/nodes` operation requires exactly one of them (`403
FORBIDDEN` otherwise).

| Permission | Grants | `viewer` | `moderator` | `admin` | `superadmin` |
| --- | --- | :-: | :-: | :-: | :-: |
| `apps:read` | `GET /v1/apps`, `GET /v1/apps/{id}` | ✓ | ✓ | ✓ | ✓ |
| `nodes:read` | `GET /v1/nodes` | ✓ | ✓ | ✓ | ✓ |
| `analytics:read` | `GET /admin/analytics/usage`, `GET /admin/analytics/apps/{id}`, `GET /admin/analytics/export` | ✓ | ✓ | ✓ | ✓ |
| `audit:read` | `GET /admin/audit-log` | | ✓ | ✓ | ✓ |
| `moderation:read` | reserved for cross-app moderation views | | ✓ | ✓ | ✓ |
| `apps:write` | `POST /v1/apps`, `PATCH /v1/apps/{id}` | | | ✓ | ✓ |
| `keys:rotate` | `POST /v1/apps/{id}/rotate-key` | | | ✓ | ✓ |
| `apps:delete` | `DELETE /v1/apps/{id}` | | | | ✓ |
| `retention:run` | `POST /admin/retention/sweep` | | | | ✓ |
| `admins:manage` | `GET/POST /admin/admins`, `GET/PATCH /admin/admins/{id}`, password reset, `logout-all` of others | | | | ✓ |

`GET /admin/me`, `POST /admin/me/password` and `POST /admin/logout-all` need only an active
account. `GET /admin/me` returns the role and the `permissions` list, so a dashboard can hide
what the operator cannot do.

**Tokens follow the database, not their claims.** An admin JWT is checked against the account
row on every request: the *current* role and active flag apply, and the token carries a
generation number that must match the account's. Role changes, deactivation, password
changes/resets and `logout-all` bump the generation, so every token issued before them is refused
with `401 TOKEN_INVALID` on every node at once — no revocation list, no waiting for expiry.

Safeguards: an administrator cannot change their own role or deactivate themself, the last
active `superadmin` cannot be demoted or deactivated (`409 CONFLICT`), and SSO role sync never
strips the last superadmin either.

Accounts are created by `POST /admin/setup` (the first one), `POST /admin/admins` (password
accounts) or on first SSO login — see
[Administrator accounts and SSO](../operations/admin-sso.md) for OpenID Connect, role mapping
and the lifecycle API.

## Player token grants

`POST /v1/tokens` takes the user identity (`external_id` + `display_name`, or an existing
`user_id`) and a list of channel grants:

```json
{"channel_id": "<uuid>", "speak": true, "receive": true, "role": "speaker"}
{"ad_hoc": {"name": "match-8f3a", "channel_type": "team", "max_participants": 10}}
```

An ad-hoc grant names a channel that is created on the first join (and removed when the last
participant leaves); its id is derived from the name, so every token for the same name lands in
the same channel and the response tells you the `channel_id` up front. `max_participants` is
clamped to the app limit and creation counts against the app's channel quota.

`speak: false` makes the member a **listener** (`ChannelRole::Listener`) in that channel: the
node drops its audio, it does not count towards `audience.max_speakers`, and with
`audience.hide_listeners` it stays out of other members' presence — the building block for
[large channels](../features/channels.md#large-channels-and-audiences).

## Errors

Every error is `{"error":{"code":"…","message":"…"}}`:

| Status | Codes (examples) |
| --- | --- |
| `400` | `VALIDATION_ERROR`, `CONFIGURATION_ERROR` |
| `401` | `AUTH_FAILED`, `TOKEN_INVALID`, `TOKEN_EXPIRED`, `TOKEN_REUSED` |
| `403` | `AUTH_DENIED` (missing permission or action-token mismatch), `ACTION_TOKEN_REQUIRED`, `USER_BANNED`, `USER_MUTED` |
| `404` | `NOT_FOUND`, `CHANNEL_NOT_FOUND`, `USER_NOT_FOUND`, `SESSION_NOT_FOUND` — unknown resource **or** a resource of another tenant; also `CHAT_DISABLED`, `TTS_DISABLED`, `USER_OFFLINE` |
| `409` | `CHANNEL_FULL`, `CHANNEL_LIMIT_EXCEEDED`, `CONFLICT` (e.g. retention sweep already running, stream not hosted on this node) |
| `422` | `MESSAGE_BLOCKED` — a chat message rejected by the filter |
| `429` | `RATE_LIMIT_EXCEEDED` |
| `500` | `INTERNAL_ERROR`, `DB_ERROR`, `REDIS_ERROR`, `RECORDING_ERROR`, … (`/ready` uses this too) |
| `501` | `NOT_IMPLEMENTED` — feature disabled in this deployment |
| `502` | `TTS_ERROR`, `STT_ERROR` — the speech provider failed |
| `503` | `MEDIA_NODE_UNAVAILABLE` |
| `504` | `TIMEOUT` |

The same codes appear in WebSocket `Error { code, message, client_ref? }` frames.
