# Usage analytics and quotas

Aurix meters what each application consumes — concurrent sessions (CCU), session and
participant minutes, unique users, active channels, recording time, media bytes, chat
messages, TTS and STT — and turns it into time series you can chart, export for billing, and
cap with per-application limits. Everything here is tenant-scoped: an API key sees its own
application, administrators with `analytics:read` see the fleet.

## What is measured and how

There are two kinds of numbers, produced by two different mechanisms:

| kind | metrics | source |
|---|---|---|
| **derived** | `peak_sessions` (CCU), `session_minutes`, `sessions_started`, `unique_users`, `peak_participants`, `participant_minutes`, `active_channels`, `recording_seconds`; per channel `peak_participants`, `participant_minutes`, `joins`, `unique_users` | recomputed from the `sessions`, `channel_memberships` and `recordings` intervals in PostgreSQL by **one node at a time** (advisory lock) every `usage.aggregate_interval_secs` |
| **metered** | `media_bytes_in/out`, `chat_messages`, `tts_requests`, `tts_characters`, `stt_audio_ms` | counted in memory on the node that did the work and **added** to the buckets by every node every `usage.flush_interval_secs` |

Derived metrics are interval arithmetic, not sampling: a session connected 10:03–10:19 puts
2 minutes into the 10:00 bucket, 5 into 10:05, 5 into 10:10 and 4 into 10:15, and the peak
of each bucket is the maximum number of intervals that overlap it. Reconnects, resumes on
another node and cross-node failover keep the same session row, so they are counted once; a
user in two channels at once is one session but two participants — `participant_minutes` is
per membership, which is what the monthly quota meters. Because the aggregator works from the
lifecycle tables, a node that dies takes nothing with it: the fleet reaper closes its
sessions at the last heartbeat (see [High availability](high-availability.md)), and the next
pass derives the corrected buckets — it clears and re-derives every bucket it touches, so a
late closure shrinks the numbers rather than leaving stale ones (every pass re-derives the
last 15 minutes before the watermark, which covers the largest `cluster.node_lost_after_secs`).
A session whose node has vanished from the registry altogether — pruned 24 h after its last
heartbeat before any node reaped it — is closed by the reaper as of the moment it is noticed,
so nothing accrues forever.

Buckets are 5 minutes for applications and 1 hour for channels, aligned to UTC epoch
multiples. The aggregator keeps a **watermark**: buckets before it are final, later ones and
the current one are still accruing. Responses expose it as `range.finalized_through`; do not
bill a bucket until the watermark has passed it. Metered counters have no watermark — they are
eventually complete a flush interval after the fact.

Retention is separate from the lifecycle tables: `usage.retention_days` (400) keeps the
application buckets, `usage.channel_retention_days` (90) the far more numerous channel
buckets. Deleting the underlying sessions (`retention.sessions_days`) does not change already
finalized buckets. On a fleet that upgrades to this release the first aggregation pass
backfills from the oldest session still in the database, capped at `usage.retention_days`.

## Reading it

All range parameters are RFC 3339 (`from`, `to` exclusive; defaults: last 7 days for series,
last 30 for exports; at most 400 days). A range selects every bucket it overlaps: `from` is
rounded down to the bucket boundary of the scope (5 minutes or the requested `step` for
application rows, 1 hour for channel rows) and the effective value is echoed in
`range.from`. Endpoints need an API key with `analytics:read`.

| endpoint | returns |
|---|---|
| `GET /v1/analytics[?from&to&step]` | `current` (live sessions/channels/users), `range` (with `step_secs`, `finalized_through`), `totals` over the range and `series` of application buckets. `step` is a multiple of 300 s; left out, the finest of 5 min / 1 h / 1 day that keeps the series under 5 000 points is chosen (an explicit step over 10 000 points is `400`). Rolled-up buckets sum counters and take the maximum of peaks |
| `GET /v1/analytics/channels[?from&to&limit]` | per-channel totals, busiest first — includes channels deleted since, usage outlives the channel |
| `GET /v1/analytics/channels/{id}[?from&to]` | one channel's hourly series and totals (`404` if the channel neither exists in your application nor has usage there) |
| `GET /v1/analytics/quota` | your limits and how much of them is used (below) |
| `GET /v1/analytics/export[?from&to&scope=app\|channels&format=json\|csv]` | every raw bucket in the range — 5-minute application rows or hourly channel rows — as JSON (`rows`, `count`, `truncated`) or CSV (`text/csv`, `Content-Disposition: attachment`, `X-Aurix-Truncated: true` when cut). At most 200 000 rows per call; page by narrowing the range |

Administrators (`analytics:read`, i.e. every role) get the fleet view: `GET
/admin/analytics/usage` (totals per application), `GET /admin/analytics/apps/{app_id}` (an
application's series plus its quota state) and `GET /admin/analytics/export` (all
applications' 5-minute rows, same shape and cap as the tenant export). `DELETE
/v1/apps/{app_id}` deactivates an application — its API keys stop authenticating and every
node closes its sessions — but its buckets stay: the admin routes keep returning it (with
`"active": false`) so the last invoice can still be produced.

A billing job therefore looks like: once a day, `GET /v1/analytics/export?scope=app&from=<last
run>&to=<finalized_through of the previous response>&format=csv`, and sum `participant_minutes`
(or whatever you charge for) per `app_id`. Rows are idempotent — re-exporting a finalized range
yields the same numbers.

## Per-application limits

Two quotas complement `max_channels` and `max_participants_per_channel`; operators set them
with `POST /v1/apps` / `PATCH /v1/apps/{app_id}` (`0` = unlimited, the default), changes reach
every node within `usage.quota_cache_secs`:

* **`max_concurrent_sessions`** — CCU across the whole fleet. The `SessionInit` that would
  exceed it is refused with `QUOTA_EXCEEDED` (HTTP `429` semantics on the WebSocket error).
  Admission is atomic: nodes take a per-application advisory lock in PostgreSQL around
  "count open sessions, insert", so two nodes cannot both admit the last slot. A user
  reconnecting to the same node replaces their previous session there and is not counted
  against themselves. Existing sessions are never cut when the limit is lowered.
* **`monthly_participant_minutes`** — channel membership minutes per UTC calendar month. It is
  checked at every channel **join** (a join is what starts charging) as *finalized minutes
  since the 1st* + *live overlap of the currently open memberships*, cached per node for
  `quota_cache_secs`; once reached, joins fail with `QUOTA_EXCEEDED` until the month rolls
  over, while sessions may still connect (so the client can show the player why). Members
  already in channels stay — the quota is an admission control, not a kill switch; an
  application that must stop consumption immediately should also `kick-all`.

`GET /v1/analytics/quota` shows both limits with `active_sessions`,
`participant_minutes_this_month` and `month_start`; every refusal increments
`aurix_quota_rejections_total{quota="concurrent_sessions"|"participant_minutes"}`.

## Configuration

```toml
[usage]
enabled = true
flush_interval_secs = 15        # metered counters → database (5..=300)
aggregate_interval_secs = 60    # lifecycle → buckets, one node at a time (30..=3600)
retention_days = 400            # application buckets
channel_retention_days = 90     # channel buckets (≤ retention_days)
quota_cache_secs = 30           # 0 = query the database on every join
```

`enabled = false` stops metering and aggregation: series and exports no longer grow and the
monthly minute quota is not enforced (`max_concurrent_sessions` still is — it only needs the
live session table). Metered counters that could not be flushed (database outage) are kept in
memory and retried; a node that shuts down flushes once more on the way out, a node that
crashes loses at most one flush interval of metered counters — derived metrics are unaffected.
`aurix_usage_deltas_flushed_total` counts what reached the database.

## Limits of the model

* Metered counters are exact only to the flush interval and are lost for a crashed node's last
  interval; derived metrics are exact to the second.
* Series resolution is 5 minutes (applications) and 1 hour (channels); there is no
  per-user or per-session breakdown here — that is what `GET /v1/sessions/{id}/stats`, the
  audit log and the event stream are for.
* Clock time is UTC throughout; billing months are UTC calendar months.
* Quotas are per application, not per API key or per region.
