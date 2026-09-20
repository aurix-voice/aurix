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
  S3 is configured and the recording is `ready`, `null` for local storage — and the consent
  state. Every row carries `kind` (`recording` — a participant track, `evidence` — a
  [safety clip](safety.md), `mixdown` — see below) and `status` (`recording`, `processing`,
  `ready`, `failed` + `error`).
* `GET /v1/recordings/{recording_id}/download` streams the decrypted `audio/ogg` bytes from the
  node's disk (`audio/wav` for WAV mixdowns; `409` while the recording is still running or
  rendering) and writes a `recording_accessed` audit row.
* `DELETE /v1/recordings/{recording_id}` removes the row, the local file, the S3 object and
  the recording's transcript.
* Expired recordings (`expires_at`) are deleted by an hourly sweep on every node that has
  recording enabled; erasing a user deletes their tracks — and every mixdown rendered from
  them — first (see
  [Moderation and lifecycle](moderation.md#user-erasure-delete-v1usersuser_id)).

## Mixdowns and transcripts of stored recordings

Tracks stay per participant on disk; a channel-level file is a **derived** recording rendered
on demand. Both jobs below run on the node that accepts the request, off the media path (the
blocking thread pool, `recording.processing.max_concurrent` workers, `max_queued` waiting →
`429` beyond). State is in the database, so any node answers `GET`s; a node restart marks the
mixdowns it was rendering `failed` (request again) and re-queues its transcripts.

```toml
[recording.processing]
enabled = true
max_concurrent = 2       # decoders running at once on this node
max_queued = 64
max_sources = 64         # tracks per mixdown
mixdown_bitrate = 64000  # Ogg/Opus output
stt_chunk_secs = 30      # audio sent to the STT provider per request, cut on silence
stt_sample_rate = 16000  # 8000 | 12000 | 16000 | 24000 | 48000
```

### Mixdown

`POST /v1/recordings/mixdown` `{channel_id, sources?, format?: ogg_opus | wav, stereo?}`
(`recordings:write`) combines finished tracks (`kind = recording`, `status = ready`) of one
channel — the listed `sources`, or every finished track of the channel when omitted — into one
file and returns a new recording with `kind = mixdown`, `status = processing`, `sources = [...]`
and `user_id`/`session_id` set to the nil UUID. Tracks are aligned on their `audio_started_at`
(the moment the first packet was written, i.e. after consent), gaps inside a track stay silent
(the Ogg granule timeline is honoured), samples are summed as floats and soft-clipped, and the
result is written as Ogg/Opus at `mixdown_bitrate` or as 16-bit PCM WAV, mono or stereo
(stereo tracks keep their L/R, mono tracks are centred). When done the row flips to `ready`
(`duration_secs`, `file_size_bytes`), is encrypted and uploaded exactly like a track, expires
after `retention_days`, and a `recording.processed {job: "mixdown", status: "ready" | "failed",
error?}` event is published. Deleting a source track does not delete the mixdown; erasing a user
does.

The node must be able to read every source: its own file or, with S3 configured, the object.
Without object storage a track recorded on another node yields `409` naming that node
(`node_id` in the recording row) — send the request there.

### Transcript

`POST /v1/recordings/{recording_id}/transcribe` (`recordings:write`) queues speech-to-text
with the node's `[stt]` provider (the same one used for live transcription; `400
INVALID_CONFIG` without one). A participant track is transcribed as a single speaker; a
**mixdown is transcribed from its source tracks one by one**, so every segment carries the
speaker's `user_id` while offsets stay on the mixdown's timeline. Audio is decoded, resampled to
`stt_sample_rate` and sent in `stt_chunk_secs` chunks cut on silence (speech is not split
mid-word when a pause exists). `409` while a transcript is already `queued`/`running`; a
`failed` one can be requested again.

`GET /v1/recordings/{recording_id}/transcript` (`recordings:read`) returns

```json
{"recording_id":"…","status":"ready","provider":"whisper","language":"en",
 "text":"hello there general kenobi","duration_ms":4120,
 "segments":[{"speaker":"<user_id>","start_ms":0,"end_ms":1500,"text":"hello there",
              "language":"en","confidence":0.94,"words":[{"word":"hello","start_ms":0,"end_ms":410,"confidence":0.97}]}],
 "requested_at":"…","finished_at":"…","error":null}
```

`?format=srt` / `?format=vtt` render the segments as subtitles (speaker as a `<v uuid>` voice
tag) once `status = ready`. The transcript row is deleted with the recording, and
`recording.processed {job: "transcript"}` announces completion. Transcripts are stored in
PostgreSQL in clear text — treat the database like the recordings themselves.

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
