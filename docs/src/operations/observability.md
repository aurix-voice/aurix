# Backups and observability

## What is durable

Everything durable lives in **PostgreSQL** and, if enabled, the **recording store** (local
volume or S3). Redis holds only ephemeral state (events, one-time token claims, session→node
mapping and session mirrors, node liveness, rate limits, global mutes) and needs no backup —
after a Redis loss, clients reconnect and state rebuilds. Failover, Sentinel and PostgreSQL HA
are covered in [High availability](high-availability.md).

```bash
docker compose exec db pg_dump -U "$POSTGRES_USER" -Fc aurix > aurix-$(date +%F).dump
docker compose exec -T db pg_restore -U "$POSTGRES_USER" -d aurix --clean < aurix-2025-01-01.dump
```

* Recordings: back up the `recordings` volume, or use S3 with versioning. If recording
  encryption is on, `AURIX__RECORDING__ENCRYPTION_KEY` **must** be backed up separately — files
  are unreadable without it (see [Recordings](../features/recordings.md)).
* Backups outlive user erasure (`DELETE /v1/users/{id}`): keep backup retention in line with
  your privacy commitments, and note that tombstones in a restored database still block tokens
  issued before the erasure.
* Migrations are embedded and applied on start (`database.run_migrations = true`), or
  explicitly with `aurix-server --migrate-only`.

## Metrics

`GET :4040/metrics` (`[metrics]`; **internal only**) exports Prometheus text. Notable series:

| metric | meaning |
|---|---|
| `aurix_active_sessions`, `aurix_sessions_total`, `aurix_session_duration_seconds` | sessions on this node |
| `aurix_active_channels`, `aurix_active_participants`, `aurix_active_bans` | live gauges |
| `aurix_packets_received_total`, `aurix_packets_sent_total`, `aurix_bytes_*_total` | SFU traffic |
| `aurix_packets_dropped_total` | packets rejected before routing: bad authentication tag, replay, unknown session, malformed |
| `aurix_packet_loss_rate`, `aurix_jitter_milliseconds`, `aurix_rtt_milliseconds` | quality aggregates |
| `aurix_session_mos`, `aurix_uplink_loss_percent`, `aurix_uplink_jitter_milliseconds` | histograms with one observation per rated session per `media.quality_interval_ms` — `histogram_quantile(0.5, sum by (le) (rate(aurix_session_mos_bucket[5m])))` is the fleet median MOS, `_sum / _count` the mean |
| `aurix_sessions_by_bars{bars}`, `aurix_sessions_mos_degraded`, `aurix_quality_events_total{metric,event}` | rated sessions per bar level, sessions with an open MOS alert, `quality.alert` / `quality.recovered` published (`metric` `packet_loss` / `uplink_packet_loss` / `mos`, `event` `alert` / `recovered`). No session or user labels anywhere — per-session detail lives in `GET /v1/analytics/sessions` and the stats endpoint ([Network quality](../features/quality.md)) |
| `aurix_pcmu_sessions`, `aurix_pcmu_frames_total{direction,outcome}` | sessions on the G.711 fallback and the frames transcoded for them (`uplink`/`downlink`, `ok`/`error`) — CPU the node spends on their behalf |
| `aurix_mixer_lost_frames_total{method}` | uplink frames the server mixers found missing when a sender's next packet arrived, by how the gap was filled: `fec`, `dred`, `plc`, `skipped` (too long / already played) — a rising `plc`/`skipped` share against `fec`/`dred` means senders' FEC/DRED does not cover the loss ([Packet loss](../sdk/native.md#packet-loss-fec-dred-and-the-neural-plc)) |
| `aurix_tunnel_sessions`, `aurix_tunnel_packets_total{direction,outcome}` | native sessions whose media rides the control WebSocket because UDP is blocked, and their packets (`uplink` `received`/`rejected`, `downlink` `sent`/`dropped`) — many `dropped` means a client's TCP connection is stalling behind loss |
| `aurix_api_requests_total{method,path,status}`, `aurix_api_request_duration_seconds` | REST (path templated, ids collapsed) |
| `aurix_ws_connections`, `aurix_ws_sessions_detached`, `aurix_ws_sessions_resumed_total` | control plane and reconnects |
| `aurix_quota_rejections_total{quota}`, `aurix_usage_deltas_flushed_total` | per-application quota refusals (`concurrent_sessions` / `participant_minutes`) and metered usage counters written to the database ([Usage analytics and quotas](usage-analytics.md)) |
| `aurix_rate_limit_hits_total`, `aurix_rate_limit_scope_hits_total{scope,backend}`, `aurix_rate_limit_backend_errors_total`, `aurix_moderation_events_total` | abuse signals — `scope` is `api_ip` / `api_key` / `connect` / `join` / `block` / `report` / `admin_login`, `backend` is `fleet` (shared Redis bucket) or `local`; backend errors mean the fleet limiter fell back to per-node buckets (or refused, with `fail_closed`) |
| `aurix_turn_allocations`, `aurix_stun_requests_total` | TURN |
| `aurix_webhook_deliveries_total{result}`, `aurix_webhook_deliveries_leased`, `aurix_event_stream_clients` | webhooks / SSE |
| `aurix_node_cpu_usage`, `aurix_node_memory_usage`, `aurix_node_bandwidth_in_mbps` / `_out_mbps` | node health as reported to the registry |

`deploy/prometheus.yml` scrapes the node and loads `deploy/prometheus-alerts.yml` — fleet-level
voice-quality rules (`AurixMedianMosLow`, `AurixP10MosPoor`, `AurixDegradedSessionsHigh`,
`AurixUplinkLossHigh`, `AurixUplinkJitterHigh`, `AurixQualityAlertStorm`,
`AurixNodeQualityOutlier`), each with a minimum number of ratings so a handful of sessions
cannot page you; `deploy/grafana/dashboards/aurix-overview.json` and `aurix-quality.json`
(MOS percentiles and heatmap, bars, alerts/recoveries, uplink loss/jitter, per-node mean MOS)
are ready dashboards (`docker compose --profile observability up -d`). Also alert on
`aurix_packets_dropped_total` rising (forged or misconfigured clients),
`aurix_packet_loss_rate`, webhook failures and `aurix_ws_sessions_detached` staying high
(clients that cannot resume).

## Logs and traces

Structured logs via `tracing`: `tracing.log_format = json` (default) or `pretty`,
`tracing.log_level` (`info`; `debug` per crate with `RUST_LOG=aurix_media=debug,info`). Set
`tracing.otlp_endpoint` to export spans over OTLP/gRPC (`tracing.service_name`). Secrets and
tokens are never logged; media payloads never leave the SFU.

## Audit log

Every privileged action — admin login, application/API-key changes, bans, mutes, kicks
(single and channel-wide), recording start/stop/download/delete, live-stream start/stop,
webhook changes, user export and erasure, retention sweeps — is written to `audit_log` with
actor, target, IP and a JSON detail blob, and readable
per application with `GET /v1/audit-log` (`audit:read`) or fleet-wide with
`GET /admin/audit-log`. `retention.audit_log_days = 0` keeps it forever. See
[Moderation and lifecycle](../features/moderation.md#audit-log).

## Health

* `GET /health` — process is up.
* `GET /ready` — PostgreSQL (and Redis when configured) reachable; used by the container
  healthcheck and by load balancers.
* `GET /v1/nodes` (admin) — registry view of every node: healthy flag, load, last heartbeat.
