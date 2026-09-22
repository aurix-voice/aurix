# Channels and audio routing

A **channel** is the unit of routing: participants join it, the SFU decides who hears whom, and
moderation, recording and transcription are configured on it. Channels belong to an application
and are created with `POST /v1/channels` (or on the fly, see [ad-hoc channels](#ad-hoc-channels)).

## Channel types

| `channel_type` | Who hears whom |
| --- | --- |
| `team` (default) | everyone hears everyone at full volume — parties, squads, lobbies |
| `positional` | volume falls off with distance between the reported positions and, with `directional: true`, is panned by where the speaker stands relative to the listener; silent until both poses are known |
| `command` | only participants whose role can speak (`speaker`, `moderator`, `administrator`) or who are listed in `command_speakers` transmit; everyone hears — announcers, commanders, tournament casters |
| `whisper` | point-to-point: audio goes only to `whisper_target` (or, without one, to the first other participant) |
| `echo` | microphone test: every participant hears only their own audio, through the real uplink → server → downlink path; never relayed to other nodes |

Roles (`listener < speaker < moderator < administrator`) come from the membership; the JWT grant or
action token decides them at join time. A grant with `speak: false` makes the member a
**listener** in every channel type: its audio is dropped at the node, and
[audience settings](#large-channels-and-audiences) decide how such members are presented.

## Configuration

```json
{
  "name": "squad-42",
  "config": {
    "channel_type": "positional",
    "max_participants": 256,
    "codec": "opus",
    "bitrate": 48000,
    "min_bitrate": 12000,
    "sample_rate": 48000,
    "enable_dtx": true,
    "enable_fec": true,
    "max_bandwidth": "fullband",
    "complexity": null,
    "audio_profile": "voice",
    "stereo": false,
    "noise_suppression": false,
    "recording_enabled": false,
    "transcription": false,
    "safety_voice": false,
    "e2ee": false,
    "positional_config": {
      "near_distance": 1.0,
      "far_distance": 50.0,
      "rolloff": "logarithmic",
      "max_radius": 100.0,
      "directional": true,
      "coordinate_system": "left_handed",
      "roster_radius": 60.0,
      "text_radius": 20.0
    },
    "ambient": { "max_voices": 4, "ambient_gain": 0.15 },
    "audience": { "hide_listeners": true, "mix_for_listeners": true, "max_speakers": 0,
                  "speaker_admission": "reject", "demote_idle_ms": 3000, "max_streams": 0 },
    "ducking": { "gain": 0.25, "attack_ms": 60, "release_ms": 400, "hold_ms": 250, "moderators": false }
  }
}
```

* The Opus fields form the channel's **audio policy** — what everyone sending into the channel
  encodes with. `bitrate` is the uplink target and the ceiling server-driven adaptation returns
  to, `min_bitrate` its floor (`6000 ≤ min_bitrate ≤ bitrate ≤ media.max_bitrate`), `enable_fec`
  requires in-band FEC, `enable_dtx` allows silence suppression, `max_bandwidth`
  (`narrowband` 4 kHz … `fullband` 20 kHz) caps the encoded band, `complexity` (`0..=10`,
  `null` = client default) is a CPU/quality hint and `audio_profile` (`voice`, `music`,
  `broadcast`, `low_bandwidth`) selects the encoder's signal mode (`music` → `OPUS_SIGNAL_MUSIC`,
  `broadcast` → auto, otherwise voice). Invalid combinations are rejected with `400`.
* `stereo` (default `false`) lets participants send **two-channel Opus** — music, DJ and
  broadcast sources; see [Stereo and music uplinks](#stereo-and-music-uplinks). Off, every
  SDK encodes mono whatever the app asked for.
* `noise_suppression` (default `false`) makes the node **denoise every uplink into the
  channel** before anyone hears it; see [Server-side noise suppression](#server-side-noise-suppression).
  Rejected together with `stereo` (the model is mono speech) or `e2ee` (the node cannot decode
  the frames).
* `e2ee` (default `false`) makes the channel **end-to-end encrypted**: members seal their Opus
  frames with sender keys the node never sees, only sessions that announced the capability may
  join (`E2EE_REQUIRED`), and everything that needs the node to hear the audio (recording,
  transcription, safety, TTS/translation, the server mix, `ambient`, `echo`) is refused for the
  channel — see [End-to-end encryption](e2ee.md).
* The policy is delivered to participants as `ChannelJoinAck.audio` and, when an operator edits
  the channel, as `ChannelAudioPolicy` to everyone in it on every node (`channel.config_updated`
  event for your backend). A session in several channels applies the **merge**: the highest
  target/floor bitrate and widest bandwidth, FEC if any channel wants it, DTX only if all allow
  it, the highest complexity hint, `music` over `voice` over auto, stereo if any channel allows
  it. The server forwards native
  frames without transcoding, so the client's encoder is what actually goes on the wire;
  native/Unity/Unreal clients apply the policy to libopus (or Concentus) directly and browsers
  apply the WebRTC-controllable part (see [quality](quality.md) and the SDK chapters).
* When a client's `QualityReport` shows more than 10 % loss or 50 ms jitter the server sends
  `BitrateCommand` (32 kbit/s, 16 kbit/s above 20 % loss, clamped to
  `min_bitrate..=bitrate` of the merged policy) and lifts it again when the link recovers.
* `recording_enabled` allows `POST /v1/recordings/start` for the channel; `transcription`
  turns on STT (when the node has an `[stt]` provider); `safety_voice` sends the speakers'
  transcripts to the [content-safety](safety.md) classifier (disclosed to clients as
  `ChannelJoinAck.safety_voice`).
* `positional_config`: attenuation is 1.0 up to `near_distance`, follows `rolloff`
  (`linear`, `logarithmic`, `custom_spline`) to `far_distance`, and speakers beyond
  `max_radius` are not delivered at all. `coordinate_system` (`left_handed` — Unity/Unreal —
  or `right_handed`) tells the panner which way is left. The optional `roster_radius` and
  `text_radius` scope *presence* and *text* by distance — see
  [Radius-scoped presence and text](#radius-scoped-presence-and-text).
* `ambient` turns on [cocktail-party mixing](#ambient-cocktail-party-mode) for any channel
  type; absent (the default) every audible speaker arrives at full gain.
* `audience` configures [large channels](#large-channels-and-audiences): hidden listeners, a
  server mix for them, a speaker admission limit and a per-receiver cap on concurrent voices.
  `max_participants` may go up to the application's `max_participants_per_channel` quota
  (`PATCH /v1/apps/{app_id}` raises it; the hard ceiling is 100 000).
* `ducking` (absent by default) turns on [priority speakers](#priority-speakers-and-ducking):
  while a raid leader / shoutcaster / moderator talks, everybody else is attenuated to `gain`.

`PUT /v1/channels/{id}/config` updates the configuration live; participants on every node hosting
the channel pick it up (channel type changes take effect for subsequent frames).

## Codecs: Opus and the PCMU fallback

Every channel is Opus internally — `ChannelConfig.codec` only accepts `opus`, and that is what
recording, transcription, live streams, cascade and every WebRTC participant see. A **native
AURX session** may nevertheless run on **G.711** — **μ-law (PCMU)** or **A-law (PCMA)**: 8 kHz,
64 kbit/s, a lookup table instead of an Opus encoder/decoder — for devices where Opus does not
fit the CPU budget.

* The client sends `SetAudioCodec { codec: "pcmu" | "pcma" }` over the control connection and
  waits for `AudioCodecChanged { codec }`; from then on its uplink frames are G.711 of
  80/160/320/480 bytes (10/20/40/60 ms) flagged `Pcmu` / `Pcma` and its downlink arrives in the
  same law with the same flag. `SetAudioCodec { codec: "opus" }` switches back; the ack decides
  which codec the next frames use. The negotiated codec is replayed in
  `ReceiverPreferences.codec` after a resume or a cross-node failover; a fresh session starts on
  Opus and the SDKs send `SetAudioCodec` again.
* In plaintext channels the node transcodes **at the edge**: a G.711 uplink is decoded and
  encoded to narrowband Opus before it enters the channel, and the Opus a G.711 receiver would
  get is converted to its law after mutes, blocks, per-participant volume, focus, positional
  attenuation and direction have been applied — the gain/direction bytes and the per-receiver
  seal are exactly as for Opus. Opus participants in the same channel notice nothing; a G.711
  listener hears narrowband audio. A PCMU and a PCMA session in one channel each get their own
  law.
* In **end-to-end encrypted** channels the node cannot transcode what it cannot decrypt, so a
  `Pcmu | E2ee` / `Pcma | E2ee` frame is relayed sealed exactly like E2EE Opus, codec flag
  included: every member decodes the G.711 frame itself after opening it (all SDKs do, browsers
  over WebTransport too — `Pcmu`/`Pcma` E2EE frames are the one case where a browser receives
  G.711). A G.711 device in an E2EE channel is heard narrowband by everybody, and hears Opus
  speakers only if it can decode Opus — the fallback saves the *uplink* encoder there, not the
  decoders.
* Not available to WebRTC sessions (`CODEC_NOT_AVAILABLE`; browsers negotiate Opus in SDP).
  G.711 frames from a session that did not negotiate, or in the other law, are dropped.
* `media.pcmu_fallback = false` refuses the negotiation (both laws) node-wide. Cost per plaintext
  G.711 session: one Opus encoder plus one Opus decoder per speaker it hears, on the node; E2EE
  G.711 sessions cost nothing extra. Metrics: `aurix_g711_sessions{codec}`,
  `aurix_g711_frames_total{codec,direction,outcome}`.

SDKs: Unity `client.SetAudioCodecAsync(AudioCodec.Pcmu)` / `AurixVoiceBehaviour.PreferredCodec`,
native `aurix_client_set_audio_codec`, Unreal `SetAudioCodec` ([overview](../sdk/overview.md)).

## Server-side noise suppression

Capture DSP belongs on the client — the native core, Unity and Unreal run an RNNoise-derived
suppressor next to AEC/AGC, browsers get the `getUserMedia` one — and the node forwards Opus
without decoding it. For clients that cannot run DSP (PCMU devices, embedded boards, a bot
piping raw microphone audio) the node can do it **as an option**: `media.noise_suppression`
runs the same RNNoise-class model (`nnnoiseless`, pure Rust) over a session's uplink before the
frame enters the channel, so receivers, recording, transcription, the server mixes and the
cascade all get the cleaned audio.

```toml
[media.noise_suppression]
enabled = false     # off by default: the node advertises it in SessionInitAck.noise_suppression
level = "high"      # low | moderate | high — how much of the model's output replaces the input
max_sessions = 256  # sessions cleaned at once per node (bounds CPU)
bitrate = 32000     # Opus bitrate of the re-encoded uplink (6000–128000)
```

* **Who asks.** A client sends `SetNoiseSuppression { enabled }` over the control connection
  and gets `NoiseSuppressionChanged { enabled }` when the node holds a slot for it, or
  `NOISE_SUPPRESSION_UNAVAILABLE` when the node runs `enabled = false` or all `max_sessions`
  are busy. The preference survives resume and failover (`ReceiverPreferences.noise_suppression`)
  and is replayed by the SDKs after a reconnect. Alternatively a channel with
  `noise_suppression: true` makes the node clean *every* participant's uplink into it without
  a client request; when the node cannot (disabled or full) those frames pass **uncleaned** and
  are counted as `outcome="skipped"` — a channel requirement never mutes anyone.
* **What is cleaned.** An Opus uplink is decoded to 48 kHz mono, run through the model in 10 ms
  blocks and re-encoded with the node's encoder — same frame duration, `bitrate`, in-band FEC on;
  2.5/5 ms and TOC-only (DTX) packets pass through. A PCMU uplink is cleaned inside its
  transcode, where the μ-law samples are already PCM (8 → 48 kHz and back). A native client in
  several channels sends each frame once per channel; the node cleans it once and reuses the
  result for the copies. **Never cleaned:** `E2ee` frames (the node cannot decode them; a
  session that asked for suppression in an encrypted channel simply keeps sending sealed
  frames) and frames into `stereo` channels (the model is mono speech — music goes through
  untouched). Levels: `high` replaces the input with the model output, `moderate` mixes 75 %,
  `low` 50 % (keeps room tone for players who dislike the "processed" sound).
* **Cost.** One Opus decode, one model pass (~1 % of a core) and one Opus encode per frame of
  every cleaned Opus session; a PCMU session only adds the model pass to its existing
  transcode. `max_sessions` caps it per node; `aurix_noise_suppression_sessions` and
  `aurix_noise_suppression_frames_total{path=opus|g711, outcome=ok|repeated|passthrough|skipped|error}`
  show what it is doing. The re-encode is a second lossy pass (`bitrate` is the quality knob),
  which is why it is off by default and per-session opt-in.

SDKs: Web `client.setServerNoiseSuppression(true)` / `serverNoiseSuppression` /
`serverNoiseSuppressionChanged`, Unity `SetServerNoiseSuppressionAsync` / `ServerNoiseSuppression`
/ `OnServerNoiseSuppressionChanged`, native `aurix_client_set_server_noise_suppression` /
`aurix_client_server_noise_suppression` (event `ServerNoiseSuppressionChanged`), Unreal
`SetServerNoiseSuppression` / `IsServerNoiseSuppressionEnabled`, Godot
`set_server_noise_suppression` / `get_server_noise_suppression` /
`server_noise_suppression_changed`; `SessionInfo.noise_suppression` tells whether the node
offers it.

## Stereo and music uplinks

Voice channels are mono end to end. A channel with `"stereo": true` (typically together with
`"audio_profile": "music"` and a higher `bitrate`) lets a sender encode **two channels**: a
music bot, a DJ deck, a broadcast feed, a stereo microphone pair.

* **Opt-in twice.** The channel policy allows stereo (`audio.stereo: true` in the join ack /
  `ChannelAudioPolicy`); the client asks for it (native `EncoderSettings.channels = 2`, Unity
  `OpusEncoderSettings.Channels = 2` / behaviour `Stereo`, Unreal `bStereo`, Web
  `opus: { stereo: true }`). Either side alone yields mono: a stereo-configured client that
  joins a voice channel is forced to one channel by the policy, and a mono client in a stereo
  channel keeps sending mono. Clients that opted out of following the policy
  (`follow_channel_policy = false`) decide alone. Stereo raises the encoder's bitrate ceiling
  to 510 kbit/s (300 mono); `music` maps to `OPUS_APPLICATION_AUDIO`.
* **Nothing to negotiate on the wire.** An Opus packet declares its channel count in its
  first byte (RFC 6716 TOC bit `s`), so the SFU forwards stereo and mono frames alike and every
  receiver decides per packet: native/Unity mixers upgrade a stream to a stereo decoder on its
  first stereo packet (and keep it — a stereo decoder plays later mono packets upmixed), keep
  the L/R image for non-positional senders, **downmix before panning** when the sender has a
  direction, and average L/R for mono outputs. Mono-only receivers (older SDKs, PCMU edges,
  the server's own decoders) simply get libopus' downmix — recording, transcription, content
  safety and live PCM taps run mono decoders and need no change. WebRTC receivers keep
  getting the server-mixed stereo downlink.
* **Captured, not processed.** The client DSP (AEC / NS / AGC) is a voice chain and is bypassed
  for stereo frames; input gain, VAD and energy reporting still apply (on the L/R average).
  Native and Unity keep the first two capture channels as L/R and duplicate a mono device;
  browsers are asked for a two-channel track with echo cancellation, noise suppression and
  auto-gain off (`defaultAudioConstraints`) because their voice processing downmixes.
* **PCMU stays mono** (G.711 has one channel); a PCMU session in a stereo channel hears the
  downmix and sends mono.
* **Recordings** started while the channel is stereo are written with a 2-channel `OpusHead`
  (mono packets inside decode fine); recordings of mono channels stay 1-channel.

## Multiple channels per session

A session can be a member of up to `media.max_channels_per_session` (10) channels, at most
`media.max_positional_channels_per_session` (1) of them positional; the limits are checked
atomically on join (`409 CHANNEL_LIMIT_EXCEEDED`). Two knobs decide how one microphone maps to
several channels:

* **Transmission mode** (`SetTransmission`): `all` (default — talk into every joined channel
  where the role allows), `single` with a `channel_id`, or `none` (listen only). Leaving the
  target channel resets to `all`; the server confirms with `TransmissionChanged`. A WebRTC frame
  shared by several channels reaches a receiver exactly once — through the channel with the
  highest gain — so a squad mate in both your party and your positional channel is not doubled.
* **Channel focus** (`SetChannelFocus`): a receiver-local choice; every other channel is
  attenuated by `media.unfocused_channel_gain` (0.5). Leaving the focused channel clears it
  (`ChannelFocusChanged`).

## Receiver-side controls

Everything below is applied **on the server before the frame is sent**, so a muted participant's
audio never reaches the receiver's network and works identically for native, WebRTC and cascaded
senders:

| Control | Message / API | Persistence |
| --- | --- | --- |
| mute one participant, in one channel or everywhere | `SetParticipantMute` (`channel_id` optional) | session lifetime, replayed after resume |
| participant volume 0–2× | `SetParticipantVolume` | session lifetime, replayed after resume |
| mutual block | `SetUserBlock` / `POST /v1/users/{id}/blocks` | persistent per application (`user_blocks`); both directions of voice **and** text |
| own microphone | `MuteStateChanged` | broadcast to the channel as roster state |

Server-side (moderator) mutes and bans are covered in [moderation](moderation.md).

## Priority speakers and ducking

A channel with `config.ducking` has **priority speakers**: while any of them is audible, every
other voice in the channel is attenuated for every receiver. Who counts:

* members whose channel grant carries `priority: true` (`POST /v1/tokens` →
  `channels[].priority`; requires `speak`) — the raid leader, the shoutcaster, the commander;
* members a moderator promoted at runtime — `SetPriority { channel_id, user_id, priority }`
  over the control plane or `POST /v1/moderation/priority` from your backend
  (`moderation:write`), both audited and both answered to the whole channel with
  `PriorityChanged`; a granted member may toggle *themselves* off and on again (to chat
  without ducking the raid for a moment), nobody else can self-promote (`AUTH_DENIED`);
* with `ducking.moderators: true`, every `moderator` / `administrator` as well.

The flag is per membership: it is disclosed as `ParticipantBrief.is_priority` in rosters and
`ChannelJoinAck.priority` for yourself, survives resume and cross-node failover, and is
cleared when the member leaves. `SetPriority` in a channel without `ducking` is a
`VALIDATION_ERROR`; the REST toggle stores the flag regardless (so a grant can be prepared
before ducking is switched on with `PUT /v1/channels/{id}/config`).

The **envelope** is the channel's: a priority speaker's audible frame (labelled with an energy
level above the speaking threshold, or — unlabelled — while the node considers them speaking)
ramps every non-priority voice down to `gain` over `attack_ms`, keeps it there for `hold_ms`
past their last audible frame so pauses between words do not pump the mix, and ramps back over
`release_ms`. Several priority speakers at once simply keep it engaged; a priority speaker is
**never** attenuated — not by their own speech and not by another priority speaker's.

Where it is applied:

* **On the node**, for everything the node delivers with a gain — native per-participant
  streams (as the same `VolumeAttenuated` byte that carries participant volume), the server
  mix for listeners, cascaded frames. The duck multiplies with the receiver's own participant
  volume, after local mute / block / positional attenuation (a muted voice stays muted, a
  voice at `0.5` ends up at `0.125`) and before ambient gating and the `max_streams` ranking,
  so a ducked voice competes for a slot at the level it is actually heard.
* **In the browser**, for dedicated WebRTC tracks the node forwards unchanged
  ([per-participant tracks](#per-participant-tracks-for-browsers)): the Web SDK reproduces the
  same envelope on its per-participant `GainNode`s from the priority members' speaking state
  and the `ducking` it received in `ChannelJoinAck` / `ChannelAudioPolicy`. The mixed track is
  ducked by the node like any other mix.
* **In the game**: every SDK raises a *ducking changed* event (`duckingChanged`,
  `OnDuckingChanged`, `AURIX_EVENT_DUCKING_CHANGED`) exactly on the transitions — with the
  channel's `DuckingConfig` — so music and SFX can follow the same curve; Unity ships a
  ready-made `AurixGameAudioDucker`. Your own priority speech never ducks your own game
  audio or downlink.

Ducking is a channel-level effect that does not touch the receiver's private controls: local
mute, volume, block, focus and positional attenuation keep working underneath it. It works in
[E2EE channels](e2ee.md) as well: the node drives it from the sender-reported level byte, which
stays outside the ciphertext, and applies it through the gain byte; browsers duck their
per-participant tracks locally as everywhere else.

## Positional audio

Clients send `PositionUpdate {channel_id, position, orientation: {forward, up}}` at a modest
rate (10 Hz is plenty). For each speaker/listener pair the SFU computes the distance
attenuation and, for directional channels, the azimuth/elevation of the speaker relative to the
listener's forward/up vectors. Native receivers get the gain in the per-frame volume byte
(`VolumeAttenuated`) and the direction in two extra bytes (`Directional`), and pan locally;
WebRTC receivers get a server-mixed stereo downlink (`sprop-stereo=1`) plus, when they negotiate
[per-participant tracks](#per-participant-tracks-for-browsers), the nearest speakers as
separate tracks they attenuate and pan themselves (Web Audio HRTF). `OcclusionUpdate`
(a listener-side 0–1 factor towards one speaker, validated and echoed to the listener's own
client) and `ReverbZoneUpdate` (broadcast to the channel) are relayed as hints for client-side
DSP; the server does not process audio for them.

## Radius-scoped presence and text

A positional channel can hold a whole shard, yet a player only cares about the people around
them. Two optional radii in `positional_config` (metres, in the game's units) scope what the
server tells each member, independently of the audio range (`max_radius`):

| Field | Scopes | Without it |
| --- | --- | --- |
| `roster_radius` | `ChannelJoinAck.participants`, `ParticipantJoined` / `ParticipantLeft`, `PositionUpdate`, `MuteStateChanged`, `SpeakingStateChanged`, `ChannelEnergy` | the whole channel, as for any other channel type |
| `text_radius` | channel chat (`ChatSend`), `ParticipantTyping`, `Transcript` | the whole channel (subject to the usual blocks / mutes / opt-ins) |

* **Unknown positions hide.** With a radius configured, a member appears to another only once
  *both* positions are known — a fresh joiner gets an empty roster and is invisible until their
  first `PositionUpdate`. Nothing about presence is ever leaked beyond the radius, so a client
  cannot enumerate a shard by joining.
* **Entering and leaving the radius is presence.** When two members come within
  `roster_radius` of each other, each receives `ParticipantJoined` for the other (with
  `role` and `is_muted`, so the roster entry is complete); when they part they receive
  `ParticipantLeft`. Leaving uses a 10 % wider radius (`ROSTER_EXIT_FACTOR`) so two players
  dancing on the edge do not flicker in and out. Leaving the channel or disconnecting notifies
  only the observers who currently see the member — exactly once.
* **Text has its own range.** `text_radius` is typically smaller than the roster radius
  ("say" versus "who is here"); the sender always receives their own echo, so a chat UI can
  confirm delivery even when nobody was in earshot. `ChatSendDirect` is unaffected.
* **Audio range stays `max_radius`.** Choose `roster_radius ≥ max_radius` so every voice you
  can hear belongs to someone on your roster; a smaller roster radius is legal (the E2E test
  uses one) and simply delivers frames from SSRCs the client has not been introduced to —
  SDKs keep such streams playable but unnamed.
* Members on other nodes are scoped the same way: nodes exchange positions on the event bus
  and each node evaluates visibility for its own sessions. The radii are announced to clients
  in `ChannelJoinAck.roster_radius` / `text_radius` (Web `channelScope()`, Unity
  `GetChannelScope`, native `channel_scope` / `aurix_client_channel_scope`, Unreal
  `GetChannelScope`).

## Ambient (cocktail-party) mode

`"ambient": {"max_voices": 4, "ambient_gain": 0.15}` keeps a crowded channel intelligible: per
receiver, the speakers with a frame in the last 400 ms are ranked by how loud they would arrive —
the delivery gain after distance attenuation, per-participant volume and focus, multiplied by the
level the sender reported for the frame (RFC 6464; unlabelled frames rank at nominal loudness) —
the loudest `max_voices` keep their full gain and every other voice is attenuated to
`ambient_gain` (a murmur rather than silence). Slot holders are sticky (a challenger must be
20 % louder to take a slot) and a speaker who pauses frees the slot after the hold time, so DTX
gaps do not reshuffle the mix. Ranking is per receiver: your own mutes, blocks and volumes
decide who competes for *your* slots, and a speaker you muted never occupies one. Directional
metadata, E2EE payloads and the PCMU re-encode are untouched — only the gain byte / WebRTC
mixer gain changes. Speakers relayed from another node carry their reported level inside the
cascade envelope, so a remote shout wins a slot exactly as a local one. Defaults when the
object is present but partial: `max_voices` 4, `ambient_gain` 0.15; `max_voices` is clamped to
at least 1 and `ambient_gain` must be `0.0..=1.0`.

## Ad-hoc channels

A grant `{"ad_hoc": {"name": "…", "channel_type": "team", "max_participants": 8}}` in the
session JWT or the `join` action token yields a deterministic per-application `channel_id`. The
first join creates the channel (or revives a soft-deleted one) under a row lock, so concurrent
first joiners converge on one row; the last leave soft-deletes it (`channel.deactivated`,
`channel.destroyed`). Application channel quotas and `max_participants` apply as usual, and the
creating join is rolled back if it fails half-way. Use this for parties and matches your backend
does not want to pre-create.

## Large channels and audiences

A channel with thousands of members costs what its *speakers* cost, not its headcount, when
three things hold: most members are listeners, listeners do not flood presence, and every
receiver hears a bounded number of voices. `ChannelConfig.audience` controls all three:

```json
"audience": { "hide_listeners": true, "mix_for_listeners": true, "max_speakers": 8,
              "speaker_admission": "demote", "demote_idle_ms": 3000, "max_streams": 4 }
```

* **Listeners.** A member whose grant has `speak: false` joins with `ChannelRole::Listener`
  (`ChannelJoinAck.role = "listener"`). The node drops any audio it sends and it never counts
  towards `max_speakers`; it hears speakers like anyone else, may chat, and receives the same
  `ChannelJoinAck.audio` policy. SDKs expose it as `canSpeakIn` / `CanSpeakIn`.
* **`hide_listeners`** (default `true` when the block is present) keeps listeners out of the
  roster and out of `ParticipantJoined` / `ParticipantLeft` / mute / energy notifications sent to
  other members, so a 5 000-listener stream does not cost 5 000 × 5 000 presence messages.
  Listeners still see the speakers (and get their own join ack); `participant_count` in the
  ack carries the real headcount across all nodes and `hidden_listeners: true` tells the client
  the roster is partial. Game servers see the full membership over REST as usual.
* **`mix_for_listeners`** (default `true`) serves native listeners one server-mixed stereo
  stream per channel instead of a stream per speaker, whatever downlink mode they asked for —
  see [server mix](#server-mix-for-native-clients). Browsers always have the mixed track and
  hear at most `media.webrtc_participant_streams` speakers on
  [dedicated tracks](#per-participant-tracks-for-browsers) next to it.
* **`max_speakers`** (`0` = no separate limit) caps how many members that *may* speak the
  channel lets speak at once; listeners keep joining up to `max_participants`. Counted over
  the members a node knows of (its own plus those learned through the cascade), so the cap is
  approximate across nodes for a few hundred milliseconds after a join. What happens to a
  joiner with a speaking grant once every slot is held is **`speaker_admission`**:
  * `reject` (default) — the join fails with `CHANNEL_FULL`.
  * `wait` — the member joins as an **effective listener**: `ChannelJoinAck.role = "listener"`
    and `waiting_to_speak: true`, its audio is dropped, and it gets its granted role back
    (`RoleChanged { reason: "speaker_admitted" }`) as soon as a slot frees — when a speaker
    leaves (on any node), is demoted, or the cap is raised. Waiting members are served
    priority speakers and moderators first, then in order of arrival.
  * `demote` — like `wait`, but slots also rotate by activity: while a waiting member is
    trying to speak (the node keeps dropping its frames), a **plain speaker that has been
    silent for `demote_idle_ms`** (default 3 s, since joining if it never spoke) yields its
    slot — the longest silent first, the quietest among equals. A joining **priority speaker,
    moderator or administrator** takes the least recently active plain speaker's slot at once.
    Priority speakers, moderators and administrators are never demoted themselves.

  Demotion changes only the *effective* role: the membership's persisted grant stays what the
  token / REST grant said (`waiting_to_speak` on the roster row says which members hold a grant
  without a slot), so a demoted speaker is admitted again automatically, and a moderator that
  is waiting still moderates. The member itself and everyone who sees it get `RoleChanged`
  (`participant.role_changed` for webhooks / SSE); in channels with `hide_listeners` the
  others see a demotion as `ParticipantLeft` and an admission as `ParticipantJoined`, since a
  waiting member is hidden like any listener. SDKs surface it as `waitingToSpeak` /
  `IsWaitingToSpeak` and a `participantRoleChanged` event.
* **`max_streams`** (`0` = unlimited) bounds the concurrent voices *one receiver* hears. The
  ranking is per receiver and uses what that receiver would actually hear — delivery gain
  after local mute, block, volume, focus, distance attenuation and ambient dimming, times the
  level the sender reported (RFC 6464) — with the same sticky slots and hold time as the
  ambient mixer, so a burst of DTX does not reshuffle who you hear. Losers are withheld
  (`aurix_streams_capped_total`), not dimmed. The cap runs before per-speaker delivery *and*
  before the server mix, so it bounds what a native client decodes, what a browser mix decodes
  and what a channel mixer decodes alike.

### Server mix for native clients

Native AURX sessions normally receive one stream per speaker and mix locally — that is what
gives them per-participant positioning and E2EE. In large channels they can instead ask for
one **server-mixed stereo Opus** stream per channel with `SetDownlinkMode { mode: "mixed" }`
(ack `DownlinkModeChanged`; `media.downlink_mix` must be on — `SessionInitAck.downlink_mix`
says so — and the session must be native, browsers are mixed anyway). The mix:

* is built **per receiver rule set**: your local mutes, volumes, focus, positional
  attenuation / direction and ambient slots are applied before summing, exactly as they would
  be on per-speaker streams. Receivers of a `team` / `command` channel without ambient mixing
  and without receiver-specific preferences share one mixer per channel (one decode per
  speaker, one encode per channel); anyone with a local mute / volume / focus, and every
  receiver of positional, whisper or ambient channels, gets a private mixer (one encode each,
  bounded by `MAX_MIXERS` per node, idle mixers are torn down after 10 s);
* arrives under a stable per-channel synthetic SSRC (top bit set, distinct from the channel's
  TTS announcement SSRC) with `PacketFlags::Mixed`, its own sequence counter per receiver, and
  is sealed with the receiver's session keys like every downlink frame. Mixed frames never
  carry `Directional` (panning is baked in) and are re-encoded as μ-law for sessions that
  negotiated [PCMU](#codecs-opus-and-the-pcmu-fallback) (`Mixed | Pcmu`);
* **excludes `E2ee` speakers** — frames the server cannot decode keep arriving as separate
  streams next to the mix, so end-to-end encrypted whispers still work in a mixed channel;
* leaves recording, live streams, transcription, safety and the cascade untouched: they tap
  the canonical per-speaker frames before fan-out, never the mix.

Metrics: `aurix_downlink_mixers{kind="shared"|"private"}`,
`aurix_downlink_mix_frames_total{outcome}`. A CPU-bound node can set
`media.downlink_mix = false`; native clients then receive per-speaker streams (their request is
refused with `VALIDATION_ERROR`, as it is for a WebRTC session) and `mix_for_listeners` has no
effect on that node.

### Per-participant tracks for browsers

A browser's WebRTC session always has one **mixed** downlink track — the server mix above,
built with that receiver's rules, so any browser on any node hears everyone. On top of it a
browser may offer extra `recvonly` audio m-lines in its SDP; the node accepts up to
`media.webrtc_participant_streams` of them (default 16, at most 64; advertised as
`SessionInitAck.webrtc_participant_streams`, `0` = mixed only) as **per-participant tracks**:

* each track carries **one speaker's own Opus frames, forwarded as-is** (no decode/re-encode,
  the sender's timestamp cadence is preserved, a talkspurt starts with an RTP marker) — the
  browser decodes, attenuates, pans (Web Audio `PannerNode`, HRTF) and mixes them itself, which
  is what gives a browser per-speaker positioning the server mix cannot: binaural
  direction instead of stereo panning, and gains that follow the listener's own head frame
  between server updates;
* the mapping `mid → user_id` is pushed as `ParticipantStreams` (the full layout; once the tracks
  are negotiated and whenever it changes) — a track that changes hands changes the layout, an idle
  track has `user_id: null` and carries nothing;
* slots are **bounded and sticky**: a speaker keeps its track while audible and for a short
  hold after going quiet (1 s) before another speaker may take it, and a silent slot is
  released after 30 s; `SetParticipantStreams { pinned }` names users that keep a track while
  audible whatever the ranking (at most the cap, else `VALIDATION_ERROR`), a pinned speaker may
  displace an unpinned one;
* **everyone the browser may hear is still in the mixed track except the speakers currently on
  a dedicated track** — so a channel with more speakers than tracks degrades to "the N most
  recent/pinned voices spatialized, the rest mixed", never to silence, and a browser or node
  without the feature (older SDK, `0` tracks, no `AudioContext`) is exactly the mixed-only
  browser of before. The server-side receiver rules (mute, block, volume, focus, `max_streams`,
  radius visibility, positional attenuation and range) decide *whether* a frame reaches the
  browser at all on either path; the browser re-applies gain/attenuation locally on dedicated
  tracks since those frames carry no volume byte;
* **ambient channels keep everyone in the mixed track**: their per-slot ranking and dimming
  are server-only state the browser cannot reproduce, so dedicated tracks are not handed out
  there;
* recording, transcription, safety and the cascade are untouched (they tap frames before
  fan-out); [end-to-end encrypted](e2ee.md) speakers reach browsers **only** on dedicated
  tracks (the node cannot decode them for the mix), so a browser with `participantStreams: 0`
  hears nothing in an encrypted channel.

Cost: no transcoding, one RTP stream and one SRTP context per track per browser; a node caps the
count, a browser can ask for fewer (`participantStreams` in the Web SDK / Unity WebGL options).

## Speaking, energy and roster

* `SpeakingStateChanged` is emitted per participant with a hangover of
  `media.speaking_timeout_ms` (400). For frames labelled with an energy level, it triggers only
  above `media.speaking_energy_threshold` (0.01 ≈ −40 dBov).
* `ChannelEnergy` carries the levels of participants whose energy changed by ≥ 3 dB (or went
  silent) every `media.energy_interval_ms` (200); a joining participant receives the current
  levels immediately. `0` disables the reports.
* `ParticipantJoined` / `ParticipantLeft` / `MuteStateChanged` keep the roster; the same facts
  are available to game servers as events and over `GET /v1/channels/{id}/participants`.

## Cross-node channels

When members of one channel sit on different nodes, each node forwards its speakers' frames to
the other nodes holding members (one-hop mesh, `media.port + 1`/UDP), inside an encrypted and
authenticated `Relay` envelope that carries the sender's user id — so receiver-side mutes, blocks,
volumes, positional attenuation and transmission rules are applied by the receiving node exactly
as for local speakers. Echo channels are never relayed. See [Scaling out](../operations/scaling.md).
