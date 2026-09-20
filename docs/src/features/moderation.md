# Moderation, action tokens and user lifecycle

Everything in this chapter is tenant-scoped: an API key or admin token acts only inside its
own application, and a `user_id`/`channel_id` belonging to another application behaves as
`404 NOT_FOUND`. Every operation below writes an audit-log row (`GET /v1/audit-log`) and,
where players are affected, a public event for webhooks/SSE.

## Server-side mute

`POST /v1/moderation/mute` `{user_id, channel_id, muted, moderator_user_id?}`
(`moderation:write`).

* The SFU stops forwarding the participant's audio **and text** in that channel on every node
  (the change travels over the Redis bus). Their client receives `MuteStateChanged
  {server_muted: true}`; other participants see the same message and `participant.muted` /
  `participant.unmuted` events are published.
* A muted participant keeps hearing the channel; sending chat returns `USER_MUTED`.
* `POST /v1/moderation/mute-all` `{channel_id, muted, except: [user_id…]}` applies the same
  path to every current participant and returns `{affected, skipped, failed}` plus one summary
  audit row (`channel.mute_all`).

## Kick

`POST /v1/moderation/kick` `{user_id, channel_id, reason}` removes the participant from the
channel: they receive `Kick {channel_id, reason}` followed by the usual `ParticipantLeft`
fan-out and a `participant.kicked` event. The session itself stays open, so the client may
rejoin unless your backend refuses to issue a new join grant. `kick-all` mirrors `mute-all`
(`{channel_id, reason, except}` → `{affected, skipped, failed}`, audit `channel.kick_all`).

## Priority speakers

`POST /v1/moderation/priority` `{user_id, channel_id, priority, moderator_user_id?}`
(`moderation:write`) promotes or demotes a **priority speaker** — while they talk, every
non-priority voice in the channel is ducked per `ChannelConfig.ducking`. The flag is set on the
user's open membership (`404` when they are not a member), announced to the channel as
`PriorityChanged` on every node, audited (`priority_changed`) and published as
`participant.priority_changed`. Moderators can do the same from the client with `SetPriority`;
see [Priority speakers and ducking](channels.md#priority-speakers-and-ducking).

## Bans

`POST /v1/moderation/ban` (`moderation:write`):

```json
{
  "user_id": "…",
  "scope": "account" | "device" | "ip_address",
  "reason": "…",
  "duration_hours": 24,
  "device_id": "…",
  "ip_address": "…",
  "moderator_user_id": "…"
}
```

* Omitting `duration_hours` makes the ban permanent. `device` / `ip_address` scopes require the
  matching field.
* The ban is stored in PostgreSQL and mirrored on the user row (`is_banned`,
  `ban_expires_at`), and a `user.banned` event closes every live session of that user on all
  nodes. Opening a new session or issuing a token for a banned user fails with
  `403 USER_BANNED`.
* `GET /v1/moderation/bans` lists bans (`user_id`, `page`, `per_page`),
  `POST /v1/moderation/bans/{ban_id}/revoke` lifts one ban and
  `POST /v1/users/{user_id}/unban` lifts all active bans of a user.

## Reports and moderation events

Players (via your backend) file reports with `POST /v1/moderation/report`
`{target_user_id, reporter_user_id, channel_id?, reason, evidence?, recording_id?}`.
Reports, bans, kicks and mutes all become **moderation events** —
`GET /v1/moderation/events` (filter by `status`, paginate with `page`/`per_page`) and
`GET /v1/moderation/events/{event_id}`. A moderator closes a case with
`POST …/events/{event_id}/resolve` `{resolution, moderator_user_id?}`, which sets
`status = resolved` and `resolved_at`. `evidence` is free-form JSON (chat excerpts, match id,
…); `recording_id` links a recording that exists in the same application.

Resolved events are subject to the retention sweep (`retention.moderation_events_days`, 365
by default); open ones are kept until resolved. Incidents raised by the
[content-safety pipeline](safety.md) are moderation events too (`safety.voice` / `safety.text`),
listed and exported with their evidence under `/v1/safety/…`.

## Action tokens

Action tokens are one-time JWTs your backend mints for a single privileged act so that a
leaked long-lived player token cannot be used to join arbitrary channels or moderate. Issue
them with `POST /v1/tokens/action` (`tokens:issue`):

```json
{
  "action": "login" | "join" | "kick" | "mute" | "unmute",
  "user_id": "…",             // or external_id (+ display_name) to upsert the player
  "channel_id": "…",          // join / kick / mute / unmute
  "target_user_id": "…",      // kick / mute / unmute
  "speak": true, "receive": true, "moderate": false,   // join grant
  "ad_hoc": { "name": "party-42", "channel_type": "team", "max_participants": 8 },
  "metadata": {},
  "ttl_secs": 90
}
```

* Each token carries a `jti`. The first use claims it atomically in Redis (`SET NX EX`,
  key includes the tenant); a replay, a different actor, a different target or a different
  channel fails with `403 ACTION_TOKEN_INVALID`.
* TTL defaults to `auth.action_token_ttl_secs` (90 s) and cannot exceed
  `auth.action_token_max_ttl_secs` (600 s).
* `login` opens a session (`SessionInit.token` or the WS bearer); it survives resume of *that*
  session but does not open a second fresh session.
* `join` is passed as `ChannelJoin.token`; `ad_hoc` lets the first joiner create the channel
  (see [Channels](channels.md#ad-hoc-channels)).
* `kick` / `mute` / `unmute` are used by a *client* through `ModerateParticipant {channel_id,
  user_id, action, token, reason?}` → `ModerateParticipantAck`. The server performs exactly the
  same steps as the REST endpoints (SFU, database, audit, events); the actor is the token
  subject, not the WS session's own identity.
* `AURIX__AUTH__REQUIRE_ACTION_TOKENS=true` makes `login`/`join` tokens mandatory: an ordinary
  session JWT can no longer open a session or join a channel (`403 ACTION_TOKEN_REQUIRED`).

SDKs: Web `moderate()`, Unity `ModerateAsync()`, native `aurix_client_moderate`; join tokens
are supplied through `joinToken` / `JoinTokenProvider` / `aurix_client_join_channel_with_token`.

## User erasure (`DELETE /v1/users/{user_id}`)

Requires `users:erase`. Order of operations:

1. recordings the user appears in are stopped and their files/objects deleted;
2. in one transaction: sessions, channel memberships, chat messages, blocks (both directions),
   recording rows, reports the user *filed* are anonymised (`reporter_user_id = null`),
   a **tombstone** row is written and the user row is deleted. Moderation events *about* the
   user are operator evidence and stay unless `?purge_moderation=true`;
3. `user.deleted` closes live sessions on every node and Redis state is cleared.

Any JWT, action or resume token issued before the tombstone is rejected on both the WS and
REST paths. Tombstones live `retention.tombstones_days` (30) — configuration validation
forces that to cover the longest token TTL so no pre-deletion token can outlive it.
Re-creating the same `external_id` yields a new `user_id`.

## Data export (`GET /v1/users/{user_id}/export`)

Requires `users:export`. Returns one JSON document (`format`, `exported_at`, `app_id`) with
the user row, sessions, channel memberships, chat messages (if persisted), blocks, bans,
recordings and `moderation.about_user` / `moderation.reported_by_user`. Each collection is cut
at 10 000 rows (newest first) and listed in `truncated` if it was.

## Retention sweep

`[retention]` in `config/default.toml`:

| key | default | effect |
|---|---|---|
| `enabled` | `true` | run the periodic sweep |
| `sessions_days` | 90 | closed sessions and their memberships |
| `moderation_events_days` | 365 | *resolved* moderation events |
| `audit_log_days` | 0 | audit rows (`0` = keep forever) |
| `analytics_days` | 400 | analytics snapshots |
| `inactive_users_days` | 0 | users without a session for N days (`0` = off; banned users are never swept) |
| `tombstones_days` | 30 | erasure tombstones |
| `batch_size` | 5000 | rows deleted per statement |
| `interval_secs` | 3600 | period; first run happens one interval after start |

The sweep runs under a PostgreSQL advisory lock, so only one node works at a time.
`POST /admin/retention/sweep` (admin JWT) runs it immediately and returns per-table counts,
or `409` if another node holds the lock.

## Audit log

`GET /v1/audit-log` (`audit:read`) returns this application's rows; `GET /admin/audit-log`
(admin JWT) spans all applications. Rows carry `action` (snake_case, e.g. `user_banned`,
`participant_kicked`, `channel_mute_all`, `api_key_created`, `webhook_secret_rotated`,
`user_data_exported`, `retention_sweep`), `actor_id`, `target_type`/`target_id`, JSON
`details`, `ip_address` and a `previous_hash`/`hash` chain so tampering with earlier rows is
detectable. Paginate with `page`/`per_page`.
