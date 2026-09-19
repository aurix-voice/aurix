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
    "recording_enabled": false,
    "transcription": false,
    "safety_voice": false,
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
    "audience": { "hide_listeners": true, "mix_for_listeners": true, "max_speakers": 0, "max_streams": 0 }
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
* The policy is delivered to participants as `ChannelJoinAck.audio` and, when an operator edits
  the channel, as `ChannelAudioPolicy` to everyone in it on every node (`channel.config_updated`
  event for your backend). A session in several channels applies the **merge**: the highest
  target/floor bitrate and widest bandwidth, FEC if any channel wants it, DTX only if all allow
  it, the highest complexity hint, `music` over `voice` over auto. The server forwards native
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

`PUT /v1/channels/{id}/config` updates the configuration live; participants on every node hosting
the channel pick it up (channel type changes take effect for subsequent frames).

## Codecs: Opus and the PCMU fallback

Every channel is Opus internally — `ChannelConfig.codec` only accepts `opus`, and that is what
recording, transcription, live streams, cascade and every WebRTC participant see. A **native
AURX session** may nevertheless run on **G.711 μ-law (PCMU)**: 8 kHz, 64 kbit/s, a lookup table
instead of an Opus encoder/decoder — for devices where Opus does not fit the CPU budget.

* The client sends `SetAudioCodec { codec: "pcmu" }` over the control connection and waits
  for `AudioCodecChanged { codec }`; from then on its uplink frames are μ-law of 80/160/320/480
  bytes (10/20/40/60 ms) flagged `Pcmu` and its downlink arrives as μ-law with the same flag.
  `SetAudioCodec { codec: "opus" }` switches back; the ack decides which codec the next frames
  use. The negotiated codec is replayed in `ReceiverPreferences.codec` after a resume; a fresh
  session starts on Opus and the SDKs send `SetAudioCodec` again.
* The node transcodes **at the edge**: a PCMU uplink is decoded and encoded to narrowband Opus
  before it enters the channel, and the Opus a PCMU receiver would get is converted to μ-law
  after mutes, blocks, per-participant volume, focus, positional attenuation and direction have
  been applied — the gain/direction bytes and the per-receiver seal are exactly as for Opus.
  Opus participants in the same channel notice nothing; a PCMU listener hears narrowband audio.
* Not available to WebRTC sessions (`CODEC_NOT_AVAILABLE`; browsers negotiate Opus in SDP) and
  never for `E2ee` frames — the node cannot transcode what it cannot decrypt, so `Pcmu | E2ee`
  frames are dropped. PCMU frames from a session that did not negotiate are dropped too.
* `media.pcmu_fallback = false` refuses the negotiation node-wide. Cost per PCMU session: one
  Opus encoder plus one Opus decoder per speaker it hears, on the node. Metrics:
  `aurix_pcmu_sessions`, `aurix_pcmu_frames_total{direction,outcome}`.

SDKs: Unity `client.SetAudioCodecAsync(AudioCodec.Pcmu)` / `AurixVoiceBehaviour.PreferredCodec`,
native `aurix_client_set_audio_codec`, Unreal `SetAudioCodec` ([overview](../sdk/overview.md)).

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

## Positional audio

Clients send `PositionUpdate {channel_id, position, orientation: {forward, up}}` at a modest
rate (10 Hz is plenty). For each speaker/listener pair the SFU computes the distance
attenuation and, for directional channels, the azimuth/elevation of the speaker relative to the
listener's forward/up vectors. Native receivers get the gain in the per-frame volume byte
(`VolumeAttenuated`) and the direction in two extra bytes (`Directional`), and pan locally;
WebRTC receivers get a server-mixed stereo downlink (`sprop-stereo=1`). `OcclusionUpdate`
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
"audience": { "hide_listeners": true, "mix_for_listeners": true, "max_speakers": 8, "max_streams": 4 }
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
  see [server mix](#server-mix-for-native-clients). Browsers are always mixed.
* **`max_speakers`** (`0` = no separate limit) caps how many members that *may* speak the
  channel admits; the next speaker join fails with `CHANNEL_FULL` while listeners keep
  joining up to `max_participants`. Counted over the members a node knows of (its own plus
  those learned through the cascade), so the cap is approximate across nodes for a few
  hundred milliseconds after a join.
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
