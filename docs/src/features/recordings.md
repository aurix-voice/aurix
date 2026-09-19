# Recordings and live audio streams

Both features take audio *off* the media path on the node that hosts the participant. Neither
ever sees end-to-end-encrypted frames (`E2EE` flag / insertable-streams payloads) — the node
cannot read them. Both are **node-local**: start them on the node the participant is connected
to (the node id is part of the sessions returned by `GET /v1/users/{user_id}`), or use push
streams, which do not depend on where the operator connects.

## File recordings

```toml
[recording]
enabled = false                     # per node
storage_path = "/var/lib/aurix/recordings"
max_recording_duration_secs = 7200  # hard stop
retention_days = 90                 # expires_at; hourly cleanup deletes file + row
encryption_enabled = true           # AES-256-GCM at rest; requires encryption_key (>= 32 chars)
require_consent = true
# s3_bucket / s3_region / s3_endpoint / s3_access_key / s3_secret_key
```

### Lifecycle

1. `POST /v1/recordings/start` `{channel_id, user_id, session_id?}` (`recordings:write`) —
   records **one participant** in one channel (per-participant, not a channel mix). Fails with
   `400 INVALID_CONFIG` when `recording.enabled = false` on this node, `400 VALIDATION_ERROR`
   when the user is not currently in the channel and `404` when the session is not on this node.
2. The participant and every member of the channel receive `RecordingNotification
   {channel_id, recording_id, active: true, initiated_by, live: false}`; a `recording.started`
   event is published and audited.
3. With `require_consent = true` the recording is `pending` and no audio is written until the
   participant answers with `RecordingConsentResponse {recording_id, consent: accepted |
   declined}` over the control socket or `POST /v1/me/recordings/{recording_id}/consent`
   (player JWT). `declined` keeps the file empty; the decision is visible as `consent` in
   `GET /v1/recordings/{recording_id}`, and the pending request is published as
   `recording.consent_required` for your UI.
4. The SFU appends the participant's Opus packets (decrypted uplink) to an **Ogg/Opus** file,
   reordering late packets and preserving gaps through the RTP timeline (granule positions).
   `POST /v1/recordings/{recording_id}/stop`, the participant leaving the channel (including
   session close and kicks), `max_recording_duration_secs` or user erasure finishes the file. If
   encryption is enabled the finished file is sealed with
   AES-256-GCM (`encrypted`, `encryption_key_id` = first 8 bytes of SHA-256 of the key, so a
   key rotation is detected instead of yielding garbage).
5. With S3 configured the finished object is uploaded as `<app_id>/<recording_id>.ogg`; the
   local copy is kept so the download endpoint keeps working, and failures to upload only log a
   warning.

### Reading recordings

* `GET /v1/recordings?channel_id=&page=&per_page=` and `GET /v1/recordings/{recording_id}`
  (`recordings:read`) return metadata plus `download_url` — a 15-minute pre-signed S3 URL when
  S3 is configured, `null` for local storage — and the consent state.
* `GET /v1/recordings/{recording_id}/download` streams the decrypted `audio/ogg` bytes from the
  node's disk (`409` while the recording is still running) and writes a `recording_accessed`
  audit row.
* `DELETE /v1/recordings/{recording_id}` removes the row, the local file and the S3 object.
* Expired recordings (`expires_at`) are deleted by an hourly sweep on every node that has
  recording enabled; erasing a user deletes their recordings first (see
  [Moderation and lifecycle](moderation.md#user-erasure-delete-v1usersuser_id)).

## Live audio streams

Besides files, a node can hand the audio of a channel to an external service **as it happens** —
your moderation/toxicity pipeline, a stream overlay, an archival or analytics sink. It is
provider-neutral: the node speaks a small WebSocket protocol and you bridge it to whatever you
run. Off by default; enable with `recording.live.enabled = true` (file recording may stay off).

Two transports, same frames:

* **Pull** — `GET /v1/channels/{channel_id}/audio/streams/pull[?format=opus|pcm_s16le&users=<id,id>&label=…]`
  with an API key (`audio_streams:write`) upgrades to a WebSocket; frames flow until you close it.
* **Push** — `POST /v1/channels/{channel_id}/audio/streams`
  `{"url":"wss://…","headers":{"Authorization":"…"},"format":…,"users":[…],"label":…}` makes the
  node dial your endpoint (custom headers are sent on the handshake, never echoed back),
  reconnect with exponential backoff up to `recording.live.max_reconnects` (a fresh `hello`
  after each reconnect) and give up with reason `push_unreachable`. `GET`/`DELETE
  …/audio/streams/{stream_id}` show status (`frames_sent`, `frames_dropped`, `reconnects`,
  `state`, per-participant consent) and stop it; header values are never returned. URLs with
  embedded credentials are refused; in production they must be `wss://` and public
  (`recording.live.require_tls` / `allow_private_urls` relax this for development).

`GET /v1/audio/streams` lists the streams of this application **on this node**.

### Wire format

The socket carries JSON text frames for control and binary frames for audio:

```json
{"type":"hello","stream_id":"…","channel_id":"…","format":"opus","sample_rate":48000,
 "channels":1,"frame_ms":20,"frame_version":1,"users":null,"consent_required":true}
{"type":"participant","user_id":"…","ssrc":123,"event":"audio_started|consent|left","consent":"accepted"}
{"type":"dropped","frames":12}
{"type":"end","reason":"consumer_disconnected|operator|duration_limit|channel_stopped|push_unreachable|shutdown","frames_sent":0,"frames_dropped":0}
```

Binary frame (36-byte header, big-endian, then the payload; `aurix_recording::live::decode_frame`
is the reference parser):

```text
 0  version (1)      1  codec (1 = Opus, 2 = PCM s16le)    2  flags (bit0 gap, bit1 first)   3  reserved
 4  ssrc (u32)       8  rtp timestamp (u32, 48 kHz)       12  server receive time (i64, unix ms)
20  participant user id (16 bytes, RFC 4122)             36  payload
```

`opus` forwards each participant's packets untouched (one 20 ms frame each; the `gap` flag
marks a hole in the RTP timeline so a decoder can run PLC); `pcm_s16le` decodes on the node —
mono 48 kHz, 960 samples per frame — with one Opus decoder per active talker
(`recording.live.allow_pcm = false` disables it). Streams are **per participant, not mixed**;
mixing, transcoding and container formats are your side's job.

### Semantics

* **Consent** follows file recording: with `recording.require_consent` every participant is
  `pending` until they answer the `RecordingNotification` (`live: true`) with
  `RecordingConsentResponse`; only `accepted` participants' frames leave the node, `declined`
  ones never do, and the consumer sees each decision as a `participant` control frame. This
  works across cascaded nodes (the decision is relayed to the node hosting the stream).
* **Streams are node-local.** Open them against a node that hosts participants of the channel
  (`409` otherwise); participants on other nodes of a cascaded channel are included through the
  relay. Behind a load balancer, pin the operator connection to one node or use push.
* **Backpressure never reaches players.** Each stream buffers `recording.live.queue_frames`
  frames; a slow consumer loses the oldest ones and gets a `dropped` count, the media path is not
  blocked. `max_per_channel` / `max_per_app` bound the number of streams per node,
  `max_duration_secs` (and always `recording.max_recording_duration_secs`) their length.
* **Lifecycle** is announced as `audio_stream.started|stopped` (webhooks/SSE, with the end
  reason and frame counters, without URLs or headers), written to the audit log, and shown to
  players like a recording (`RecordingNotification` with `live: true`). Streams end with the
  channel and on node shutdown (`end` frame); erasing a user drops their per-stream state
  (consent, decoder) while the stream itself keeps running.

The live E2E suite (`crates/aurix-server/tests/e2e_live.rs`) contains a complete pull and push
consumer in Rust you can copy as a starting point.
