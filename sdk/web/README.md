# @aurix/web-sdk

Browser client for [Aurix](../../README.md): JSON control channel over WebSocket and a single
WebRTC peer connection (Opus) carrying the microphone uplink and the server-mixed downlink.
No runtime dependencies; ES2020 module with type declarations.

```bash
npm install && npm run build      # -> dist/
npm run demo                      # http://localhost:5173/  (demo/index.html)
```

## Usage

```ts
import { AurixClient } from '@aurix/web-sdk';

const client = new AurixClient({
  apiUrl: 'https://voice.example.com',      // REST (TURN credentials)
  wsUrl: 'wss://voice.example.com/ws',      // control channel
  token,                                    // player JWT from your backend (POST /v1/tokens)
});

client.on('remoteStream', (stream) => { audioElement.srcObject = stream; });
client.on('participantJoined', (channelId, p) => console.log('joined', p.displayName));
client.on('speaking', (channelId, userId, speaking) => { /* UI indicator */ });
client.on('recording', (channelId, recordingId, active) => {
  if (active) client.respondToRecording(recordingId, 'accepted');
});

await client.connect();                     // SessionInitAck + getUserMedia + WebRTC offer/answer
const participants = await client.joinChannel(channelId);
client.setMuted(true);
client.updatePosition(channelId, { x: 0, y: 0, z: 0 }, { forward_x: 0, forward_y: 0, forward_z: 1, up_x: 0, up_y: 1, up_z: 0 });
client.disconnect();
```

## How it maps to the server

| SDK | server |
|---|---|
| `new WebSocket(wsUrl, ['aurix', 'bearer.<jwt>'])` | JWT authenticated at upgrade; `SessionInitAck` carries `session_id`/`ssrc` |
| `connect()` → `WebRtcOffer` over WS | `SfuNode::attach_webrtc` (str0m, ICE-lite, host candidate = `media.external_ip:media.port`) |
| `GET /v1/me/turn-credentials` (optional) | time-limited TURN credentials for the browser's own relay candidates |
| `joinChannel()` → `ChannelJoin{channel_id, token}` | membership check against the token's channel claims |
| `setMuted()` → track `enabled` + `MuteStateChanged` | broadcast to channel members |
| `reportQuality()` → `QualityReport` | adaptive `BitrateCommand`, applied via `RTCRtpSender.setParameters` |
| `Ping`/`Pong` | keepalive + `roundTripMs` |

Native AURX-over-UDP is not available from browsers (no raw UDP); the native path is used by the
Unity SDK (`sdk/unity`) and the Rust load generator.

## Requirements

* The API origin must list the page origin in `AURIX__SERVER__CORS_ORIGINS`.
* Browsers only allow `getUserMedia` on `https://` or `http://localhost`.
* `media.external_ip` must be reachable from the browser (UDP `media.port`), or a TURN server
  (built-in `AURIX__TURN__*`) must be reachable.
