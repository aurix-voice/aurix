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

### Local mute, per-participant volume, block

These are *receiver-local*: they change what you hear, are enforced on the server before the
audio is forwarded to you, and the other player is never told.

```ts
client.setParticipantMuted(userId, true, channelId);  // silence them in one channel
client.setParticipantMuted(userId, true);             // …or in every channel
client.isParticipantMuted(userId, channelId);         // true if muted there or everywhere
client.setParticipantVolume(userId, 0.5);             // 0 (silence) … 1 (unity) … 2 (~+6 dB); RangeError otherwise
client.setUserBlocked(userId, true);                  // persistent, mutual; survives sessions and channels
client.on('userBlockChanged', (userId, blocked) => refreshBlockList(client.getBlockedUsers()));
client.on('receiverPreferences', (prefs) => { /* server state at session start (blocks from the DB) */ });
```

Mutes and volumes live for the session and are replayed automatically after a non-resumed
reconnect (channel-scoped mutes once the channel is re-joined); blocks are stored per application
in the server database and also manageable from your backend via `/v1/users/:id/blocks`.

### Reconnect / session resume

Enabled by default (`autoReconnect: true`). When the control connection drops the client keeps
its session state and reconnects with exponential backoff (`reconnect: { initialDelayMs: 500,
maxDelayMs: 8000, factor: 2, jitter: 0.3, maxAttempts: 10 }`), presenting the one-time resume
token from `SessionInitAck`:

```ts
client.on('recovering', (attempt, delayMs, cause) => showBanner(`reconnecting… (${attempt})`));
client.on('recovered', (info) => hideBanner(info.resumed ? 'resumed' : 'rejoined'));
client.on('failedToRecover', (err) => showError(err));   // state is now 'failed'
client.on('sessionClosed', (reason) => { /* kicked/banned/shutdown: no reconnect */ });
```

* Within the server's grace window (`client.resumeGrace`, default 30 s) the same session comes
  back (`info.resumed === true`): same SSRC, same channels — peers never see a leave. The server
  replays one `ChannelJoinAck` per channel, so `channelJoined` fires again with a fresh roster.
  A still-connected `RTCPeerConnection` is kept; otherwise a new offer is negotiated.
* After the grace window the server hands out a fresh session (`info.resumed === false`); the
  client emits `channelLeft` for the old channels and re-joins them with the same token.
* Two missed `Pong`s close the socket proactively so half-open connections are detected within
  ~2.5 × `pingIntervalMs`. `reconnectNow()` skips the current backoff delay (e.g. on `online`).
* `disconnect()` cancels any pending reconnect; a `SessionClose` from the server never reconnects.

### One-time action tokens

Your backend can mint single-use tokens with `POST /v1/tokens/action` (90 s by default) instead
of handing the reusable player JWT to the client. Each token authorises exactly one `login`,
`join`, `kick`, `mute` or `unmute`; a replay — on any node — fails with `TOKEN_REUSED`. When the
server runs with `auth.require_action_tokens = true` they are mandatory for `connect()` and
`joinChannel()`.

```ts
const client = new AurixClient({
  apiUrl, wsUrl,
  token: await backend.actionToken({ action: 'login' }),          // one-time login credential
  refreshToken: () => backend.actionToken({ action: 'login' }),   // fresh one before each reconnect
  joinToken: (channelId) => backend.actionToken({ action: 'join', channel_id: channelId, speak: true }),
});
await client.connect();
await client.joinChannel(channelId);                 // uses joinToken(channelId)
await client.joinChannel(channelId, explicitToken);  // …or pass one yourself

// in-game moderation: the backend mints a kick/mute/unmute token bound to actor + channel + target
const kick = await backend.actionToken({ action: 'kick', channel_id, target_user_id: userId });
await client.moderate(channelId, userId, 'kick', kick, 'afk');    // resolves on ModerateParticipantAck
```

A `login` token that opened a session may still be presented for a *resume* of that same session
within the grace window; `refreshToken` is only needed when the resume fails and a fresh session
has to be opened (the client calls it before every reconnect attempt, so keep it cheap). Without
`refreshToken` the current `token` is reused — fine for player JWTs, not for consumed action
tokens. A `login` token also authenticates `GET /v1/me/*` (TURN credentials) while it is within
its TTL, which is exactly when the SDK fetches them.

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
| reconnect with `['aurix', 'bearer.<jwt>', 'resume.<session_id>.<resume_token>']` | `SessionInitAck{resumed: true}` + replayed `ChannelJoinAck`s within `server.session_resume_grace_secs` |

Native AURX-over-UDP is not available from browsers (no raw UDP); the native path is used by the
Unity SDK (`sdk/unity`) and the Rust load generator.

## Requirements

* The API origin must list the page origin in `AURIX__SERVER__CORS_ORIGINS`.
* Browsers only allow `getUserMedia` on `https://` or `http://localhost`.
* `media.external_ip` must be reachable from the browser (UDP `media.port`), or a TURN server
  (built-in `AURIX__TURN__*`) must be reachable.
