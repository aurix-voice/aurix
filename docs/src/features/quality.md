# Network quality and statistics

Every SDK exposes one statistics snapshot with the same vocabulary — packets/bytes in both
directions, RTT last/min/avg/max, downlink jitter, loss over the last period (**percent**,
`0..=100`), frames lost / late / discarded and jitter-buffer underruns, authentication and
replay failures, heartbeat loss, active remote streams — and the derived rating: a simplified
E-model **R-factor** (`0..=100`), the **MOS** it maps to and **1–5 bars**:

| R-factor | bars |
|---|---|
| ≥ 80 | 5 |
| ≥ 70 | 4 |
| ≥ 60 | 3 |
| ≥ 50 | 2 |
| otherwise | 1 |

The same formula (`aurix_common::types::quality`) runs on the server, so a HUD can show either
side's number without recalibrating.

The E-model in use is the simplified ITU-T G.107 form: effective latency `RTT + 2·jitter +
10 ms`, `R = 93.2 − latency/40` below 160 ms (steeper above), minus 2.5 R per percent of
loss; `MOS = 1 + 0.035·R + R·(R − 60)·(100 − R)·7·10⁻⁶` (4.3 is the ceiling for a
narrowband codec). It is a network score: it does not hear the audio, so it cannot see a
broken microphone or a codec at 6 kbit/s — pair it with `channel.energy` / client DSP stats
for that.

| | snapshot | server rating event | report interval |
|---|---|---|---|
| Web | `getStats()` → `ClientStats` (from `RTCPeerConnection.getStats()`), `stats` event | `networkQuality` event | `qualityReportIntervalMs` (5000) |
| Unity | `Client.GetStats()` → `VoiceStats`, `OnStats` | `OnNetworkQuality`, `VoiceStats.Server` | `QualityReportInterval` (5 s) |
| native / C ABI | `Client::stats()` / `aurix_client_stats`, `aurix_client_network_quality` | `NetworkQuality` event | client config |
| Unreal | `GetStats`, `GetNetworkQuality` | `OnNetworkQuality` | plugin settings |

Client counters are cumulative for the current transport (they reset on a fresh session, not
on resume); loss, R-factor, MOS and bars describe the latest period.

## How the rating is produced

1. Clients send `QualityReport {rtt_ms, jitter_ms, packet_loss}` (loss in percent) every report
   interval; `0` disables it.
2. The node uses the report for the **adaptive uplink bitrate** — `BitrateCommand
   {target_bitrate_kbps, reason, expected_loss_percent}` (also a native `BitrateCommand`
   packet): loss > 10 % or jitter > 50 ms drops the target to 32 kbit/s, loss > 20 % to
   16 kbit/s (and raises a `quality.alert` event with `metric: "packet_loss"`); loss ≤ 2 % with
   jitter ≤ 20 ms returns to the channel target. Targets are clamped to the merged channel
   policy's `min_bitrate..=bitrate` ([channels](channels.md)), a command is only sent when the
   value changes, and `expected_loss_percent` lets libopus-based clients tune in-band FEC
   (`OPUS_SET_PACKET_LOSS_PERC`). The command changes the running encoder, not the client's
   configured baseline: a new policy (join/leave/edit) recomputes from the baseline.
3. Every `media.quality_interval_ms` (2000; `0` disables, minimum 500) the node merges the
   client report with what the SFU measures on that session's **uplink** — sequence gaps (loss),
   RFC 3550 inter-arrival jitter and bitrate. The worse direction decides the rating. Sessions
   whose media path is not bound yet (no `SessionBind` / no WebRTC track) are not rated.
4. The result goes back to the client as `NetworkQuality` whenever the bars change, whenever
   the loss its senders must protect against crosses a tier (3 % / 10 %) and every fifth period
   as a summary:

```json
{"type":"NetworkQuality","data":{"quality":{"bars":4,"r_factor":76.2,"mos":3.9,"rtt_ms":48.0,
 "downlink_jitter_ms":6.5,"downlink_loss_percent":1.2,"uplink_jitter_ms":3.1,
 "uplink_loss_percent":4.0,"receivers_loss_percent":0.0,"uplink_bitrate_kbps":31,
 "uplink_packets_received":4120,"uplink_packets_lost":170}}}
```

5. Uplink loss above `quality.loss_alert_percent` (20 %) over a period raises `quality.alert`
   with `metric: "uplink_packet_loss"` (webhooks/SSE) even if the client reports nothing; the
   same threshold applies to the client-reported loss (`metric: "packet_loss"`).
6. `receivers_loss_percent` is the worst downlink loss any *receiver* of that session's audio
   on the same node reported in its own `QualityReport` (0 with no receivers; receivers hosted
   on other nodes of a cascade are not included). Only the sender can add redundancy for a
   receiver on a lossy link, so native, Unity and Unreal/Godot clients feed the higher of
   `uplink_loss_percent` and `receivers_loss_percent` to their **loss profile**: ≥ 3 % turns
   in-band FEC on and tunes it for ≥ 10 % loss, ≥ 10 % adds Opus DRED history and a
   28 kbit/s floor; tiers relax after a 6 s dwell below 1 % / 5 %. Lost frames are rebuilt on
   receivers and in the server mixers from FEC → DRED → neural PLC
   ([Packet loss](../sdk/native.md#packet-loss-fec-dred-and-the-neural-plc);
   `aurix_mixer_lost_frames_total{method}`).

## MOS alerts

Loss alerts are per period; the **MOS alert** is a state machine per session (`[quality]`):

```toml
[quality]
mos_alert_threshold = 3.1   # 0 disables
mos_alert_periods = 3       # consecutive periods (media.quality_interval_ms each)
loss_alert_percent = 20.0
persist_interval_secs = 60
```

* MOS below `mos_alert_threshold` for `mos_alert_periods` consecutive evaluations →
  `quality.alert {metric: "mos", value, threshold}`, once. Bad samples while the alert is open
  produce nothing more.
* MOS **at or above `threshold + 0.2`** for as many consecutive evaluations →
  `quality.recovered {metric: "mos", value, threshold}`. The hysteresis band stops a session
  hovering at the threshold from flapping; a single good sample resets the recovery count but
  does not clear the alert.
* Unrated periods (media path not bound) neither count nor reset. Resume on the same node keeps
  the state; cross-node takeover starts a fresh state machine (the new node has not observed
  the session yet), so at most one duplicate alert can follow a failover.

Both events are public (`GET /v1/webhooks/events`), carry `session_id` / `user_id` and are
counted in `aurix_quality_events_total{metric,event}`; `aurix_sessions_mos_degraded` is the
number of sessions whose alert is open right now. With the defaults a player has to sit at
2 bars or worse for 6 s to alert and above ~3.3 MOS for 6 s to recover.

## Per-session history

Every rated period also feeds a per-session **summary** that survives the session:

```json
{"samples":412,"seconds":824.0,"mos_avg":3.94,"mos_min":2.71,"mos_last":4.12,
 "r_factor_avg":78.3,"rtt_avg_ms":51.0,"rtt_max_ms":212.0,"jitter_avg_ms":6.1,
 "loss_avg_percent":0.8,"loss_max_percent":14.0,"bars":[0,3,21,150,238],
 "poor_seconds":48.0,"mos_alerts":1,"last":{"bars":5,"r_factor":83.1,"mos":4.12,…}}
```

`bars[i]` counts samples at `i + 1` bars, `poor_seconds` is time spent at 1–2 bars, averages
are per sample (samples are `media.quality_interval_ms` apart), jitter and loss are the worse
direction of each sample. The summary is:

* live in `GET /v1/sessions/{id}/stats` as `quality_summary` (plus `mos_alerting`);
* checkpointed to `sessions.quality_stats` every `quality.persist_interval_secs` (60; `0` =
  only on disconnect) and written finally when the session closes, so it is also what
  `GET /v1/users/{id}` returns for past sessions;
* carried across cross-node failover: the adopting node reads the last checkpoint and continues
  the counts (no double counting — usage counters are only ever flushed once per sample, by
  the node that observed it), so a session's history is one record however many nodes served
  it. A node crash loses at most the samples since its last checkpoint.

`GET /v1/analytics/sessions[?from&to&min_samples&limit]` (`analytics:read`) ranks the
sessions of your application that **connected** in the range worst average MOS first — live
and closed alike, `min_samples` (3) hides sessions rated only a moment, `limit` up to 500 —
which is the "who had a bad call last night" query; `GET /v1/analytics` and the exports
carry the fleet-level aggregates ([Usage analytics](../operations/usage-analytics.md)). Live
sessions in that list are at most one checkpoint stale; the stats endpoint has the current
value.

## Operator view

`GET /v1/sessions/{session_id}/stats` (`channels:read`) returns the same picture per session:

```json
{"session_id":"…","user_id":"…","transport":"Aurx","channels":["…"],
 "packets_sent":9120,"bytes_sent":1093400,"packets_received":4310,"bytes_received":517200,
 "client_report":{"rtt_ms":48.0,"jitter_ms":6.5,"packet_loss_percent":1.2,"bitrate_kbps":32,"mos_score":3.9},
 "quality":{"bars":4,"r_factor":76.2,…}}
```

`transport` is `Aurx` or `WebRtc`; `client_report` is `null` until the first `QualityReport`.
It is **node-local**: query the node the session lives on (`media_node_id` in the sessions of
`GET /v1/users/{user_id}`); another node answers `404`. A session of another application is
also `404`.

## Prometheus

The node exports aggregate counterparts on `/metrics` (see
[Backups and observability](../operations/observability.md)): packet/byte counters,
`aurix_packets_dropped_total`, `aurix_packet_loss_rate`, session/participant/channel
gauges, `aurix_api_request_duration_seconds` per route, and the quality distributions:
`aurix_session_mos` (histogram, one observation per rated session per period),
`aurix_uplink_loss_percent`, `aurix_uplink_jitter_milliseconds`, `aurix_sessions_by_bars{bars}`,
`aurix_sessions_mos_degraded` and `aurix_quality_events_total{metric,event}`. Per-session
detail is deliberately kept out of Prometheus labels (a label per session or user would blow
up the time-series database) — use the stats endpoint, `GET /v1/analytics/sessions` or the
`quality.alert` events for that. `deploy/prometheus-alerts.yml` ships fleet-level rules
(median / p10 MOS, degraded share, uplink loss / jitter, alert storms, per-node outliers) and
`deploy/grafana/dashboards/aurix-quality.json` the matching dashboard.
