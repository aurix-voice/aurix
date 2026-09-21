# Photon Voice → Aurix

Applies to Photon Voice 2 for Unity on top of PUN 2, Fusion or Quantum (`Recorder`, `Speaker`,
`PhotonVoiceNetwork`, `PhotonVoiceView`, `FusionVoiceClient`, `VoiceNetworkObject`,
`WebRtcAudioDsp`). Names are from the public Photon Voice documentation; check the version you run.

Photon Voice is a **relay**: the Photon Cloud (or a self-hosted Photon Server) forwards each
client's Opus frames to the other clients in the room / interest group and every client decodes
and mixes locally. Aurix is an **SFU with a media node**: the node knows who is where, decodes
when it must (server mix, recording, transcription, positional attenuation) and forwards
otherwise. Most of the migration is moving decisions from the client (`Speaker`, interest
groups, `Recorder` settings) to the backend (channel config, tokens).

## Concept map

| Photon Voice | Aurix | Notes |
| --- | --- | --- |
| Voice **App ID** (separate from the PUN / Fusion App ID) | application + **API key** on your backend | the client never sees an app-level credential |
| `AppSettings` (region, `FixedRegion`, Best Region ping) | token endpoint chooses `endpoint` by `region` / `location`; SDKs can probe `GET /v1/me/regions` and pick by RTT | you run the regions |
| Voice room = PUN room name (+ suffix) / `PhotonVoiceNetwork.AutoConnectAndJoin` | channel UUID or `ad_hoc` grant named after the match; `joinChannel` after `connect` | one voice session can be in the lobby, team and proximity channels at once |
| **Interest groups** (`Recorder.InterestGroup`, `Client.OpChangeGroups`) for teams / proximity | separate `team` channels for teams; a `positional` channel with `positional_config` for proximity | Aurix computes proximity per pair on the node from reported positions instead of the game moving players between groups |
| `Recorder` | the SDK's capture path: `AurixVoiceBehaviour` / `AurixVoiceClient` (Unity), `Client` (native), `AurixClient` (Web) | one capture per session, not one per room |
| `Recorder.TransmitEnabled` | `SetMuted(false)` / `setMuted` | shows in the roster |
| `Recorder.IsRecording` (start/stop capture device) | `SetMuted(true)`; device release is engine-side | |
| `Recorder.VoiceDetection`, `VoiceDetectionThreshold`, `VoiceDetectionDelayMs` | `VoiceActivityDetector` threshold / hang-over, `GateOnVad` | plus server-side `speaking` events for everyone |
| `Recorder.Codec` (Opus), `SamplingRate`, `FrameDuration`, `Bitrate` | channel config: `codec`, `sample_rate`, `bitrate`, `min_bitrate`, `complexity`, `enable_fec`, `enable_dtx`; frame = 20 ms | the **server** sets the policy (`AudioPolicy`) and can lower bitrate under congestion; a client cannot pick its own bitrate |
| `Recorder.ReliableMode` | not applicable (media is unreliable by design; FEC / DRED / PLC cover loss) | |
| `Recorder.Encrypt` | transport encryption always on; optional group [E2EE](../features/e2ee.md) | |
| `Recorder.UserData` | `metadata` in the token request → stored on the user (`GET /v1/users/{id}`); game data for avatars travels through your own netcode | set by the backend, not the client |
| `Recorder.DebugEchoMode` | `echo` channel type | |
| `Recorder.SourceType = AudioClip` / `Factory` | `AudioInjector` / `injectAudio` | |
| `Recorder.MicrophoneType` (Unity / Photon), `MicrophoneDevice`, `AudioChangesHandler` | `AurixVoiceBehaviour.InputDevices` / `OnInputDeviceChanged` (Unity), `GetCaptureDevices` (Unreal), `setInputDevice` + `devicesChanged` (Web) | |
| `WebRtcAudioDsp` (AEC, AGC, NS, VAD, high-pass, `ReverseStream`) | `DspMode`, `EchoCancellation`, `NoiseSuppression`, `Agc` in the native core; Unity native transport gets them through the core | managed / WebGL fall back to browser or no AEC |
| `Speaker` component on the remote player's object (+ `AudioSource` with `spatialBlend`) | `AurixParticipantAudioSource` / `RemoteMixer.Pull(userId)` — per-participant PCM for an `AudioSource` on the avatar | same pattern; or let the node mix (`DownlinkMode.Mixed`) and play one `AudioSource` |
| `PhotonVoiceView` (links `Recorder` / `Speaker` to a `PhotonView`) | link `user_id` → avatar yourself: your token endpoint returns `user_id` for each player — replicate it with your netcode; roster events carry `user_id` | the roster does not expose `external_id` |
| `Speaker.PlayDelayMs`, jitter settings | `RemoteMixer` / `JitterBuffer(targetDepthFrames, maxDepthFrames)` — 20 ms frames | |
| `VoiceClient.RemoteVoiceAdded`, `SpeakerLinked` | `participantJoined`, `participantUpdated`, `speaking` | |
| `LoadBalancingTransport` / Photon Server self-hosting | `aurix-server` nodes + PostgreSQL + Redis + TURN ([Deployment](../operations/deployment.md)) | |
| Photon Server plugins for server-side logic | REST API + webhooks / SSE; server-side mute / kick / ban; recording; transcription | the node already does the things a Photon plugin would be written for |
| Client-side mute of a remote (`Speaker` disable / `AudioSource.mute`) | `SetParticipantMutedAsync(userId, true)` — the node stops forwarding; `SetParticipantVolumeAsync` | saves bandwidth, survives reconnects, invisible to the muted player |
| No server-side moderation | `POST /v1/moderation/mute`, `/kick`, `/ban`, `/mute-all`, `/kick-all`; in-game moderators via action tokens; persistent cross-mute `user_blocks` | [Moderation](../features/moderation.md) |
| No server recording | `POST /v1/recordings/start` with consent gating, mixdown, transcript | [Recordings](../features/recordings.md) |
| Text chat via PUN RPC / Photon Chat (separate product) | built-in chat: `sendMessage`, `sendDirectMessage`, history, read markers, offline delivery | [Text chat](../features/chat.md) |
| Reconnect: `PhotonVoiceNetwork` re-joins with PUN | `recovering` / `recovered` / `failedToRecover`; session, SSRC and channels survive, also across nodes | |

## Proximity voice without interest groups

Photon proximity is usually built by re-assigning interest groups on a grid and letting each
client attenuate remote `AudioSource`s. With Aurix:

```csharp
// once per tick (the SDK rate-limits) — only YOUR pose
await voice.Client.UpdatePositionAsync(zoneChannelId, voice.Client.Session.UserId,
    new Position3D { X = p.x, Y = p.y, Z = p.z },
    new Orientation3D { ForwardX = f.x, ForwardY = f.y, ForwardZ = f.z, UpX = u.x, UpY = u.y, UpZ = u.z });
```

and on the backend:

```json
PUT /v1/channels/{id}/config
{"channel_type": "positional",
 "positional_config": {"near_distance": 2, "far_distance": 40, "max_radius": 50,
                       "rolloff": "logarithmic", "roster_radius": 50, "text_radius": 15}}
```

The node applies the roll-off per pair and stops sending anyone beyond `max_radius`, so a
1 000-player zone is one channel: each client receives only the neighbours it can hear
(`max_streams` / server mix cap the rest). Keep Unity's `AudioSource` on the avatar for HRTF or
occlusion by feeding it per-participant PCM (`AurixParticipantAudioSource`) — the node has
already decided *who* is audible, the engine decides *how it sounds*. Presence (`roster_radius`)
and text (`text_radius`) can follow the same distances.

## Teams and squads

Interest group 1 = team A, 2 = team B becomes two `team` channels and one token:

```json
{"external_id": "player:42", "display_name": "Bob",
 "channels": [
   {"ad_hoc": {"name": "match-8f3a:team-a", "channel_type": "team", "max_participants": 5}},
   {"ad_hoc": {"name": "match-8f3a:zone", "channel_type": "positional", "max_participants": 100}}
 ]}
```

The client joins both; `SetTransmissionAsync(TransmissionMode.Single(teamChannel))` is push-to-talk
into the squad while still hearing proximity. Changing teams = a new token from your backend
(the old grant is not in it) — there is no client-side "switch group".

## Fusion / Quantum

`FusionVoiceClient` + `VoiceNetworkObject` tie voice to a `NetworkRunner`. With Aurix there is no
coupling to the netcode: when the runner spawns a player, ask your backend for a voice token for
the match's channels, `Connect()`, and map the returned `user_id` to the `PlayerRef` through a networked property. Voice
keeps working through Fusion host migration because the Aurix session is independent of the
game session.

## What you give up

* **Photon Cloud** (regions, relay, scaling on demand) — you deploy the nodes.
* The **"no server, just relay"** simplicity: Aurix nodes need PostgreSQL, Redis and TURN.
  In return you get moderation, recording, transcripts, analytics and server-side positional
  routing that a relay cannot do.
* Per-client codec knobs (`Recorder.Bitrate` etc.) — policy moves to the channel.
* Photon Server plugin hooks — replaced by REST + webhooks.

## Checklist

- [ ] Voice App ID removed from the client; token endpoint issues JWTs from the game session.
- [ ] Interest groups → channels (teams) / `positional` channel (proximity) with `positional_config`.
- [ ] `Recorder` settings → channel config on the backend; `WebRtcAudioDsp` → core DSP flags.
- [ ] `Speaker` per avatar → `AurixParticipantAudioSource` keyed by the replicated `user_id`, or the server mix.
- [ ] Client-side remote mutes → `SetParticipantMutedAsync`; block lists → `POST /v1/users/:id/blocks`.
- [ ] Moderation / recording that used to be impossible: decide what you want now that it is.
- [ ] Nodes per region + TURN; load test with `aurix-loadtest` at peak CCU before the canary.
