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

### Multiple channels: transmission policy, focus, channel limit

A session may sit in several channels at once (team + party + proximity…). Two independent
controls decide where the microphone goes and how the channels are heard:

```ts
await client.joinChannel(teamId);
await client.joinChannel(partyId);

client.transmitToChannel(partyId);                     // speak to the party only, keep hearing the team
client.setTransmission({ type: 'all' });               // default: every joined channel
client.setTransmission({ type: 'none' });              // listen only (mic stays live locally)
client.transmitsTo(teamId);                            // would audio sent now reach `teamId`?
client.on('transmissionChanged', (mode) => updatePttIndicator(mode));

client.setChannelFocus(teamId);                        // team at full volume, the rest attenuated
client.setChannelFocus(undefined);                     // everything at full volume again
client.on('channelFocusChanged', (channelId) => highlight(channelId));
```

`setTransmission` is enforced on the server: frames for channels outside the policy are dropped
before fan-out, so `none` is a server-side push-to-talk release. `single` and the focus must
point at a joined channel; calling them before the join is fine (they are sent on the
`ChannelJoinAck`), and the server resets them (`TransmissionChanged {none}` /
`ChannelFocusChanged {}`) when that channel is left. Focus is receiver-local — other channels
are scaled by `media.unfocused_channel_gain` (0.5 by default), multiplied with per-participant
volume; local mutes and blocks still win. Both settings are replayed after a reconnect.

The server caps memberships per session (`media.max_channels_per_session`, default 10, and
`media.max_positional_channels_per_session`, default 1); `joinChannel` rejects with
`CHANNEL_LIMIT_EXCEEDED: …` when the cap is reached.

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

### Text chat (lite)

Real-time text rides on the same control WebSocket: channel messages to the channels you are a
member of, directed messages to a user who is online in the same app, and typing indicators.
There is no history, no offline delivery and no read state — it is party/team chat, `/`-commands
and map pings, not a messenger. Persistent blocks (`setUserBlocked`) suppress text exactly like
voice, in both directions.

```ts
client.on('chatMessage', (m) => {
  // m.own — my echo (the send promise resolves with the same object); m.system — from the server
  // (`POST /v1/channels/:id/messages`, fromUserId === SYSTEM_USER_ID); m.toUserId set for DMs
  render(m.channelId ?? 'dm', m.displayName, m.text, m.metadata);
});
client.on('participantTyping', (channelId, userId, typing) => showTyping(userId, typing));

const sent = await client.sendMessage(channelId, 'gg', {
  metadata: { ping: { x: 12.5, y: 8 } },   // any JSON, counts toward chat.max_message_bytes
  clientRef: localId,                       // optional; echoed only to you → reconcile optimistic UI
});
await client.sendDirectMessage(userId, '/w psst');   // target must be online in this app
client.setTyping(channelId, true);                   // call on every keystroke; coalesced to one frame per 1.5 s
client.setTyping(channelId, false);                  // always sent
```

`sendMessage`/`sendDirectMessage` resolve with the server-stamped message (`id`, `sentAt`) and
reject with `Error('<CODE>: <message>')`: `AUTH_DENIED` (not a member, or a block between the
two users), `USER_MUTED` (server-muted while `chat.server_mute_blocks_text`), `VALIDATION_ERROR`
(empty, too long, control characters, self-DM), `USER_OFFLINE`, `RATE_LIMIT_EXCEEDED` (anti-flood,
per session), `MESSAGE_BLOCKED` (content filter), `CHAT_DISABLED`. The server correlates a rejection with the send through the
`client_ref`, so only that promise fails — an unrelated `Error` frame is emitted as `serverError`.
Messages from the same sender arrive in order; you are not told about your own typing.

### Audio energy / voice activity

Remote levels come from the server (`ChannelEnergy`, derived from the `ssrc-audio-level` RTP
header extension the browser already sends): `participant.energy` is the last linear RMS `0..1`
(`0` = silent; decays to `0` about two intervals after the last frame) and the `energy` event
carries only the levels that changed (≥ 3 dB or a silence transition). Your own microphone can
be metered locally with Web Audio — no server round-trip, works while muted-by-server too:

```ts
const client = new AurixClient({ …, localVoiceActivity: true });
// or: localVoiceActivity: { threshold: 0.02, hangoverMs: 250, intervalMs: 50, smoothing: 0.5 }
client.on('energy', (channelId, levels) => levels.forEach(l => bar(l.user_id).width = toDb(l.energy)));
client.on('localEnergy', (s) => micMeter.width = toDb(s.energy));  // { energy, rms, speaking, changed }
client.on('localSpeaking', (on) => micIcon.hidden = !on);
client.localEnergy; client.localSpeaking;                          // current values
```

`encodeAudioLevel` / `decodeAudioLevel` convert between linear RMS and the RFC 6464 byte
(`0` = full scale, `127` = silence, else `-dBov`) if you want to display the same scale as the
server, and `VoiceActivityDetector` / `AudioLevelMeter` are exported for custom pipelines. The
meter is skipped silently where `AudioContext` is unavailable.

### Devices, input gain, speaker mute

```ts
const { inputs, outputs } = await AurixClient.enumerateAudioDevices(); // labels need a granted mic permission
client.on('devicesChanged', ({ inputs, outputs }) => refillSelects(inputs, outputs));

const client = new AurixClient({ …, inputDeviceId: micSelect.value, inputGain: 1.5 });
await client.setInputDevice(deviceId);   // hot-swaps the microphone (RTCRtpSender.replaceTrack, no renegotiation)
await client.setInputDevice(undefined);  // back to the system default
client.on('inputDeviceChanged', (id) => micSelect.value = id ?? '');
client.setInputGain(2);                  // 0..4 software gain (1 = unity) before encoding; independent of setMuted()

client.attachAudioOutput(audioElement);  // remote mix plays through the <audio>; several elements may be attached
if (AurixClient.supportsOutputSelection) await client.setOutputDevice(speakerSelect.value); // HTMLMediaElement.setSinkId
client.setOutputVolume(0.5);             // 0..1 master volume
client.setOutputMuted(true);             // speaker mute: you keep sending, remote audio is silenced locally
```

`setInputDevice` keeps the mute state, gain and local meter; if the requested device cannot be
opened the promise rejects and the current microphone keeps working. When the active
microphone disappears (track `ended`) the SDK falls back to the system default and emits
`inputDeviceChanged`. The first non-unity gain routes the microphone through a Web Audio
`GainNode` and sends the processed track (the raw track is sent until then). `setOutputDevice` rejects with
`NotFoundError` for unknown ids and throws where `setSinkId` is missing (Safari, Firefox without
`media.setsinkid.enabled`) — check `supportsOutputSelection` first. Output volume/mute are
applied to attached elements only (`element.volume` / `element.muted` + remote track
`enabled`), nothing is signalled to the server.

## How it maps to the server

| SDK | server |
|---|---|
| `new WebSocket(wsUrl, ['aurix', 'bearer.<jwt>'])` | JWT authenticated at upgrade; `SessionInitAck` carries `session_id`/`ssrc` |
| `connect()` → `WebRtcOffer` over WS | `SfuNode::attach_webrtc` (str0m, ICE-lite, host candidate = `media.external_ip:media.port`) |
| `GET /v1/me/turn-credentials` (optional) | time-limited TURN credentials for the browser's own relay candidates |
| `joinChannel()` → `ChannelJoin{channel_id, token}` | membership check against the token's channel claims |
| `setMuted()` → track `enabled` + `MuteStateChanged` | broadcast to channel members |
| `setTransmission()` → `SetTransmission{mode}` | `MediaSession::set_transmission`; routers drop frames outside the policy |
| `setChannelFocus()` → `SetChannelFocus{channel_id}` | `ReceiverPrefs::set_focus`; unfocused channels scaled in the per-receiver gain |
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
