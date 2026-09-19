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
2. The node uses the report for the **adaptive downlink bitrate** — `BitrateCommand
   {target_bitrate_kbps, reason}` (also a native `BitrateCommand` packet): loss > 10 % or
   jitter > 50 ms drops the target to 32 kbit/s, loss > 20 % to 16 kbit/s and also raises a
   `quality.alert` event with `metric: "packet_loss"`.
3. Every `media.quality_interval_ms` (2000; `0` disables, minimum 500) the node merges the
   client report with what the SFU measures on that session's **uplink** — sequence gaps (loss),
   RFC 3550 inter-arrival jitter and bitrate. The worse direction decides the rating. Sessions
   whose media path is not bound yet (no `SessionBind` / no WebRTC track) are not rated.
4. The result goes back to the client as `NetworkQuality` whenever the bars change and every
   fifth period as a summary:

```json
{"type":"NetworkQuality","data":{"quality":{"bars":4,"r_factor":76.2,"mos":3.9,"rtt_ms":48.0,
 "downlink_jitter_ms":6.5,"downlink_loss_percent":1.2,"uplink_jitter_ms":3.1,
 "uplink_loss_percent":4.0,"uplink_bitrate_kbps":31,"uplink_packets_received":4120,"uplink_packets_lost":170}}}
```

5. Uplink loss above 20 % over a period raises `quality.alert` with
   `metric: "uplink_packet_loss"` (webhooks/SSE) even if the client reports nothing.

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
gauges and `aurix_api_request_duration_seconds` per route. Per-session detail is deliberately
kept out of Prometheus labels — use the stats endpoint or `quality.alert` events for that.
