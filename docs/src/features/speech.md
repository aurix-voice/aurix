# Transcripts and text-to-speech

Both features are off by default and point at **HTTP providers you run yourself** (no cloud
account is baked in): speech-to-text expects an OpenAI-compatible
`POST /v1/audio/transcriptions` (faster-whisper-server, whisper.cpp server, …) and
text-to-speech an OpenAI-compatible `POST /v1/audio/speech` returning WAV
(OpenedAI-Speech/Piper, Kokoro-FastAPI, …). Provider keys stay on the node: they never appear
in events, REST responses or logs. `cargo run -p aurix-server --example mock_speech` starts a
stand-in for both during development.

```toml
[stt]
enabled = false
endpoint = "http://stt.internal:8000/v1/audio/transcriptions"
# api_key, model, language
segment_secs = 3.0          # cut a segment after this much speech
silence_flush_ms = 700      # …or earlier after this much silence
min_segment_ms = 400        # drop shorter fragments
timeout_ms = 15000
max_concurrent_requests = 8
include_words = false       # forward word-level timings if the provider returns them

[tts]
enabled = false
endpoint = "http://tts.internal:8000/v1/audio/speech"
# api_key, model
voices = ["alloy"]
default_voice = "alloy"
allow_client_requests = true    # false = only POST /v1/channels/{id}/tts
max_text_chars = 500
max_audio_secs = 30             # longer synthesis is truncated
timeout_ms = 15000
max_concurrent_requests = 4
max_queued_per_session = 3
max_queued_per_channel = 8
requests_per_minute_per_session = 10
```

## Transcripts

A channel opts in with `"transcription": true` in its config; nothing else is ever sent to STT,
and neither are end-to-end-encrypted frames (the node cannot read them). The SFU decodes each
speaker's Opus, cuts segments of `stt.segment_secs` (earlier after `silence_flush_ms` of
silence, dropping anything shorter than `min_segment_ms`) and pushes the result as

```json
{"type":"Transcript","data":{"transcript":{"id":"…","channel_id":"…","user_id":"…",
 "text":"push left","language":"en","started_at":"…","duration_ms":1840,
 "words":[{"word":"push","start_ms":0,"end_ms":420}]}}}
```

`words` is present only when the node runs with `stt.include_words = true` and the provider
returns timings.

to the speaker and to the members of that channel who would hear them — local mutes, blocks
and zero gain suppress captions too — on every node. Clients opt out/in with
`SetTranscripts {enabled}` (Web `setTranscripts()`, Unity `SetTranscriptsAsync()`, native
`aurix_client_set_transcripts`); the `ChannelJoinAck.transcription` flag tells them whether a
channel is captioned. Game servers get the same segments as `channel.transcript`
(webhooks / SSE, only when explicitly subscribed). Transcripts are **ephemeral** — the server
stores nothing; keep them yourself if your policy requires it.

## Text-to-speech

A participant sends

```json
{"type":"TtsSpeak","data":{"channel_id":"…","text":"Regroup at B","voice":"alloy",
 "destination":"channel","client_ref":"tts-1"}}
```

(Web `speak()`, Unity `SpeakAsync()`, native `aurix_client_speak`). `destination` is

* `channel` — everyone their microphone would reach: same routing, mutes, blocks, focus and
  cascade as their voice;
* `local` — only themselves (accessibility read-out);
* `both`.

The node synthesizes, Opus-encodes and paces the audio in real time on a **synthetic SSRC** —
the participant's SSRC with the top bit set — so native receivers attribute it to the right user
while telling it apart from the microphone (native `aurix_client::is_synthesized_ssrc` /
`SYNTH_SSRC_FLAG`, Unity `IsSynthesizedSsrc`); browsers get it inside their mixed WebRTC
downlink like any other voice.

The requester alone receives `TtsStatus {request_id, client_ref, state, duration_ms?,
message?}` with `state` going `queued → playing → finished | cancelled | failed`, and can
`TtsCancel` everything still pending (Web `cancelSpeech()`, Unity `CancelSpeechAsync()`, native
`aurix_client_cancel_speech`); a closed connection cancels too. Text destined for a channel
runs through the chat content filter, server-muted participants cannot speak into a channel,
and `tts.max_text_chars`, `max_audio_secs`, per-session/per-channel queue depth and
`requests_per_minute_per_session` bound the cost. Provider failures reach the client as a
sanitized `failed` status; `allow_client_requests = false` rejects `TtsSpeak` entirely.

### Operator announcements

`POST /v1/channels/{channel_id}/tts` `{text, voice?}` (`tts:write`): every node hosting the
channel plays the announcement to its participants on a per-channel system SSRC (no participant
attached), and progress is published as `tts.status` events. `GET /v1/tts/voices` returns
`{enabled, client_requests, voices, max_text_chars, max_audio_secs}` so a client can build its
voice picker from the server's configuration.
