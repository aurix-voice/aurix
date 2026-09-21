# Vivox → Aurix

Applies to the Unity `VivoxService` package (Unity Gaming Services) and to the Vivox Core SDK
(`vx_req_*`, used from Unreal and native code). Vivox names are from Unity's public Vivox
documentation; check its current reference for signatures.

## Concept map

| Vivox | Aurix | Notes |
| --- | --- | --- |
| Issuer / domain / server URL + **token signing key** | application (`POST /v1/apps`) + **API key** (`aurx_…`) | both stay on your backend; Aurix keys carry [permissions](../concepts/auth.md#permissions) and a rate budget |
| Vivox Access Token (`vxa: login`, `join`, `join_muted`, `kick`, `mute`) | **player JWT** (`POST /v1/tokens`) for login + join; single-use **action tokens** (`POST /v1/tokens/action`: `login`, `join`, `kick`, `mute`, `unmute`) | one JWT lists every channel the player may join, with `join`/`speak`/`receive`/`moderate`/`priority` per channel; `join_muted` = a grant with `speak: false` or a server-side mute at join |
| `IVivoxTokenProvider.GetTokenAsync` | `refreshToken` / `joinToken(channelId)` callbacks (Web), `TokenRefresher` (Unity), token-request callbacks in the native core | same shape: the SDK calls your backend when it needs a fresh credential |
| `LoginAsync(LoginOptions { PlayerId, DisplayName })` | `connect()` with the JWT — identity is in the token | `external_id` (your player id) + `display_name` are set by the backend when issuing the token |
| `JoinGroupChannelAsync(name, ChatCapability)` | `joinChannel(channelId)` on a `team` channel | join-by-name → an `ad_hoc` grant with that name; text chat is node-wide (`[chat]`); `ChatCapability.TextOnly` ≈ a grant with `speak: false` |
| `JoinPositionalChannelAsync(name, ChatCapability, Channel3DProperties)` | `positional` channel with `positional_config` | see [Positional](#positional-channels) |
| `JoinEchoChannelAsync` | `echo` channel | mic test through the real uplink → node → downlink path |
| `ChannelType.Positional` `audibleDistance` / `conversationalDistance` / `audioFadeIntensity` / `audioFadeModel` | `positional_config.max_radius` / `near_distance` … `far_distance` / `rolloff` (`linear`, `logarithmic`, `custom_spline`) | plus `max_radius`, `roster_radius`, `text_radius`, `directional`, `coordinate_system` |
| `Set3DPosition(speakerPos, listenerPos, listenerAtOrient, listenerUpOrient)` | `updatePosition(channelId, position, orientation)` (Web), `UpdatePositionAsync` (Unity), `UpdatePositions` (Unreal) | one pose per player, rate-limited by the SDK; the node computes every pair |
| `SetChannelTransmissionModeAsync(TransmissionMode.Single/All/None, channel)` | `setTransmission({type: 'all' \| 'none' \| 'single', channelId})` / `transmitToChannel(id)` (Web), `SetTransmissionAsync` (Unity) | plus `setChannelFocus` for "hear this one louder" |
| `MuteInputDevice` / `UnmuteInputDevice`, `SetInputDeviceVolume` | `setMuted(bool)`, `setInputGain(0..4)` | client-side; the node also learns the mute for the roster |
| `MuteOutputDevice`, `SetOutputDeviceVolume`, `SetChannelVolumeAsync` | `setOutputMuted`, `setOutputVolume`, per-channel volume via `setParticipantVolume` per member or the engine mixer | there is no per-channel gain on the wire; the SDK exposes per-participant and master |
| `MutePlayerLocally` / `UnmutePlayerLocally`, `vx_req_session_set_participant_mute_for_me` | `setParticipantMuted(userId, muted)` / `SetParticipantMutedAsync(userId, muted, channelId?)` | enforced on the node before forwarding, invisible to the muted player |
| `vx_req_session_set_participant_volume_for_me` | `setParticipantVolume(userId, 0..2)` | multiplied with positional attenuation server-side |
| `BlockPlayerAsync` / `vx_req_account_create_block_rule` / `vx_req_account_control_communications` | `setUserBlocked(userId, blocked)` → persistent, mutual `user_blocks`; backend: `POST /v1/users/:id/blocks` | applied to live sessions on every node |
| Server-to-server mute / kick | `POST /v1/moderation/mute`, `/kick`, `/ban`, `/mute-all`, `/kick-all`; in-game moderators via action tokens | [Moderation](../features/moderation.md) |
| `ParticipantAddedToChannel` / `ParticipantRemovedFromChannel` | `participantJoined` / `participantLeft` | in `positional` channels with `roster_radius` these also fire as players move in and out of range |
| `SpeechDetected`, `AudioEnergy` | `speaking(channelId, userId, bool)`, `energy`, `localSpeaking` / `localEnergy` | server-emitted from the client's frame level byte; local VAD in every SDK |
| `ConnectionRecovering` / `ConnectionRecovered` / `ConnectionFailedToRecover` | `recovering` / `recovered` / `failedToRecover` | resume keeps session, SSRC and channels; a resume on another node is a takeover |
| Text: `SendChannelTextMessageAsync`, `SendDirect…`, `Get…TextMessageHistoryAsync`, `SetMessageAsReadAsync`, `GetConversationsAsync` | `sendMessage`, `sendDirectMessage`, `history`, `markRead`, `readMarkers`, `chatInboxSynced` | no edit/delete of sent messages — see [Text chat](../features/chat.md) |
| `SpeechToTextEnableTranscription` (paid add-on) | channel `transcription: true` + `[stt]` provider; `setTranscripts(bool)` | your STT provider, your bill; also post-hoc transcription of recordings |
| Text-to-speech (`TextToSpeech…`) | `speak(text, channelId, destination)` + `POST /v1/channels/:id/tts` | `[tts]` provider; destinations `channel` / `local` / `both` |
| `StartAudioInjection` / `StopAudioInjection` | `injectAudio(source, options)` / `stopAudioInjection()` (Web), `AudioInjector` (Unity) | same semantics (mix with or replace the microphone) |
| VAD properties (`vx_req_aux_set_vad_properties`) | `VoiceActivityDetector` threshold / hang-over, `GateOnVad` | client-side |
| Capture DSP (AEC / NS / AGC) | `DspMode`, `EchoCancellation`, `NoiseSuppression`, `Agc` | native core; managed fallback without AEC |
| Channel "focus" (`vx_req_sessiongroup_set_focus`) | `setChannelFocus(channelId)` | focused channel at full gain, others attenuated |
| Unity `VivoxService.Instance` singleton | `AurixVoiceBehaviour` component + `AurixVoiceClient` | one client per session; several channels per client |

What Aurix has that Vivox does not expose: [recording](../features/recordings.md) with consent,
[group E2EE](../features/e2ee.md), [live translation](../features/speech.md#live-translation),
per-participant PCM for engine spatialization, [priority speakers / ducking](../features/channels.md#priority-speakers-and-ducking),
voice effects and visemes, webhooks + SSE, analytics with MOS.

## What you give up

* **The hosted network.** Vivox runs the servers, TURN and support; you will
  ([Operations](README.md#what-changes-for-your-operations-team)).
* **Console SDKs.** Vivox ships platform-approved binaries; Aurix offers a
  [porting guide over the C ABI](../sdk/consoles.md) and no console binaries.
* **Message edit/delete** in text chat, and the Vivox "sessiongroup" abstraction — an Aurix
  session simply joins several channels.
* Unity Gaming Services integration (Authentication → Vivox login). With Aurix, your backend
  issues the JWT after whatever authentication you already do.

## Token issuance

Vivox tokens are signed on your server with the issuer key; the client receives a token per
action. With Aurix the token endpoint calls the node with the API key:

```http
POST /v1/tokens
X-API-Key: aurx_…

{
  "external_id": "steam:76561198000000000",
  "display_name": "Alice",
  "channels": [
    {"channel_id": "0193e0d2-…", "speak": true, "receive": true, "moderate": false},
    {"ad_hoc": {"name": "match-8f3a", "channel_type": "team", "max_participants": 10}}
  ],
  "region": "eu-west"
}
```

The response carries `token`, `user_id`, `expires_at` (lifetime = `auth.token_ttl_secs`, default
1 h) and an `endpoint` (`ws_url`, `api_url`) chosen by region/location — hand exactly these to
the client. For a per-action equivalent of Vivox
`kick`/`mute` tokens use `POST /v1/tokens/action` with `{"action": "kick", "user_id": <actor>,
"target_user_id": …, "channel_id": …}`; the token binds actor, target and channel and is valid once.
Reference implementations: `sdk/server/{node,python,go,csharp}/examples/token-server`.

## Positional channels

Vivox attenuates between `conversationalDistance` (full volume) and `audibleDistance` (silence)
with `audioFadeIntensity` shaping the curve. The equivalent `positional_config`:

```json
{"near_distance": 1.0, "far_distance": 50.0, "max_radius": 60.0, "rolloff": "logarithmic",
 "directional": true, "coordinate_system": "left_handed", "roster_radius": 60.0, "text_radius": 20.0}
```

* `near_distance` ≈ `conversationalDistance`, `far_distance` ≈ `audibleDistance`;
  `max_radius` is where the node stops sending the stream at all (Vivox has no separate cut-off).
* `audioFadeModel` `InverseByDistance` ≈ `logarithmic`, `LinearByDistance` ≈ `linear`,
  `ExponentialByDistance` ≈ `custom_spline` with your own points. Tune by ear — the curves are
  not numerically identical.
* Vivox sends a listener orientation every update; Aurix uses the reported orientation for
  `directional` panning (left/right by where the speaker stands) and, in engines, you can
  instead take per-participant PCM (`AurixParticipantAudioSource`, `UAurixParticipantSoundWave`)
  and let the engine spatialize.
* Presence and text can be scoped by distance (`roster_radius`, `text_radius`) — Vivox
  positional channels show everybody in the channel.

## Unity: side by side

```csharp
// Vivox
await VivoxService.Instance.InitializeAsync();
await VivoxService.Instance.LoginAsync(new LoginOptions { DisplayName = name });
await VivoxService.Instance.JoinPositionalChannelAsync("zone-7", ChatCapability.TextAndAudio,
    new Channel3DProperties(50, 1, 1.0f, AudioFadeModel.InverseByDistance));
VivoxService.Instance.Set3DPosition(speakerPos, listenerPos, listenerAt, listenerUp, "zone-7");

// Aurix (com.aurix.voice)
var voice = GetComponent<AurixVoiceBehaviour>();
voice.WebSocketUrl = endpoint.WsUrl;         // from your token endpoint
voice.Token = token;                          // player JWT, channels inside
voice.ChannelId = zoneChannelId;              // "zone-7" as an ad_hoc grant → id in the token response
await voice.Connect();
await voice.Client.UpdatePositionAsync(zoneChannelId, voice.Client.Session.UserId,
    new Position3D { X = p.x, Y = p.y, Z = p.z },
    new Orientation3D { ForwardX = fwd.x, ForwardY = fwd.y, ForwardZ = fwd.z, UpX = up.x, UpY = up.y, UpZ = up.z });
```

The channel's fade curve lives on the server (`positional_config`), not in the join call, so
designers can retune it live with `PUT /v1/channels/{id}/config` while players are in it.
Events: `voice.Client.OnParticipantJoined`, `OnParticipantLeft`, `OnSpeaking`, `OnChannelEnergy`,
`OnRecovering` / `OnRecovered` / `OnFailedToRecover`. Full map: [Unity SDK](../sdk/unity.md#feature-map).

## Unreal / native (Vivox Core SDK)

`vx_req_connector_create` → `Connect`; `vx_req_account_anonymous_login` → the JWT in `Connect`;
`vx_req_sessiongroup_addsession` → `JoinChannel`; `vx_req_session_set_participant_mute_for_me` /
`…volume_for_me` → `SetParticipantMuted` / `SetParticipantVolume`; `vx_req_aux_set_capture_device`
/ `…render_device` → `GetCaptureDevices` + your capture path (the plugin captures through
Unreal's audio device by default). Per-participant spatialization: `CreateParticipantSound`
returns a `USoundWave` you attach to the avatar's audio component instead of the Vivox channel
mix. See [Native core and Unreal SDK](../sdk/native.md) and `sdk/unreal/AurixVoice/Docs/QuickStart.md`.

## Checklist

- [ ] Token endpoint returns Aurix JWTs; API key never reaches the client (the token servers'
      tests assert this — copy them).
- [ ] Channel names → channel ids or `ad_hoc` grants; positional curves moved to `positional_config`.
- [ ] Moderation: server-to-server calls → `/v1/moderation/*`; in-game moderator UI → action tokens.
- [ ] Vivox callbacks your backend consumed → webhooks / SSE (`participant.*`, `moderation.*`).
- [ ] Cross-mute lists exported from your own store → `POST /v1/users/:id/blocks`.
- [ ] Reconnect UX rewired to `recovering` / `recovered` / `failedToRecover`.
- [ ] Load test with `tools/loadtest` at your peak CCU before the canary.
