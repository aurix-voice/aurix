# Observability

## What is durable

Everything durable lives in **PostgreSQL** and, if enabled, the **recording store** (local
volume or S3). Redis holds only ephemeral state (events, one-time token claims, session→node
mapping and session mirrors, node liveness, rate limits, global mutes) and needs no backup —
after a Redis loss, clients reconnect and state rebuilds. Failover, Sentinel and PostgreSQL HA
are covered in [High availability](high-availability.md); what to back up, the restore drill
and the restore procedure in [Backup and restore](backup-restore.md) (`tools/backup/run.sh`).

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
| `aurix_noise_suppression_sessions`, `aurix_noise_suppression_frames_total{path,outcome}` | sessions the node is denoising (`media.noise_suppression.max_sessions` bounds it) and the frames it saw per path (`opus`/`g711` for PCMU and PCMA): `ok` cleaned, `repeated` a copy of the previous frame for another channel (cleaned once), `passthrough` too short / DTX, `skipped` a channel requirement the node could not honour (disabled or full), `error` decode/encode failure (forwarded as it came) |
| `aurix_g711_sessions{codec}`, `aurix_g711_frames_total{codec,direction,outcome}` | sessions on the G.711 fallback (`pcmu`/`pcma`) and the plaintext frames transcoded for them (`uplink`/`downlink`, `ok`/`error`) — CPU the node spends on their behalf; E2EE G.711 frames are relayed, not counted |
| `aurix_mixer_lost_frames_total{method}` | uplink frames the server mixers found missing when a sender's next packet arrived, by how the gap was filled: `fec`, `dred`, `plc`, `skipped` (too long / already played) — a rising `plc`/`skipped` share against `fec`/`dred` means senders' FEC/DRED does not cover the loss ([Packet loss](../sdk/native.md#packet-loss-fec-dred-and-the-neural-plc)) |
| `aurix_tunnel_sessions`, `aurix_tunnel_packets_total{direction,outcome}` | native sessions whose media rides the control WebSocket because UDP is blocked, and their packets (`uplink` `received`/`rejected`, `downlink` `sent`/`dropped`) — many `dropped` means a client's TCP connection is stalling behind loss |
| `aurix_quic_connections`, `aurix_quic_sessions`, `aurix_quic_handshakes_total{outcome}`, `aurix_quic_packets_total{direction,outcome}`, `aurix_quic_migrations_total` | open QUIC connections on the media port (bound or not yet), native sessions bound through QUIC, handshakes (`accepted` / `accepted_0rtt` / `refused` at the connection cap / `failed`), their packets (`uplink` `received`/`rejected`/`dropped` when the inbound queue is full, `downlink` `sent`/`dropped`), and connections whose peer address moved while bound — `failed` counts handshakes the node saw and could not finish (a middlebox that drops QUIC leaves it flat — the client just falls back to UDP), a growing `refused` means `media.quic_max_connections` is too low |
| `aurix_tls_tunnel_connections`, `aurix_tls_tunnel_sessions`, `aurix_tls_tunnel_handshakes_total{outcome}`, `aurix_tls_tunnel_packets_total{direction,outcome}` | the dedicated TLS media tunnel (`media.tls_tunnel_port`, typically 443): connections, bound sessions, handshakes (`accepted` / `refused` at `media.tls_tunnel_max_connections` / `failed` TLS or ALPN error — a proxy terminating TLS instead of passing it through / `unbound` no authenticated `SessionBind` within `media.tls_tunnel_bind_timeout_ms`) and packets (`uplink` `received`/`rejected`/`malformed` — a framing violation closes the connection; `downlink` `sent`/`dropped` at `media.tls_tunnel_queue_packets`) |
| `aurix_webtransport_connections`, `aurix_webtransport_sessions`, `aurix_webtransport_handshakes_total{outcome}`, `aurix_webtransport_packets_total{direction,outcome}`, `aurix_webtransport_cert_rotations_total{outcome}` | browsers on WebTransport: sessions, handshakes (`accepted` / `refused` / `failed` — usually a stale certificate pin / `not_found` wrong path / `unbound`), datagrams (`uplink` `received`/`rejected`/`malformed`, `downlink` `sent`/`dropped`) and rotations of the short-lived certificate (`rotated` / `failed` — a failed rotation becomes a handshake outage when the served certificate expires; `aurix doctor` shows the days left) |
| `aurix_downlink_mixers{kind}`, `aurix_downlink_mix_frames_total{outcome}`, `aurix_streams_capped_total` | server mix for `downlink_mode = mixed` listeners — `shared` (one per channel) and `private` (per receiver with own prefs) 20 ms tickers and their frames (`sent` / `dropped` ticker late / `failed` encode: the node is CPU-bound) — and per-speaker packets withheld by `audience.max_streams` |
| `aurix_speaker_slot_events_total{event}` | speaker-slot admission in channels with `audience.max_speakers`: `rejected` (mode `reject`, join refused), `waited` (joined as listener, waits for a slot), `demoted` (an idle speaker yielded its slot), `admitted` (a waiting member got its slot) |
| `aurix_live_streams{mode}`, `aurix_live_streams_reconnecting`, `aurix_live_stream_frames_total{outcome}`, `aurix_live_streams_closed_total{reason}` | live audio streams this node owns (`pull` / `push`), streams whose consumer is away with frames in the outage buffer, frames `sent` / `dropped` (slow consumer or buffer overflow — the media path never blocks on a consumer) and closes by reason (`operator`, `channel_stopped`, `duration_limit`, `consumer_disconnected` — pull with `recording.live.outage_buffer_ms = 0`, `consumer_timeout`, `push_unreachable`, `server_shutdown`) |
| `aurix_cascade_links{transport}`, `aurix_cascade_forwarded_total{role}`, `aurix_cascade_hub_channels`, `aurix_cascade_tcp_dropped_total` | inter-node cascade: peers by transport (`udp` / `tcp` fallback when UDP between the nodes is blocked / `unconfirmed` — no probe answered), envelopes forwarded (`origin` / `hub` / `hop_limit`), channels this node relays for as a tree hub, and envelopes dropped by a disconnected or congested TCP link (`GET /v1/nodes/links` has the pairwise RTT/transport table) |
| `aurix_api_requests_total{method,path,status}`, `aurix_api_request_duration_seconds` | REST (path templated, ids collapsed) |
| `aurix_ws_connections`, `aurix_ws_sessions_detached`, `aurix_ws_sessions_resumed_total` | control plane and reconnects |
| `aurix_quota_rejections_total{quota}`, `aurix_usage_deltas_flushed_total` | per-application quota refusals (`concurrent_sessions` / `participant_minutes`) and metered usage counters written to the database ([Usage analytics and quotas](usage-analytics.md)) |
| `aurix_rate_limit_hits_total`, `aurix_rate_limit_scope_hits_total{scope,backend}`, `aurix_rate_limit_backend_errors_total`, `aurix_moderation_events_total` | abuse signals — `scope` is `api_ip` / `api_key` / `connect` / `join` / `block` / `report` / `admin_login`, `backend` is `fleet` (shared Redis bucket) or `local`; backend errors mean the fleet limiter fell back to per-node buckets (or refused, with `fail_closed`) |
| `aurix_turn_allocations`, `aurix_stun_requests_total` | TURN |
| `aurix_webhook_deliveries_total{result}`, `aurix_webhook_deliveries_leased`, `aurix_event_stream_clients` | webhooks / SSE |
| `aurix_node_cpu_usage`, `aurix_node_memory_usage`, `aurix_node_bandwidth_in_mbps` / `_out_mbps` | node health as reported to the registry |

### Alerts and dashboards

`deploy/prometheus.yml` scrapes the node and loads `deploy/prometheus-alerts.yml`, four rule
groups:

* `aurix-quality` — fleet-level voice quality (`AurixMedianMosLow`, `AurixP10MosPoor`,
  `AurixDegradedSessionsHigh`, `AurixUplinkLossHigh`, `AurixUplinkJitterHigh`,
  `AurixQualityAlertStorm`, `AurixNodeQualityOutlier`), each with a minimum number of ratings
  so a handful of sessions cannot page you.
* `aurix-transports` — per node, for QUIC, the TLS tunnel and WebTransport: handshakes failing
  or never binding above 20 % (`Aurix*HandshakesFailing` — a certificate the clients do not
  pin, a TLS-terminating proxy, clock skew), the connection cap reached
  (`Aurix*ConnectionCapReached`), downlink drops above 2 % (`Aurix*DownlinkDrops`, also for
  the WebSocket tunnel), malformed TLS tunnel framing, a failed WebTransport certificate
  rotation (`AurixWebTransportCertRotationFailed`, critical — new browser sessions fail once
  the served certificate expires) and rejected uplink packets across all transports
  (`AurixRejectedUplinkPackets`: stale keys after a failover, replay, abuse).
* `aurix-processing` — the noise-suppression pool full (`AurixNoiseSuppressionPoolFull`:
  channel-requested cleaning silently not applied) or erroring, server-mixed downlink frames
  dropped (`AurixDownlinkMixFramesDropped`: CPU-bound node), live-stream frames dropped,
  streams stuck `reconnecting` or closed because the consumer never came back, and joins
  rejected for lack of a speaker slot (`AurixSpeakerSlotsRejecting`, info — consider
  `speaker_admission = "wait"` or `"demote"`).
* `aurix-cascade` — peers that answer no probe (`AurixCascadeLinksUnconfirmed`: cross-node
  audio may be one-directional), the TCP fallback active for 15 minutes
  (`AurixCascadeTcpFallbackActive`, info) and envelopes dropped on it (`AurixCascadeTcpDrops`).

Ready dashboards under `deploy/grafana/dashboards/` (`docker compose --profile observability
up -d`): `aurix-overview.json` (sessions, traffic, TURN, REST, rate limits), `aurix-quality.json`
(MOS percentiles and heatmap, bars, alerts/recoveries, uplink loss/jitter, per-node mean MOS)
and `aurix-media-paths.json` (sessions per transport, handshakes and their failure ratios,
packets and drop ratios on every fallback transport, certificate rotations, noise
suppression, downlink mix, speaker slots, live streams, cascade links and TCP fallback).
`tools/observability/check.py` (run in CI together with `promtool check rules`) fails when a
rule or panel references a metric or label the node does not register. Also alert on
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
