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

## Voice effects

Every SDK can run the same library of **voice effects** on the microphone uplink — after noise
suppression / AEC / AGC and the input gain, before the VAD meter, the viseme analyser and the
encoder — so what peers hear, the level bars, the transcripts and the recordings all reflect
the effected voice, and the server never sees the raw one. Effects touch only the microphone:
injected audio, TTS, translations and the downlink are untouched, and the mono, stereo and
PCMU paths all see the processed frame.

The chain, in order, with one parameter set (`VoiceEffectParams` / `AurixVoiceEffects`;
`0` = stage off, everything clamped):

| Stage | Parameters | Range |
| --- | --- | --- |
| high-pass / low-pass filters | `highpass_hz`, `lowpass_hz` | 20–20 000 Hz |
| formant shift (vocal-tract size, pitch kept) | `formant_semitones` | ±12 |
| pitch shift | `pitch_semitones` | ±24 |
| ring modulation (robot) | `ring_mod_hz` | ≤ 2 000 Hz |
| distortion (soft clip) | `distortion_drive` | ≤ 20 |
| tremolo | `tremolo_hz`, `tremolo_depth` | ≤ 20 Hz, 0–1 |
| radio static | `static_level` | 0–1 |
| reverb | `reverb_mix`, `reverb_size`, `reverb_damping` | 0–1 each |

Presets — `robot`, `monster`, `radio`, `helium`, `ghost` — are fixed parameter sets you can
start from and tweak (`aurix_voice_effects_preset`, `voiceEffectPreset()`,
`VoiceEffectParams.Preset()`). Where it runs:

* **Native core** (Rust, C ABI, Unreal, Godot): `Client::set_voice_effects`,
  `aurix_client_set_voice_effects(&AurixVoiceEffects)`, `SetVoiceEffects`,
  `set_voice_effects(Dictionary)`; the host may still append its own stage with
  `aurix_client_set_voice_effect_callback` (capture thread, 20 ms 48 kHz frame — no
  blocking, no allocation). The processor is also exposed stand-alone
  (`aurix_voice_effects_create/process_f32`) for engines that own their capture.
* **Unity (native players)**: the C# SDK drives that stand-alone processor on its own
  capture path (`SetVoiceEffectsAsync`, the `VoiceEffect` field of `AurixVoiceBehaviour`);
  it is available wherever the native library ships (Windows, macOS, Linux, Android, iOS).
* **Browsers and Unity WebGL**: a pure-TypeScript port of the same chain runs in an
  `AudioWorklet` between the microphone and the encoder (`setVoiceEffects`,
  `AurixWebGLVoiceBehaviour.VoiceEffect`); it needs Web Audio worklets and reports
  `supportsVoiceEffects() == false` otherwise.

## Visemes (lip-sync)

Lip-sync is computed **on the receiver, from audio it plays anyway**: every decoded 20 ms frame
of a participant — and, for the local avatar, of your own processed microphone — is reduced to
a `VisemeFrame`: a weight per mouth-shape bucket (`sil PP FF SS aa E ih oh ou`, the
conventional lip-sync set: bilabial closure, labiodental, sibilant and five vowels), the
`dominant` bucket, `mouth_open` (0–1 from level), `energy`, `confidence` and a `sequence`
that advances per analysed frame. Nothing is sent to the server, no phoneme data crosses the
wire, and it works in E2EE channels because the analysis runs after decryption. The analysis
sees the audio *before* the receiver's volume, mute, panning and positional attenuation, so a
far-away or turned-down speaker still moves their mouth.

It is a signal-processing heuristic, not a phoneme recogniser: a spectrum per frame gives the
level, a voiced / fricative split and the first two formants, the vowels are the nearest of
five formant centroids, and the weights are smoothed (fast attack, slower release) so the mouth
does not flicker. Expect convincing openness and vowel motion, not text-accurate articulation.

* **Native core** (Rust, C ABI, Unreal, Godot): `Client::set_visemes(true)` /
  `aurix_client_set_visemes` / `SetVisemesEnabled` / `set_visemes_enabled` switches the
  analysis on for every heard stream and the microphone; poll `participant_visemes(user_id)`
  and `local_visemes()` each render frame (`aurix_client_participant_visemes`,
  `GetParticipantVisemes`, `get_participant_visemes`). A stand-alone analyser
  (`aurix_viseme_analyzer_create/push_f32/frame`) serves engines that decode elsewhere.
* **Unity (native players)**: `SetVisemesAsync`, `GetParticipantVisemes`, `GetLocalVisemes`
  on the client and the `AurixLipSync` component, which smooths frames and drives blend
  shapes (or `OnFrame` for custom rigs) for one participant or the local microphone.
* **Browsers and Unity WebGL**: the same analysis in a worklet on each dedicated
  per-participant track and on the microphone (`setVisemes(true)`, `participantVisemes` /
  `localVisemes` events); participants heard only through the mixed track cannot be
  separated and get no frames.
