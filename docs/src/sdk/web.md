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

Players should connect to the nearest node with capacity — and reconnect to the *same* node,
because session resume is node-local. Either pass the `endpoint.ws_url` your backend receives
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
| Action tokens | `token`, `refreshToken()`, `joinToken(channelId)`, `moderate(channelId, userId, 'kick'\|'mute'\|'unmute', token, reason)` | mandatory under `auth.require_action_tokens` |
| Local mute / volume / block | `setParticipantMuted(userId, muted, channelId?)`, `setParticipantVolume(userId, 0..2)`, `setUserBlocked(userId, blocked)`, `receiverPreferences`, `userBlockChanged` | enforced server-side before fan-out; replayed after a non-resumed reconnect |
| Multiple channels | `setTransmission({type:'all'\|'none'\|'single', channelId?})`, `transmitToChannel(id)`, `setChannelFocus(id?)`, `transmissionChanged`, `channelFocusChanged` | `CHANNEL_LIMIT_EXCEEDED` when `media.max_channels_per_session` is hit |
| Positional / directional | `updatePosition(channelId, {x,y,z}, {forward_*, up_*})`, `positions` | stereo downlink (`stereo=1`) — do not re-pan |
| Energy / VAD | `energy`, `localVoiceActivity: true \| {...}`, `localEnergy`, `localSpeaking` | remote levels from the server's `ChannelEnergy` |
| Devices | `AurixClient.enumerateAudioDevices()`, `setInputDevice()`, `setInputGain(0..4)`, `attachAudioOutput(el)`, `setOutputDevice()` (`supportsOutputSelection`), `setOutputVolume()`, `setOutputMuted()`, `devicesChanged`, `inputDeviceChanged` | mic hot-swap via `replaceTrack`, no renegotiation |
| Echo test / injection | echo channel + `injectAudio(AudioBuffer \| MediaStream, {loop, gain, mixWithMicrophone})`, `stopAudioInjection()`, `decodeAudio()`, `audioInjection` | same Web Audio graph as the input gain |
| Chat lite | `sendMessage()`, `sendDirectMessage()`, `setTyping()`, `chatMessage`, `participantTyping` | see [Text chat](../features/chat.md) |
| Transcripts / TTS | `transcript`, `setTranscripts(bool)`, `isChannelTranscribed()`, `isChannelMonitored()`, `speak(text, {destination, voice, clientRef})` → `{ done }`, `ttsStatus`, `cancelSpeech()` | see [Transcripts and TTS](../features/speech.md) |
| Stats / quality | `getStats()` → `ClientStats`, `stats`, `networkQuality`, `client.networkQuality`, `qualityReportIntervalMs`, `bitrate` | see [Network quality](../features/quality.md) |
| Opus controls | `opus: {maxBitrateBps, fec, dtx, maxBandwidth, cbr, followChannelPolicy}`, `setOpusOptions()`, `audioPolicy` / `channelAudioPolicy(id)`, `opusPreferences`, `negotiatedOpus`, `renegotiateMedia()`, `audioPolicy` event | see [Opus in the browser](#opus-in-the-browser) |
| Errors | rejected promises carry `Error('<CODE>: <message>')`; unsolicited server errors arrive as `serverError` | codes listed in [Errors](../concepts/auth.md#errors) |

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

## How it maps to the server

| SDK | server |
|---|---|
| `new WebSocket(wsUrl, ['aurix', 'bearer.<jwt>'])` | JWT authenticated at upgrade; `SessionInitAck` carries `session_id` / `ssrc` |
| `connect()` → `WebRtcOffer` | `SfuNode::attach_webrtc` (str0m, ICE-lite, host candidate = `media.external_ip:media.port`) |
| `GET /v1/me/turn-credentials` (optional) | time-limited TURN credentials for the browser's relay candidates |
| `joinChannel()` → `ChannelJoin {channel_id, token}` | membership check against the token's channel claims / ad-hoc grant |
| `setMuted()` → track `enabled` + `MuteStateChanged` | broadcast to channel members |
| `getStats()` → `QualityReport` (loss in %) | `BitrateCommand` (applied through `RTCRtpSender.setParameters`, within the channel policy) + merged `NetworkQuality` |
| answer `fmtp` rewrite (`useinbandfec`, `usedtx`, `maxplaybackrate`, `maxaveragebitrate`) | `ChannelJoinAck.audio` / `ChannelAudioPolicy` from the channel config |
| `Ping` / `Pong` | keepalive + `roundTripMs`; two missed pongs close the socket |
| reconnect with `['aurix', 'bearer.<jwt>', 'resume.<session_id>.<resume_token>']` | `SessionInitAck {resumed: true}` + replayed `ChannelJoinAck`s within `server.session_resume_grace_secs` |

## End-to-end encryption

The Web SDK has no end-to-end encryption mode. Native AURX clients may mark frames with the
`E2ee` packet flag; the node forwards such frames untouched to native receivers only and never
decodes, mixes, records, streams or transcribes them — so they **do not reach browsers**. Media
encryption on the Web path is DTLS-SRTP to the node plus the hop-by-hop AURX/relay encryption
behind it (see [Security model](../concepts/security.md)).
