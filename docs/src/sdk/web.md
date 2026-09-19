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
| Transcripts / TTS | `transcript`, `setTranscripts(bool)`, `isChannelTranscribed()`, `speak(text, {destination, voice, clientRef})` → `{ done }`, `ttsStatus`, `cancelSpeech()` | see [Transcripts and TTS](../features/speech.md) |
| Stats / quality | `getStats()` → `ClientStats`, `stats`, `networkQuality`, `client.networkQuality`, `qualityReportIntervalMs` | see [Network quality](../features/quality.md) |
| Errors | rejected promises carry `Error('<CODE>: <message>')`; unsolicited server errors arrive as `serverError` | codes listed in [Errors](../concepts/auth.md#errors) |

## How it maps to the server

| SDK | server |
|---|---|
| `new WebSocket(wsUrl, ['aurix', 'bearer.<jwt>'])` | JWT authenticated at upgrade; `SessionInitAck` carries `session_id` / `ssrc` |
| `connect()` → `WebRtcOffer` | `SfuNode::attach_webrtc` (str0m, ICE-lite, host candidate = `media.external_ip:media.port`) |
| `GET /v1/me/turn-credentials` (optional) | time-limited TURN credentials for the browser's relay candidates |
| `joinChannel()` → `ChannelJoin {channel_id, token}` | membership check against the token's channel claims / ad-hoc grant |
| `setMuted()` → track `enabled` + `MuteStateChanged` | broadcast to channel members |
| `getStats()` → `QualityReport` (loss in %) | `BitrateCommand` (applied through `RTCRtpSender.setParameters`) + merged `NetworkQuality` |
| `Ping` / `Pong` | keepalive + `roundTripMs`; two missed pongs close the socket |
| reconnect with `['aurix', 'bearer.<jwt>', 'resume.<session_id>.<resume_token>']` | `SessionInitAck {resumed: true}` + replayed `ChannelJoinAck`s within `server.session_resume_grace_secs` |

## End-to-end encryption

The Web SDK has no end-to-end encryption mode. Native AURX clients may mark frames with the
`E2ee` packet flag; the node forwards such frames untouched to native receivers only and never
decodes, mixes, records, streams or transcribes them — so they **do not reach browsers**. Media
encryption on the Web path is DTLS-SRTP to the node plus the hop-by-hop AURX/relay encryption
behind it (see [Security model](../concepts/security.md)).
