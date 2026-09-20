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
returns timings. Channels with `"safety_voice": true` are transcribed for the
[content-safety](safety.md) classifier only, whether or not they are captioned.

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

## Live translation

With `[translation]` enabled each listener picks the language they want captions in and the
node translates every captioned segment **once per requested language** through a
machine-translation server you run yourself — LibreTranslate-compatible `POST /translate`
(`provider = "libretranslate"`) or an OpenAI-compatible `POST /v1/chat/completions` with a
translation prompt (`provider = "openai_chat"`: vLLM, Ollama, llama.cpp, hosted LLMs). STT stays
the input, so `[stt]` must be on; the mock speech server also answers `/translate`.

```toml
[translation]
enabled = true
provider = "libretranslate"
endpoint = "http://libretranslate:5000"
# api_key, model (openai_chat)
timeout_ms = 10000
max_concurrent_requests = 8
max_text_chars = 2000
languages = ["en", "de", "fr", "pt-BR"]   # offer; empty = any BCP-47 tag
max_languages_per_channel = 8
cache_entries = 2048
speech = true                             # spoken translations through [tts]
[translation.voices]
de = "thorsten"
```

`SessionInitAck.translation` (`{speech, languages}`) tells a client what the node offers; it is
absent on nodes without translation. A listener then sends

```json
{"type":"SetTranslation","data":{"language":"de","spoken_language":"en","speech":true}}
```

(Web `setTranslation("de", {spokenLanguage, speech})`, Unity `SetTranslationAsync`, native
`aurix_client_set_translation`, Unreal `SetTranslation`). Tags are normalised (`DE_de` →
`de-de`), checked against the offer (`VALIDATION_ERROR` otherwise) and echoed back as
`TranslationChanged`; `language: null` stops translating, and `speech` without a target is
dropped. `spoken_language` is a hint for the transcriber when the provider cannot detect the
speaker's language. Preferences survive a resume, including on another node.

What each participant receives for a segment:

* the **speaker** and listeners without a target — or whose target is the segment's language —
  get the original `Transcript` immediately, before any translation starts;
* a listener with another target gets the same segment (same `id`, `user_id`, `started_at`,
  `duration_ms`) with `text` and `language` replaced and the source attached as
  `"original": {"text": "…", "language": "en"}`; word timings do not survive translation;
* a listener with `speech: true` additionally hears the translation spoken **privately** on the
  channel's translator SSRC (a synthetic SSRC distinct from the announcement voice), through the
  same `[tts]` provider, voice per language from `translation.voices`. Nobody else — not the
  speaker, not listeners of other languages — gets a frame, and no `TtsStatus` is emitted for it.

Translation never widens who hears whom: tenant, channel membership, transcript opt-in, local
mutes, blocks, zero gain, text reachability and radius / ambient visibility are evaluated
exactly as for the original caption, and re-checked after the provider round trip, so a listener
who muted the speaker meanwhile gets nothing. When the provider fails, times out, the segment is
longer than `max_text_chars` or the node's `max_concurrent_requests` are busy, the listener gets
the **original** transcript instead (never marked as translated); results are cached per node
(`cache_entries`), failures are not. With more distinct targets than
`max_languages_per_channel` in one channel the most-requested languages win and the rest fall
back to the original. `aurix_translations_total{outcome="ok"|"cached"|"error"|"busy"|"skipped"}`
and `aurix_translation_latency_seconds` count it all. Provider keys stay on the node; translated
text is as ephemeral as the transcripts it comes from.

## Voice effects (native SDK)

The native core (and thus Unreal) can run a chain of **voice effects** on the microphone uplink,
after noise suppression / AEC / AGC and input gain and before the VAD meter and the encoder, so
what peers hear, the level bars and the transcripts all reflect the effected voice. Built in:
`PitchShift` (±24 semitones), `RingModulator` (robot voice, up to 2 kHz) and `CallbackEffect`
for the host's own DSP (`aurix_client_set_voice_effect_callback`: called on the capture thread
with a 20 ms 48 kHz frame, mono or interleaved stereo — no blocking, no allocation). Effects
touch only the microphone: injected audio, TTS and the downlink are untouched, and the mono /
stereo / PCMU paths all see the processed frame. Rust `Client::set_voice_effects(EffectChain)`,
C `aurix_client_set_voice_effects(&AurixVoiceEffects)`, Unreal `SetVoiceEffects`. Unity and the
Web SDK run the engine's / browser's own audio graph and have no effect chain.
