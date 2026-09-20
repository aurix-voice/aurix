# Web SDK (`@aurix/web-sdk`)

Browser client: JSON control channel over WebSocket plus **one** WebRTC peer connection carrying
the Opus microphone uplink and the server-mixed downlink. ES2020 module with type declarations
and no runtime dependencies. Source and full API reference: `sdk/web/README.md`; a complete demo
page lives in `sdk/web/demo`.

```bash
cd sdk/web
npm install && npm run build   # -> dist/
npm run demo                   # http://localhost:5173/
npm test                       # quality model unit tests
```

## Connect and join

```ts
import { AurixClient } from '@aurix/web-sdk';

const client = new AurixClient({
  apiUrl: 'https://voice.example.com',   // REST: TURN credentials (GET /v1/me/turn-credentials)
  wsUrl: 'wss://voice.example.com/ws',   // control channel
  token,                                 // player JWT or one-time login token from YOUR backend
});

client.on('remoteStream', (stream) => { audioEl.srcObject = stream; });
client.on('participantJoined', (channelId, p) => roster.add(p));
client.on('participantLeft', (channelId, p) => roster.remove(p));
client.on('speaking', (channelId, userId, speaking) => roster.mark(userId, speaking));
client.on('recording', (channelId, recordingId, active) => {
  if (active) client.respondToRecording(recordingId, 'accepted');
});

await client.connect();                          // SessionInitAck + getUserMedia + WebRTC offer/answer
const participants = await client.joinChannel(channelId);
client.setMuted(true);
client.disconnect();
```

Requirements: the page origin must be in `AURIX__SERVER__CORS_ORIGINS`; `getUserMedia` needs
`https://` or `http://localhost`; the browser must reach `media.external_ip:media.port` over UDP
or a TURN server (built-in `[turn]` or your own).

## Region selection

Players should connect to the nearest node with capacity — and reconnect to the *same* node
while it is up (a resume elsewhere is a [takeover](../operations/high-availability.md#cross-node-session-failover),
which works but moves media). Either pass the `endpoint.ws_url` your backend receives
from `POST /v1/tokens` (with `region` / `location` hints), or let the browser choose:

```ts
import { AurixClient, discoverRegions } from '@aurix/web-sdk';

const { recommended, regions } = await discoverRegions({
  apiUrl: 'https://voice.example.com',          // any node / shared API hostname
  token,                                        // player JWT → GET /v1/me/regions
  region: partyLeaderRegion,                    // optional: ranks first when reachable
  location: { latitude: 48.9, longitude: 2.3 }, // optional: server orders by distance
  // probe: true (default) → GET each region's probe_url a few times, best RTT wins
});
const client = new AurixClient({ apiUrl, wsUrl: recommended!.ws_url, token });
```

`discoverRegions` calls `GET /v1/me/regions`, then (unless `probe: false`) fetches each entry's
`probe_url` (`/health` of that node) — one warm-up request discarded, `probeSamples` (3) timed,
the minimum kept. Ranking: preferred region if its probe succeeded → measured RTT in 15 ms
buckets (ties keep the server's distance/load order) → unprobed regions → regions whose probe
failed. `rankRegions` / `probeRtt` / `parseRegionsResponse` are exported for custom policies.
Only healthy nodes with a public `wss://` URL are advertised (`regions: []` means none is
configured for discovery); probes are cross-origin requests, so the node's `cors_origins` applies
to them as it does to the REST calls. Details on the server side: [Regions](../operations/scaling.md#regions).

## Feature map

| Feature | API | Notes |
|---|---|---|
| Reconnect / resume | `autoReconnect` (default on), `reconnect: {...}` backoff, `recovering` / `recovered` / `failedToRecover` / `sessionClosed`, `reconnectNow()` | resumed session keeps SSRC and channels; `channelJoined` fires again with a fresh roster |
| Failover | `endpoint`, `failover`, `endpointChanged(url)`, `recovered(info)` with `info.migrated` | attempts rotate current node → advertised failover nodes; a takeover keeps session id/SSRC, new media key/endpoint |
| Action tokens | `token`, `refreshToken()`, `joinToken(channelId)`, `moderate(channelId, userId, 'kick'\|'mute'\|'unmute', token, reason)` | mandatory under `auth.require_action_tokens` |
| Local mute / volume / block | `setParticipantMuted(userId, muted, channelId?)`, `setParticipantVolume(userId, 0..2)`, `setUserBlocked(userId, blocked)`, `receiverPreferences`, `userBlockChanged` | enforced server-side before fan-out; replayed after a non-resumed reconnect |
| Multiple channels | `setTransmission({type:'all'\|'none'\|'single', channelId?})`, `transmitToChannel(id)`, `setChannelFocus(id?)`, `transmissionChanged`, `channelFocusChanged` | `CHANNEL_LIMIT_EXCEEDED` when `media.max_channels_per_session` is hit |
| Positional / directional | `updatePosition(channelId, {x,y,z}, {forward_*, up_*})`, `positions` | mixed track: server-panned stereo (`stereo=1`) — do not re-pan; per-participant tracks: the SDK attenuates/pans them itself with HRTF from the same positions, see [below](#per-participant-tracks-and-spatial-audio) |
| Per-participant tracks / HRTF | `participantStreams`, `spatialAudio: true \| 'equalpower' \| false`, `audioContext`; `participantStreamCap`, `negotiatedParticipantStreams`, `setPinnedParticipants(ids)`, `getPinnedParticipants()`, `getParticipantStreams()` / `getParticipantStream(userId)`, `isParticipantSpatialized(userId)`, `participantStreams` event, `resumeAudio()` | bounded by the node's `webrtc_participant_streams`; everyone else stays in the mixed track |
| Presence / text range | `channelScope(channelId)` → `{ rosterRadius?, textRadius? }`; `participantJoined` / `participantLeft` also fire as players move in and out of the roster radius (`Participant.role` / `muted` are filled from the event) | see [radius-scoped presence](../features/channels.md#radius-scoped-presence-and-text) |
| Large channels | `channelInfo(channelId)` → `ChannelInfo { role, participantCount, hiddenListeners, transcription, safetyVoice }`, `canSpeakIn(channelId)`; `channelJoined` still delivers the visible roster | `participantCount` counts every node and the listeners hidden from the roster; a `listener` is receive-only (the mic track is still sent, the node drops it). Browsers always get the server's WebRTC mix (plus bounded per-participant tracks), so `SetDownlinkMode` does not apply — see [large channels](../features/channels.md#large-channels-and-audiences) |
| Energy / VAD | `energy`, `localVoiceActivity: true \| {...}`, `localEnergy`, `localSpeaking` | remote levels from the server's `ChannelEnergy` |
| Devices | `AurixClient.enumerateAudioDevices()`, `setInputDevice()`, `setInputGain(0..4)`, `attachAudioOutput(el)`, `setOutputDevice()` (`supportsOutputSelection`), `setOutputVolume()`, `setOutputMuted()`, `devicesChanged`, `inputDeviceChanged` | mic hot-swap via `replaceTrack`, no renegotiation |
| Echo test / injection | echo channel + `injectAudio(AudioBuffer \| MediaStream, {loop, gain, mixWithMicrophone})`, `stopAudioInjection()`, `decodeAudio()`, `audioInjection` | same Web Audio graph as the input gain |
| Chat | `sendMessage()`, `sendDirectMessage()`, `setTyping()`, `chatMessage`, `participantTyping`; stored chat: `history(scope, {before, after, limit})`, `markRead(scope, messageId)`, `readMarkers(scope)`, `chatReadMarker`, `chatInboxSynced`, `message.offline` | see [Text chat](../features/chat.md) |
| Transcripts / TTS | `transcript`, `setTranscripts(bool)`, `isChannelTranscribed()`, `isChannelMonitored()`, `speak(text, {destination, voice, clientRef})` → `{ done }`, `ttsStatus`, `cancelSpeech()` | see [Transcripts and TTS](../features/speech.md) |
| Live translation | `sessionInfo.translation` (`{speech, languages}` or `undefined`), `setTranslation(language, {spokenLanguage, speech})`, `translationPrefs`, `translationChanged`; translated `transcript`s carry `original {text, language}` | replayed on reconnect; see [live translation](../features/speech.md#live-translation) |
| Stats / quality | `getStats()` → `ClientStats`, `stats`, `networkQuality`, `client.networkQuality`, `qualityReportIntervalMs`, `bitrate` | see [Network quality](../features/quality.md) |
| Opus controls | `opus: {maxBitrateBps, fec, dtx, maxBandwidth, cbr, followChannelPolicy}`, `setOpusOptions()`, `audioPolicy` / `channelAudioPolicy(id)`, `opusPreferences`, `negotiatedOpus`, `renegotiateMedia()`, `audioPolicy` event | see [Opus in the browser](#opus-in-the-browser) |
| Errors | rejected promises carry `Error('<CODE>: <message>')`; unsolicited server errors arrive as `serverError` | codes listed in [Errors](../concepts/auth.md#errors) |

## Per-participant tracks and spatial audio

Next to the mixed downlink the SDK offers `recvonly` audio m-lines the node fills with
**one speaker each** ([per-participant tracks](../features/channels.md#per-participant-tracks-for-browsers)):
as many as `participantStreams` asks for, capped by `SessionInitAck.webrtc_participant_streams`
(`participantStreamCap`, `0` on nodes without the feature — then the client is the mixed-only
browser of before). Each track's Opus frames are the speaker's own, so the SDK renders them
through a Web Audio graph — `MediaStreamAudioSourceNode → GainNode → PannerNode → master gain →
destination`:

* **gain** = your local volume for that user × focus (`unfocused_channel_gain` from the node
  for channels other than the focused one) × distance attenuation reproduced from
  `ChannelJoinAck.positional` (near/far distance, rolloff, `max_radius`) — and `0` for users
  you muted or blocked. The server has already dropped what you may not hear at all (mutes,
  blocks, `max_streams`, radius); the local gain only shapes what arrives.
* **position** — `PannerNode` with `panningModel = 'HRTF'` (`spatialAudio: 'equalpower'` for the
  cheaper model) fed from the same `updatePosition` calls: your last position/orientation is the
  listener frame, the speaker's `positions` entry the source, handedness converted per the
  channel's `coordinate_system`. Non-positional channels skip the panner (plain gain).
* **layout** arrives asynchronously as the `participantStreams` event
  (`[{ mid, userId | undefined, stream | undefined }]`) and `getParticipantStreams()`; a track
  changes hands with a short server-side hold, and the SDK re-renders on every layout, roster,
  position, mute, block, volume and focus change. `setPinnedParticipants(ids)` keeps the given
  users on a dedicated track while audible (`RangeError` above the cap; replayed after a
  reconnect); everyone without a track is in the mixed track, which keeps playing in the
  attached `<audio>` elements.
* **fallback**: `spatialAudio: false` still negotiates the tracks but leaves rendering to you
  (`getParticipantStream(userId)` → `MediaStream`, mute/volume/position are then yours to
  apply); no `AudioContext` (old browser, `createAudioContext()` returns `undefined`) or
  `participantStreams: 0` → mixed only. The autoplay policy applies to the `AudioContext` as
  to `<audio>`: call `resumeAudio()` from a user gesture — it resumes the graph and the attached
  elements together. `setOutputVolume` / `setOutputMuted` / `setOutputDevice` apply to both paths
  (`AudioContext.setSinkId` where the browser has it). On reconnect the tracks are re-offered,
  the layout is re-pushed by the node and the graph rebuilt; `disconnect()` closes the context
  the SDK created (a caller-supplied `audioContext` is left open).

Ambient channels never hand out dedicated tracks (their ranking/dimming is server-only state);
`isParticipantSpatialized(userId)` says whether a voice currently goes through the panner. Nodes
cap the count (`media.webrtc_participant_streams`, ≤ 64); expect ~1 RTP stream per track.

## Opus in the browser

A browser never exposes its Opus encoder; everything goes through WebRTC, so the SDK applies the
server's channel audio policy ([channels](../features/channels.md#configuration)) only where
WebRTC has a knob for it:

| Control | Mechanism | Takes effect |
|---|---|---|
| bitrate target / ceiling | `RTCRtpSender.setParameters({encodings: [{maxBitrate}]})` and `maxaveragebitrate` in the Opus `fmtp` | live / negotiation |
| in-band FEC | `useinbandfec` in the Opus `fmtp` (RFC 7587) | negotiation |
| DTX | `usedtx` in the Opus `fmtp` | negotiation |
| max bandwidth | `maxplaybackrate` in the Opus `fmtp` (`wideband` → 16000 …) | negotiation |
| constant bitrate | `cbr` in the Opus `fmtp` (`opus.cbr`, local only) | negotiation |
| stereo uplink | `stereo=1` in the Opus `fmtp` + a 2-channel track (`opus.stereo`, `defaultAudioConstraints`) | negotiation |
| complexity, signal mode, VBR mode, expected loss | **not controllable** — the browser decides | — |

RFC 7587 makes the `fmtp` parameters the *receiver's* wishes, so the SDK rewrites the Opus
`fmtp` line of the server's answer before `setRemoteDescription`. The policy of every joined
channel is merged exactly like in the native SDKs (`mergeAudioPolicies` in `opus.ts`) and
`audioPolicy` fires on change; the bitrate ceiling is re-applied immediately, while FEC/DTX/
bandwidth changes wait for the next negotiation — compare `client.negotiatedOpus` with
`client.opusPreferences` and call `renegotiateMedia()` (a short audio gap) when it matters.
`BitrateCommand` lowers the ceiling within the policy floor and the SDK never disables the
browser's own congestion control underneath `maxBitrate`. Set `opus.followChannelPolicy: false`
to keep only your explicit options.

`opus: { stereo: true }` asks the browser to encode two channels — honoured only when the merged
channel policy has `stereo: true` (see
[Stereo and music uplinks](../features/channels.md#stereo-and-music-uplinks)) unless
`followChannelPolicy: false`. Because browsers' voice processing downmixes the input, the
default microphone constraints for a stereo client (`defaultAudioConstraints(opts.opus)`) request
`channelCount: {ideal: 2}` with echo cancellation, noise suppression and auto-gain **off** —
meant for music sources and stereo interfaces, not a headset; pass your own `audioConstraints`
to override. Stereo takes effect at the next negotiation (`renegotiateMedia()`) and shows up in
`negotiatedOpus.stereo`; whether the browser actually sends two channels also depends on the
device and the browser's Opus implementation.

## How it maps to the server

| SDK | server |
|---|---|
| `new WebSocket(wsUrl, ['aurix', 'bearer.<jwt>'])` | JWT authenticated at upgrade; `SessionInitAck` carries `session_id` / `ssrc` |
| `connect()` → `WebRtcOffer` (1 `sendrecv` + N `recvonly` audio m-lines) | `SfuNode::attach_webrtc` (str0m, ICE-lite, host candidate = `media.external_ip:media.port`); first m-line = server mix, the rest per-participant tracks up to `media.webrtc_participant_streams` |
| `setPinnedParticipants()` → `SetParticipantStreams {pinned}` | `ParticipantStreams {streams: [{mid, user_id}]}` on negotiation and every layout change |
| `GET /v1/me/turn-credentials` (optional) | time-limited TURN credentials for the browser's relay candidates |
| `joinChannel()` → `ChannelJoin {channel_id, token}` | membership check against the token's channel claims / ad-hoc grant |
| `setMuted()` → track `enabled` + `MuteStateChanged` | broadcast to channel members |
| `getStats()` → `QualityReport` (loss in %) | `BitrateCommand` (applied through `RTCRtpSender.setParameters`, within the channel policy) + merged `NetworkQuality` |
| answer `fmtp` rewrite (`useinbandfec`, `usedtx`, `maxplaybackrate`, `maxaveragebitrate`) | `ChannelJoinAck.audio` / `ChannelAudioPolicy` from the channel config |
| `Ping` / `Pong` | keepalive + `roundTripMs`; two missed pongs close the socket |
| reconnect with `['aurix', 'bearer.<jwt>', 'resume.<session_id>.<resume_token>']` | `SessionInitAck {resumed: true}` + replayed `ChannelJoinAck`s within `server.session_resume_grace_secs` |

## Standalone bundle and `AurixBridge`

`npm run build` also produces `dist/aurix-web-sdk.js`, the SDK as one classic script defining
`window.AurixWebSdk` for pages without a module system, and exports `AurixBridge`: a handle-based
façade (`create(optionsJson)` → handle, `invoke(handle, method, argsJson, rid?)` → JSON,
`drain(handle)` → JSON event array, `destroy(handle)`) for hosts that can only exchange strings.
Promise results arrive as `result` events keyed by `rid`, token callbacks are inverted into
`tokenRequest` events answered with `provideToken`, remote audio is attached to a hidden `<audio>`
element (mixed track) and the Web Audio graph (per-participant tracks) with `remoteAudio` /
`resumeAudio` for the autoplay policy — `participantStreams` events and `setPinnedParticipants` /
`participantStreamCap` / `participantStreams` / `isParticipantSpatialized` expose the track
layout without ever passing a `MediaStream` through the string bridge — and the per-client queue is
bounded (`overflow` reports drops). This is the contract the Unity WebGL client is built on
([Unity WebGL](unity.md#unity-webgl)); details in `sdk/web/README.md`.

## End-to-end encryption

Channels with `e2ee: true` ([End-to-end encryption](../features/e2ee.md)) work in browsers that
have WebCrypto and an encoded-frame API: the SDK seals each Opus frame in the sender's encoded
stream and opens it in the receivers' per-participant tracks, so the node relays ciphertext.

```ts
const support = AurixClient.e2eeSupport();      // { crypto, transform: 'script' | 'streams' | undefined, ok }
const client = new AurixClient({
  apiUrl, wsUrl, token,
  participantStreams: 16,                        // encrypted members are only audible on dedicated tracks
  e2ee: { identity: storedSecret },              // optional: keeps the fingerprint stable across sessions
});
await client.connect();
client.e2eeAvailable;                            // false → encrypted channels fail with E2EE_REQUIRED
client.e2eeFingerprint;                          // show it so peers can compare out of band
client.on('e2eePeerKey', (userId, fingerprint, previous) => { /* new or changed peer identity */ });
client.on('e2eePeerDecryptable', (userId, decryptable) => { /* we hold (or lost) their sender key */ });
client.on('e2eeKeyRotated', (generation) => {});
client.isChannelEncrypted(channelId); client.e2eePeerFingerprint(userId);
client.isE2eePeerDecryptable(userId); client.e2eeDecryptablePeers();
client.e2eeStats;                                // { framesE2ee, undecryptable, held } (refreshE2eeStats() on the worker path)
await client.rotateE2eeKey();                    // normally automatic on join/leave
localStorage.e2ee = base64(client.e2eeIdentitySecret);
```

`e2ee: true` is the default; it only means "announce the capability when the browser has it".
`e2ee: false` never joins encrypted channels. Without the APIs `connect()` still succeeds, but
joining an encrypted channel is refused with `E2EE_REQUIRED` — there is no plaintext fallback,
and the SDK never plays plaintext arriving from an encrypted channel. `transform: 'auto'`
prefers `RTCRtpScriptTransform` (a worker built from a `blob:` URL — set `workerUrl` to a
file serving `e2eeWorkerSource` under a strict CSP) and falls back to `createEncodedStreams()`
(Chromium). One peer connection encrypts all of its channels or none (`E2EE_MIXED_CHANNELS`),
encrypted members are inaudible with `participantStreams: 0` (the mixed track cannot carry
them; the SDK emits an `error` when that happens), and the browser support matrix is in the
[feature chapter](../features/e2ee.md#browser-support). Media encryption underneath is still
DTLS-SRTP to the node plus the hop-by-hop AURX/relay encryption behind it
([Security model](../concepts/security.md)).
