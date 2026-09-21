# Agora → Aurix

Applies to the Agora Voice SDK / Video SDK used for audio only (`IRtcEngine`, Unity, Web
`agora-rtc-sdk-ng`, Unreal), the Spatial Audio extension, RTM for chat, and the Cloud Recording
REST API. Agora names are from Agora's public API reference; check the version you run.

Aurix covers Agora's **voice** surface. It does not cover video, screen share, live streaming
(CDN push), Interactive Whiteboard, Conversational AI or Agora's global "SD-RTN" network — those
are out of scope by design ([Limitations](../limitations.md)).

## Concept map

| Agora | Aurix | Notes |
| --- | --- | --- |
| App ID + **App Certificate** (token signing) | application + **API key** on your backend | with Aurix the backend calls the node; nothing is signed client-side or in your code |
| `AccessToken2` / `RtcTokenBuilder.buildTokenWithUid(appId, cert, channel, uid, role, privileges…)` | `POST /v1/tokens` → **player JWT** with per-channel grants | one JWT may cover several channels; `join` / `speak` / `receive` / `moderate` / `priority` flags replace `PUB_AUDIO_STREAM`-style privileges |
| `uid` (uint32) / string user account | `external_id` (your id) → `user_id` (UUID) | stable across sessions; the node creates/looks up the user from `external_id` |
| Channel name (string, created on first join) | channel UUID (`POST /v1/channels`) or `ad_hoc` grant with a name | an `ad_hoc` grant creates the channel on first join with the template in the grant |
| `CHANNEL_PROFILE_COMMUNICATION` | `team` / `positional` channel, everybody `speaker` | |
| `CHANNEL_PROFILE_LIVE_BROADCASTING` + `CLIENT_ROLE_BROADCASTER` / `AUDIENCE` | channel `audience` config: `speaker` vs `listener` role from the grant (`speak: false`) | listeners are not counted as media senders and can be hidden from the roster ([Large channels](../features/channels.md#large-channels-and-audiences)) |
| `setClientRole` at runtime | issue a new token or a `moderate` grant → `POST /v1/moderation/mute` / `unmute` | there is no self-promotion without a grant |
| `joinChannel(token, channel, uid, ChannelMediaOptions)` | `connect(token)` then `joinChannel(channelId)` | one connection, several channels ("multi-channel" `joinChannelEx` ≈ several `joinChannel` calls on one session) |
| `leaveChannel` | `leaveChannel(channelId)` | session stays up |
| `renewToken`, `onTokenPrivilegeWillExpire`, `onRequestToken` | `refreshToken` callback / `TokenRefresher`; the node warns before expiry | same pattern |
| `muteLocalAudioStream`, `enableLocalAudio` | `setMuted(bool)` | mute shows in the roster for everyone |
| `muteRemoteAudioStream(uid)`, `muteAllRemoteAudioStreams` | `setParticipantMuted(userId, muted)`, `setOutputMuted(bool)` | node-side, per receiver |
| `adjustUserPlaybackSignalVolume(uid, 0..100)` | `setParticipantVolume(userId, 0..2)` | multiplied with positional attenuation on the node |
| `adjustRecordingSignalVolume`, `adjustPlaybackSignalVolume` | `setInputGain(0..4)`, `setOutputVolume(0..1)` | client-side |
| `enableAudioVolumeIndication` + `onAudioVolumeIndication(speakers[], totalVolume)` | `speaking` events + `energy` (per channel, per participant), `localEnergy` | server-emitted from the frame level byte; no polling interval to configure |
| `onUserJoined` / `onUserOffline` | `participantJoined` / `participantLeft` | |
| `onConnectionStateChanged`, `onRejoinChannelSuccess` | `connectionState`, `recovering` / `recovered` / `failedToRecover` | resume keeps session, SSRC and channels (also across nodes) |
| `onNetworkQuality(uid, tx, rx)`, `onRemoteAudioStats.mosValue` | `networkQuality` event (bars 1–5, RTT, jitter, loss) + `getStats()`; server-side E-model MOS per session in `/v1/analytics/sessions` and Prometheus | [Quality](../features/quality.md) |
| `setAudioProfile(SPEECH_STANDARD / MUSIC_HIGH_QUALITY_STEREO …)`, `setAudioScenario` | channel `audio_profile` (`voice` / `music` / `broadcast` / `low_bandwidth`), `stereo`, `bitrate`, `sample_rate`, `enable_dtx`, `enable_fec` | profile is a channel property set by the backend, not by each client |
| `setAINSMode`, `enableAudioProcessing` (AEC/ANS/AGC) | `DspMode` / `EchoCancellation` / `NoiseSuppression` / `Agc` (native core); browser: `getUserMedia` constraints | client DSP is classical, not a neural denoiser — [Limitations](../limitations.md) |
| Spatial Audio extension: `ILocalSpatialAudioEngine.updateSelfPosition`, `updateRemotePosition`, `setAudioRecvRange`, `setDistanceUnit`, `setRemoteAudioAttenuation`, `muteAllRemoteAudioStreams` in range | `positional` channel: `updatePosition(channelId, position, orientation)` per player; `positional_config` (`near_distance`, `far_distance`, `max_radius`, `rolloff`, `directional`, `roster_radius`, `text_radius`) | attenuation and cut-off are channel-wide server settings, not per-listener client code; per-participant PCM / tracks for engine or HRTF spatialization |
| `setAudioEffectPreset` (`VOICE_CHANGER_EFFECT_*`, `ROOM_ACOUSTICS_*`), `setLocalVoicePitch`, `setLocalVoiceEqualization` | `setVoiceEffects(params)` + presets `robot` / `monster` / `radio` / `helium` / `ghost` | uplink-only, before VAD and the encoder |
| `startAudioMixing`, `playEffect`, `preloadEffect` (mix files into the uplink) | `injectAudio(source, {mix})`, `AudioInjector` | same idea, no preload cache — feed decoded PCM |
| `enableInEarMonitoring` | not in the SDK; monitor your own capture in the engine | |
| `setDefaultAudioRouteToSpeakerphone`, `setEnableSpeakerphone` | Unity/native: platform audio session; Web: output device selection (`setOutputDevice`) | |
| RTM (`RtmClient`, channel / peer messages, history) | built-in text chat: `sendMessage`, `sendDirectMessage`, `history`, `markRead`, offline delivery ([Text chat](../features/chat.md)) | one connection, same token; no separate product |
| Cloud Recording REST (`acquire` / `start` / `stop`, S3 upload) | `POST /v1/recordings/start` / `stop`, consent gating, mixdown, transcript, `download` — files on the node's storage ([Recordings](../features/recordings.md)) | you provide storage; upload to S3 from your side or with a live stream tap |
| Real-Time Transcription / STT (Agora "Real-Time STT") | channel `transcription: true` + your `[stt]` provider; live translation via `[translation]` ([Speech](../features/speech.md)) | |
| Agora Analytics / Console call inspector | `GET /v1/analytics`, `/v1/analytics/sessions`, `/v1/sessions/:id/stats`, Prometheus + Grafana dashboards | no hosted UI |
| Webhooks (Notification Center: `103 broadcaster join`, `104 leave`…) | `POST /v1/webhooks` subscriptions (HMAC-signed) + `GET /v1/webhooks/events` SSE | [Webhooks and the event stream](../api/webhooks-sse.md) |
| Server: `kick` via Banning REST API (`/dev/v1/kicking-rule`) | `POST /v1/moderation/kick`, `/ban` (with TTL and reason), `/mute`, `/mute-all`, `/kick-all` | [Moderation](../features/moderation.md) |
| Encryption: `enableEncryption(AES-128-GCM2, key, salt)` — key shared by your app | transport encryption always on (AURX AEAD / DTLS-SRTP / TLS); optional **group E2EE** with per-channel sender keys and rotation on join/leave ([E2EE](../features/e2ee.md)) | with Agora the key distribution is your problem; with Aurix E2EE the node relays wrapped keys and cannot read frames |

## Roles and audiences

Agora's live-broadcasting profile is the most common reason a voice game outgrows "everyone
talks". The Aurix equivalent is per-channel:

```json
PUT /v1/channels/{id}/config
{"channel_type": "team", "max_participants": 2000,
 "audience": {"max_speakers": 32, "hide_listeners": true, "mix_for_listeners": true, "max_streams": 8}}
```

* `speak: true` in the grant → `speaker`; `speak: false` → `listener`. A listener that later
  needs to talk gets a new token (or a moderator promotes them).
* Native listeners receive a server mix (`downlink_mode`), browsers receive the mix plus up to N
  per-participant tracks; nobody receives 2000 streams.
* `hide_listeners` keeps the roster to speakers — Agora's audience list is likewise not
  delivered to everyone.

## Spatial audio

Agora's Spatial Audio extension runs on **each client**: every listener calls
`updateRemotePosition` for every speaker and the SDK attenuates locally. Aurix does it once on
the node: each player reports **only their own** pose, the node computes every pair, applies the
channel's roll-off and stops sending streams beyond `max_radius`. Consequences:

* bandwidth and CPU on the client drop with player count (only audible speakers are sent);
* the curve is data, not code — designers change `positional_config` live;
* if you want HRTF or engine occlusion, take per-participant tracks (browser) or PCM (Unity /
  Unreal / Godot) and spatialize yourself — the node still decides who is audible.

`setAudioRecvRange(r)` ≈ `max_radius`; `setDistanceUnit(u)` — Aurix positions are metres, scale
in your code; `setRemoteAudioAttenuation(uid, a, forceSet)` has no per-listener equivalent —
use `setParticipantVolume` for per-listener adjustments.

## Tokens: server side by side

```js
// Agora — signs locally with the App Certificate
const token = RtcTokenBuilder.buildTokenWithUid(APP_ID, APP_CERT, channelName, uid,
  RtcRole.PUBLISHER, tokenExpire, privilegeExpire);

// Aurix — asks the node with the API key (@aurix/server-sdk)
const { token, user_id, endpoint } = await aurix.issueToken({
  external_id: `steam:${player.steamId}`,
  display_name: player.name,
  channels: [{ channel_id: matchChannelId, speak: player.role !== 'spectator' }],
  region: match.region,
});
res.json({ token, userId: user_id, wsUrl: endpoint.ws_url });
```

Agora tokens are bound to one channel and one uid; an Aurix JWT is bound to the user and lists
every channel with its permissions, so a lobby + team + proximity setup is one token. Runnable
servers in Node / Python / Go / C#: `sdk/server/*/examples/token-server`.

## Web side by side

```ts
// Agora
const client = AgoraRTC.createClient({ mode: 'rtc', codec: 'vp8' });
await client.join(APP_ID, channel, token, uid);
const mic = await AgoraRTC.createMicrophoneAudioTrack();
await client.publish([mic]);
client.on('user-published', async (user, type) => { await client.subscribe(user, type); user.audioTrack.play(); });

// Aurix (@aurix/web-sdk)
const voice = new AurixClient({ apiUrl, wsUrl, token, refreshToken: () => fetchToken() });
voice.on('remoteStream', (stream) => { audioEl.srcObject = stream; });   // the server mix
voice.on('speaking', (channelId, userId, speaking) => ui.setTalking(userId, speaking));
await voice.connect();                     // getUserMedia + offer/answer; mic is published
await voice.joinChannel(channelId);
```

There is no per-user subscribe step: what you hear is decided by grants, channel config,
mute/block preferences and positional range. `spatialAudio: true` (HRTF) or `'equalpower'` in the
options spatializes the per-participant tracks in the browser.

## What you give up

* **SD-RTN and last-mile optimisation.** Agora's network is the product; you deploy nodes per
  region and let the token endpoint pick one (`region` / `location` → `endpoint`).
* **Video and screen share** — Aurix is voice-only by design.
* **Hosted analytics UI**, cloud recording storage, Agora's neural noise suppression.
* **Chat product features** beyond what the built-in chat has (no channels-of-channels, no
  message edit/delete, no attachments).

## Checklist

- [ ] `App Certificate` code path deleted; token endpoint calls `POST /v1/tokens` with the API key.
- [ ] Channel names → channel ids or `ad_hoc` grants; broadcaster/audience → `speak` flag + `audience` config.
- [ ] Spatial Audio extension calls → one `updatePosition` per tick + `positional_config`.
- [ ] Notification Center consumers → webhooks / SSE; Banning REST → `/v1/moderation/*`.
- [ ] RTM → built-in chat or your existing chat service (keep it if it does more than you need here).
- [ ] Cloud Recording → node recordings + your storage; consent flow decided.
- [ ] Per-region nodes, TURN and monitoring in place before the canary.
