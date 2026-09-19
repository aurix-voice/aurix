# Content safety

`[safety]` turns the transcripts and chat messages a node already sees into **incidents**,
a per-user **risk score** and optional **automatic mute/kick**, with the flagged text, the
surrounding chat and (for voice) an audio evidence clip kept for your trust & safety team.
Everything is off by default; nothing ships with a model — the classifier is an HTTP service you
run, or you use just the lexicon.

```toml
[safety]
enabled = true
incident_threshold = 0.7      # classifier / lexicon score that records an incident
risk_half_life_secs = 900     # how fast a user's risk decays
risk_elevated = 1.0           # risk levels used by auto_mute / auto_kick
risk_high = 2.5

[safety.classifier]
endpoint = "http://moderation:8000/v1/moderations"
format = "openai_moderation"  # or "aurix"
# api_key = "…"               # sent as Bearer; never logged or exposed
timeout_ms = 5000
max_concurrent_requests = 8

[safety.voice]                # transcripts of channels with "safety_voice": true (needs [stt])
evidence = true               # store the offending audio as a recording of kind "evidence"
evidence_pre_segments = 2
evidence_retention_days = 30
auto_mute = "never"           # never | incident | elevated | high
auto_kick = "never"

[safety.text]                 # chat: lexicon -> classifier -> chat.filter_webhook
lexicon_path = "configs/lexicon.example.toml"
block_threshold = 0.9
context_messages = 5
fail_open = true
auto_mute = "never"
auto_kick = "never"
```

`categories` (optional) restricts which classifier categories count — e.g.
`["harassment", "hate", "self-harm"]` — so a sports-chat deployment can ignore `profanity`.

## Classifier

Two response formats are understood:

* `openai_moderation` — `POST {"input": text, "model"?}` →
  `{"results":[{"flagged":bool,"categories":{…},"category_scores":{"harassment":0.93,…}}]}`,
  the shape served by OpenAI, Azure Content Safety shims, Detoxify/Perspective wrappers and
  llama-guard/`omni-moderation` compatible servers;
* `aurix` — `POST {"text","language","context":[…],"source":"voice"|"text"}` →
  `{"score":0.93,"categories":{"harassment":0.93},"labels":["harassment"],"flagged":true}` for
  your own model, with the preceding chat messages as context.

Input is capped at 4000 characters, responses at 1 MiB, redirects are refused and the key is
sent as `Authorization: Bearer`; the endpoint is validated like every outbound URL (no private
addresses in production unless allowed). A voice segment that cannot be classified is logged and
dropped; a chat message follows `text.fail_open` (deliver, the default) or is blocked. The mock
provider — `cargo run -p aurix-server --example mock_speech` — serves `/v1/moderations` too and
flags anything containing `hate` (0.95) or `rude` (0.75).

## Lexicon

The dictionary filter runs first, on a **normalized** copy of the message: case-folded, accents
stripped, leetspeak (`sh1t`, `$hit`) and Latin-lookalike Cyrillic letters mapped, zero-width
characters removed, spaced-out letters (`k.y.s`, `k y s`) re-joined, repeated letters collapsed. Rules live in a TOML file (`[[entries]]`)
or inline as `[[safety.text.lexicon]]`:

```toml
[[entries]]
pattern = "kys"            # matched as a whole word unless substring = true
action = "block"           # mask | block | flag
severity = 0.9             # incident score contributed (0 = filter only)
category = "self-harm"

[[entries]]
pattern = "fuck"
action = "mask"            # delivered as ****
severity = 0.3
category = "profanity"
substring = true
```

`mask` replaces the match with `mask_char`, `block` rejects the message
(`MESSAGE_BLOCKED`), `flag` only counts towards the score. The lexicon has no network dependency,
so it keeps working when the classifier is down.

## Voice

A channel opts in with `"safety_voice": true` in its config. Its speakers are transcribed by
the `[stt]` provider **even when `transcription` is off** — the text then goes to the classifier
only, never to the participants. End-to-end-encrypted frames are never analysed (the node cannot
read them). Players are told: `ChannelJoinAck.safety_voice` is `true` for a monitored channel
(Web `isChannelMonitored(channelId)`, Unity `IsChannelMonitored(channelId)`), so a game can show
the disclosure its policy requires.

With `voice.evidence = true` and `recording.enabled`, the offending segment plus the
`evidence_pre_segments` before it are written as an Ogg/Opus **recording of kind `evidence`**:
same encryption at rest, same object-storage mirror, same `GET /v1/recordings/{id}/download`
(`recordings:read`, audited as `recording.accessed`) and same retention sweep after
`evidence_retention_days`. Evidence is per speaker (nobody else's voice is in the clip) and is
never taken from channels without `safety_voice`.

## Text

Chat goes through `lexicon → classifier → chat.filter_webhook`; each stage receives the previous
stage's replacement, the first `block` wins. Above `incident_threshold` the message is still
delivered (masked when the lexicon said so) but recorded; at `block_threshold` it is rejected.
System/operator messages (`POST /v1/channels/{id}/messages`) bypass the pipeline. Text incidents
carry the last `context_messages` messages of the same channel (or between the same two users
for directed messages) as evidence; that context is kept in memory only until it is attached to
an incident, and chat history itself is still governed by `chat.persist`.

## Incidents, risk and automatic actions

An incident is a **moderation event** (`event_type` `safety.voice` / `safety.text`, status
`pending`) whose `evidence` holds the score, categories, flagged text, classifier and lexicon
verdicts, chat context and evidence-clip metadata — so the existing
`POST /v1/moderation/events/{id}/resolve` closes it and the retention sweep applies once resolved.

```text
GET /v1/safety/incidents?user_id=&source=voice|text&status=&page=&per_page=   moderation:read
GET /v1/safety/incidents/{incident_id}                                         moderation:read
GET /v1/safety/incidents/{incident_id}/export                                  moderation:read (+ recordings:read for audio)
GET /v1/safety/users/{user_id}/risk                                            moderation:read
```

`…/export` is the hand-off bundle: incident, context and — when the key also holds
`recordings:read` and the clip has not expired — the decrypted audio inline as
`audio.content_base64` (`audio/ogg`), otherwise only `audio.download_path`. Every incident and
recording belongs to one application; another tenant's key gets `404`.

**Risk** is the decayed sum of a user's incident scores (`risk_half_life_secs`), computed from
the stored incidents — identical on every node and after a restart. Levels: `none` (0), `low`
(> 0), `elevated` (≥ `risk_elevated`), `high` (≥ `risk_high`). `auto_mute` / `auto_kick` in
`[safety.voice]` and `[safety.text]` fire on `incident`, `elevated` or `high` through the regular
moderation primitives — a server mute (`participant.muted`, text blocked with `USER_MUTED`, audit
entry, moderator = nil user) or a kick from the channel — and the actions taken are listed in the
incident.

Game servers get `safety.incident` (user, channel, source, score, categories, text, classifier,
evidence recording id, resulting risk and actions) and `safety.risk_changed` (level transitions
in both directions) over [webhooks and SSE](../api/webhooks-sse.md); both are tenant-scoped like
every other event. Prometheus: `aurix_safety_checks_total{source,outcome}` (`clean`, `incident`,
`blocked`, `error`) and `aurix_safety_actions_total{action}` (`mute`, `kick`).

## What it is not

No model, no cloud account and no player-facing appeal flow are included; the classifier's
quality (and false positives) is yours. Voice safety costs one STT request per segment for every
speaker in a monitored channel — size the provider accordingly and prefer monitoring
specific channels (ranked, public) over everything.
