# SDK cookbook: tokens per action, join order, channel naming, bandwidth, errors

Short recipes that answer the questions every integration asks in its first week. The
mechanics are described in [Client flow](../getting-started/client-flow.md), the
[REST API](../api/rest.md) and the [WebSocket control plane](../api/websocket.md); this page only
shows the shapes.

## One token per intent

Player tokens are minted by **your** backend with the SDK key (`POST /v1/tokens`), never by the
client. The `channels` list carries one grant per channel; what the player may do is the grant,
not a role name.

| Intent | Grant (one entry of `channels`) |
|---|---|
| Join and talk | `{ "channel_id": "…", "join": true, "speak": true, "receive": true }` |
| Listen only (spectator, muted on join) | `{ "channel_id": "…", "join": true, "speak": false, "receive": true }` |
| Priority speaker (referee, GM, host) | `{ "channel_id": "…", "join": true, "speak": true, "receive": true, "priority": true }` — everyone else is ducked while they talk (`ducking` on the channel) |
| Moderator | `{ "channel_id": "…", "join": true, "speak": true, "receive": true, "moderate": true }` — may `ModerateParticipant` (mute/kick) in that channel |
| Ad-hoc channel (party, squad, lobby) | `{ "ad_hoc": { "name": "party-…", … }, "join": true, "speak": true, "receive": true }` — the id is derived from the name, the channel exists while someone is in it |

```http
POST /v1/tokens
x-api-key: <SDK key>

{ "external_id": "player-4711", "display_name": "Mara",
  "channels": [
    { "channel_id": "2d4c…", "join": true, "speak": true, "receive": true },
    { "channel_id": "9a01…", "join": true, "speak": false, "receive": true }
  ],
  "region": "eu-central" }
```

→ `{ token, endpoint, … }`: the client connects to `endpoint` with `token`. One token covers
every channel the player may enter during the session; mint a new one (and call
`SetToken` / the token refresher) when the set changes.

### Action tokens: one-shot permissions

`POST /v1/tokens/action` mints a token for a single act, handed to the client or used by your
backend directly — the equivalent of "a signed instruction":

| `action` | Fields | Effect |
|---|---|---|
| `login` | `external_id`, `display_name` | sign in without channel grants (chat-only, late join) |
| `join` | `channel_id`, `speak`, `receive`, `moderate`, `priority` | enter one channel with exactly these rights (`speak: false` = listen-only / join-muted) |
| `kick` | `channel_id`, `target_user_id` | `ModerateParticipant { action: kick, token }` from the game client |
| `mute` / `unmute` | `channel_id`, `target_user_id` | server mute / unmute of one participant from the game client |

Server-side moderation (ban, mute-all, kick-all, priority, report) does not need tokens at
all: your backend calls `/v1/moderation/…` with the SDK key — see
[Moderation](../features/moderation.md).

## Join order and channel layout

* **Positional first, then group channels.** Join the world/proximity channel before the
  squad/party channels: the node attaches the positional downlink to the session once and
  the group channels layer on top. Leaving the positional channel last avoids a brief
  silence while the mix is rebuilt.
* **One transmission target.** With several channels joined, `SetTransmission` decides where
  the microphone goes (`Single(channel)`, `All`, `None`); use it for push-to-talk per channel
  instead of muting the device. When the transmitting channel is left, transmission falls
  back to `None` — set it again on the next join.
* **Focus for what matters.** `SetChannelFocus(channel)` lifts one channel in the mix and
  attenuates the others (team call over ambient proximity chatter).

### Channel naming for large games

Channel ids are UUIDs; what you choose is the **name** (and for ad-hoc channels the id follows
from it). Make it carry the shard so operations can split a title across nodes without
touching clients:

```
<shard>-<kind>-<key>      eu1-world-zone17   eu1-party-8f3a   na2-raid-42
```

Rules of thumb: one positional channel per map zone or server shard (the server mix has no
"large 3D" tier — the node capacity sets the ceiling), a party/squad channel per group, a
command channel for leaders joined with `receive: true, speak: <leader>`. Keep names free of
player-identifying data — they appear in logs, webhooks and the audit trail.

## Push-to-talk and the input meter

Both are built in; wire them in the inspector or in a few lines. Push-to-talk drives the real
mute (`SetMuted`), so the server, the roster flags and `IsMuted` agree, and a release delay keeps
the last word from being clipped. The meter is `0..1` from the last captured frame — draw it as a bar.

| SDK | Push-to-talk | Input meter |
| --- | --- | --- |
| Unity | `AurixVoiceBehaviour.PushToTalkKey` (legacy Input) or `PushToTalkAction` (Input System: key, gamepad…); `PushToTalkToggle`, `PushToTalkReleaseDelay` | `InputLevel` / `OnInputLevel` |
| Unreal | `SetMuted(false/true)` from an Input Action (Enhanced Input *Started*/*Completed*); the lobby sample's `Set Push To Talk Pressed` shows the pattern | `GetInputLevel()` / `GetInputEnergy()` (BlueprintPure; poll from Tick) |
| Godot | `push_to_talk_action` = an InputMap action (`push_to_talk_toggle`, `push_to_talk_release_delay`) | `get_input_level()` / `get_input_energy()` |
| Web | `attachPushToTalk(client, { keys: ["KeyV"], toggle: false, releaseDelayMs: 150 })` → `.press(bool)` for gamepad/on-screen buttons, `.dispose()` | `localVoiceActivity` level events |
| Native (C/C++) | `aurix_client_set_muted` from your input layer | `aurix_client_capture_level` (0 = loudest, 127 = silence) / `aurix_client_capture_energy` |

Open microphone instead: leave PTT unset and use the VAD gate (`GateOnVad` / `SetVadGate` /
`set_vad_gate_enabled`) so silence is not sent.

## Bandwidth per participant

Only speech is sent: with voice activity detection on (the default) a silent player costs
a keep-alive, not a stream. Figures are per talking participant, Opus 20 ms frames, before
transport overhead (~20–30 % on UDP/QUIC, more inside the WebSocket tunnel).

| Profile | Encoder bitrate | Uplink while talking | Notes |
|---|---|---|---|
| Narrowband voice (mobile data, G.711-class devices) | 12–16 kbit/s | ≈ 14–18 kbit/s | PCMU/PCMA fallback is 64 kbit/s flat — use only where Opus does not fit the CPU |
| Default voice | 24–32 kbit/s | ≈ 26–35 kbit/s | the SDK default; in-band FEC adds ~20–30 % when loss is reported |
| High-quality / stereo / music uplink | 48–96 kbit/s | ≈ 50–100 kbit/s | channel policy must allow stereo |

Downlink is **one mixed stream** per native client regardless of how many people talk
(server mix), or one track per pinned participant plus the mix for browsers that asked for
per-participant tracks. The node's `BitrateCommand` lowers the uplink when the receiver side
reports loss; `SetBitrate` / `SetComplexity` let the game pin a profile. Details and the
statistics to watch: [Network quality](../features/quality.md), [Codecs](../features/channels.md#codecs-opus-and-the-pcmu-fallback).

## Errors: what to do with them

Every SDK surfaces the same codes (REST `error.code`, WebSocket `Error { code, message }`,
native `aurix_last_error()`, Unity `AurixApiException`, Web `SdkError`). The table says what
the game should do, which is what matters at 2 a.m.

The table ships as code too, so you never re-type it: native `aurix_error_describe(code)` /
`aurix_error_retryable(code)`, Unity `AurixErrors.Describe` / `IsRetryable` / `IsPlayerFacing`,
Unreal `DescribeError` / `IsErrorRetryable` (Blueprint-pure), Web `describeError` /
`isErrorRetryable` / `isErrorPlayerFacing`.

| Code | Meaning | Do |
|---|---|---|
| `AUTH_FAILED`, `TOKEN_INVALID`, `TOKEN_EXPIRED`, `TOKEN_REUSED` | token missing, malformed, expired or replayed | mint a new token on your backend and reconnect (`SetToken` + the SDK's token refresher does this when configured) |
| `AUTH_DENIED`, `ACTION_TOKEN_REQUIRED` | the grant does not allow this (speak without `speak`, moderate without `moderate`, kick without an action token) | programming error in your token server — fix the grant, do not retry |
| `CHANNEL_NOT_FOUND`, `USER_NOT_FOUND`, `SESSION_NOT_FOUND`, `NOT_FOUND` | unknown channel / user / session / message | stale id after a leave or a resume with a fresh session — refresh the roster |
| `CHANNEL_FULL`, `CHANNEL_LIMIT_EXCEEDED`, `QUOTA_EXCEEDED` | channel or app at its participant ceiling (plan limit, spend-cap ladder) | show "voice full", retry with backoff; the console shows why |
| `USER_BANNED`, `USER_MUTED` | moderation decision | tell the player; no retry |
| `RATE_LIMIT_EXCEEDED` | too many requests / messages | back off; chat has its own anti-flood limits |
| `CONFLICT` | duplicate or contradictory request (already joined, ad-hoc name mismatch) | treat as success when idempotent, otherwise re-read state |
| `VALIDATION_ERROR` | malformed payload | programming error — log and fix |
| `E2EE_REQUIRED`, `E2EE_MIXED_CHANNELS` | the channel demands end-to-end encryption / E2EE and plaintext channels mixed in one session | enable E2EE in the client or split the sessions — configuration, not runtime |
| `MEDIA_NODE_UNAVAILABLE`, `TRANSPORT_ERROR`, `STUN_TURN_ERROR` | no media node / the media path could not be established | the SDK already tried QUIC → WebRTC → WebSocket tunnel; tell the player voice is unavailable on this network |
| `FailedToRecover` (SDK event) | reconnect attempts or the recovery deadline (30 s) exhausted | the client is `Failed`; call `connect()` again on the player's next action, not in a loop |
| `INTERNAL_ERROR`, `DB_ERROR`, `REDIS_ERROR`, `TIMEOUT` | node-side failure | retry once after a second; if it persists it is an incident, not your bug |

Rule: anything the **player** can fix (network, "voice full") gets a message; anything the
**developer** caused (`forbidden`, `invalid`) gets a log line and no retry; the SDK handles
the rest (reconnect, media fallback, token refresh) when you let it.

## Roster hints players actually read

- **No microphone**: when the built-in capture cannot open a device (none plugged in, permission
  denied, device in use) the SDK tells the server (`SetCaptureAvailable(false)`) and every member's
  roster entry gets `capture_unavailable` (Unity `Participant.CaptureUnavailable`, Unreal
  `bCaptureUnavailable`, Web `captureUnavailable`). Draw a crossed-out mic instead of letting
  teammates wonder why someone never talks. If you feed audio yourself, call `SetCaptureAvailable`
  when your pipeline fails or recovers.
- **You are muted**: `SpeakingWhileMuted` (Unity `AurixVoiceBehaviour.OnSpeakingWhileMuted`,
  Unreal `OnSpeakingWhileMuted`, Web `speakingWhileMuted`, Godot `speaking_while_muted`) fires when
  the local VAD detects speech while muted — show the hint for a few seconds, no more than once per
  burst. It needs the microphone to stay open while muted, so it does not fire with
  `ReleaseMicrophoneWhenMuted` / `bReleaseCaptureWhenMuted`.
- **Why they left**: `ParticipantLeft.reason` — `kicked`, `banned`, `timeout` (connection lost),
  `out_of_range` (positional roster radius, usually no toast), `channel_closed`, `replaced`, `left`.

## Reproduce a player's problem without an engine

`cargo run -p aurix-client --example voice_cli -- wss://voice.example.com/ws <token> [channel-uuid]`
gives you the native core as a console: `join`, `mute on`, `tx none`, `pos <ch> x y z`,
`play clip.wav [loop]` (a WAV is the microphone), `record out.wav` (what you would hear, 48 kHz
stereo), `who` (mute / speaking / priority / no-mic flags), `stats`, `session`, `reconnect`
(network-change simulation) and every event as it arrives (`--json` for machine-readable lines).
Two instances on two tokens are a complete two-player test of a channel, a region or a token
server — attach the `stats` output and the event log to a support ticket.
