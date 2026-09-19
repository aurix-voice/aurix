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
action token decides them at join time.

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
      "coordinate_system": "left_handed"
    }
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
  or `right_handed`) tells the panner which way is left.

`PUT /v1/channels/{id}/config` updates the configuration live; participants on every node hosting
the channel pick it up (channel type changes take effect for subsequent frames).

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

## Ad-hoc channels

A grant `{"ad_hoc": {"name": "…", "channel_type": "team", "max_participants": 8}}` in the
session JWT or the `join` action token yields a deterministic per-application `channel_id`. The
first join creates the channel (or revives a soft-deleted one) under a row lock, so concurrent
first joiners converge on one row; the last leave soft-deletes it (`channel.deactivated`,
`channel.destroyed`). Application channel quotas and `max_participants` apply as usual, and the
creating join is rolled back if it fails half-way. Use this for parties and matches your backend
does not want to pre-create.

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
