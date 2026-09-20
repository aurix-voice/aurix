# @aurix/web-sdk

Browser client for [Aurix](../../README.md): JSON control channel over WebSocket and a single
WebRTC peer connection (Opus) carrying the microphone uplink and the server-mixed downlink.
No runtime dependencies; ES2020 module with type declarations.

```bash
npm install && npm run build      # -> dist/
npm run demo                      # http://localhost:5173/  (demo/index.html)
npm test                          # unit tests (node --test) for the quality model
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

### Choosing a region

Players should talk to the nearest node with capacity — and reconnect to the *same* node, since
session resume is node-local. Either take the `endpoint` your backend gets from `POST /v1/tokens`
(pass `region`/`location` hints there), or let the browser measure:

```ts
import { AurixClient, discoverRegions } from '@aurix/web-sdk';

const { recommended, regions } = await discoverRegions({
  apiUrl: 'https://voice.example.com',   // any node / shared API hostname
  token,                                 // player JWT → GET /v1/me/regions
  region: partyLeaderRegion,             // optional: ranks first when reachable
  location: { latitude: 48.9, longitude: 2.3 }, // optional: server orders by distance
  // probe: true (default) → GET each region's probe_url a few times, best RTT wins
});
// regions[i] = { region, node_id, ws_url, probe_url, distance_km, nodes, load_factor, rttMs }
const client = new AurixClient({ apiUrl, wsUrl: recommended!.ws_url, token });
```

Ranking: preferred region (if its probe succeeded) → measured RTT in 15 ms buckets (ties keep the
server's distance/load order) → unprobed regions. Regions whose probe failed rank last.
`rankRegions`/`probeRtt` are exported for custom policies. Only healthy nodes with a public
`wss://` URL are advertised; an empty list means no node is configured for discovery.

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

### Positional / directional audio

In a `positional` channel the server places every speaker for every listener from the poses
the clients publish, so call `updatePosition` whenever the local player moves or turns (a few
times per second is plenty):

```ts
// position in world units, orientation = the player's forward and up vectors
client.updatePosition(arenaId, { x: 12.5, y: 0, z: -3 },
  { forward_x: 0, forward_y: 0, forward_z: 1, up_x: 0, up_y: 1, up_z: 0 });
client.on('positions', (channelId, positions) => { /* others' poses, if you want to mirror them */ });
```

Nothing is heard until both sides have reported a pose, nothing beyond the channel's
`max_radius`, and the distance roll-off (`near_distance` → `far_distance`) is applied on the
server together with your participant volumes and channel focus. When the channel was created
with `positional_config.directional: true` the downlink is a **stereo** mix: each speaker is
panned by their azimuth relative to your orientation (constant-power, elevation is reported but
not rendered), so a teammate on your right stays on your right when you turn. Left/right follow
the channel's `coordinate_system` (`left_handed` — Unity/Unreal — by default, `right_handed` for
OpenGL/Three.js/Godot conventions). The SDK offers Opus with `stereo=1` so the browser decodes
both channels; play the remote stream through a stereo output (or `attachAudioOutput`) and do
not re-pan it yourself. Your microphone is still sent in mono.

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
* If the node itself is gone, reconnect attempts rotate through the failover nodes the server
  advertised (`client.failover`; attempt 1 → current node, 2 → first failover, 3 → second, …).
  A node that answers takes the session over from its Redis mirror: `endpointChanged(url)`
  fires, then `recovered` with `info.migrated === true` — same session id and SSRC, new media
  key/endpoint, channels and preferences restored, peers see no leave; a new
  `RTCPeerConnection` is negotiated with the new node. `client.endpoint` names the node in use.
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

### Echo channel (mic test) & audio injection

```ts
// backend: POST /v1/channels {"name":"mic-test","config":{"channel_type":"echo"}}
await client.joinChannel(echoChannelId);   // you hear only yourself, nobody hears you

const clip = await client.decodeAudio(await (await fetch('/sfx/test.ogg')).arrayBuffer());
client.injectAudio(clip, { loop: true, gain: 0.8 });        // mixed over the microphone
client.injectAudio(clip, { mixWithMicrophone: false });     // replaces the microphone until it ends
client.injectAudio(radioElement.captureStream(), { gain: 1 }); // any live MediaStream (TTS, radio)
client.on('audioInjection', (active) => testButton.disabled = active);
client.injectingAudio;                     // true while something plays
client.stopAudioInjection();               // microphone is audible again immediately
```

An `echo` channel loops each participant's own frames back through the real uplink → server →
downlink path (so a level meter on `remoteStream` is the real round trip) and never forwards
them to anybody else or to other nodes. Injection feeds the uplink through the same Web Audio
graph as the input gain (`AudioBufferSourceNode` / `MediaStreamAudioSourceNode` → `GainNode`
summed into the sent track), so there is no renegotiation and it goes to every channel the
microphone goes to (transmission mode, focus and channel limits apply unchanged); `setMuted(true)`
silences microphone and injection together, `setInputGain` scales the microphone only. A new
`injectAudio` replaces the current one; a one-shot buffer ends on its own (`audioInjection(false)`),
a stream plays until `stopAudioInjection()` or `disconnect()`. `injectAudio` needs media
(`connect()` first) and Web Audio; `decodeAudio` accepts whatever `decodeAudioData` decodes.

### Transcripts & text-to-speech

```ts
// backend: channel config {"transcription": true} + [stt] configured on the server
client.on('transcript', (t) => captions.append(t.userId, t.text, t.startedAt, t.words));
client.isChannelTranscribed(channelId);      // from ChannelJoinAck.transcription
client.isChannelMonitored(channelId);        // ChannelJoinAck.safety_voice: speech is analysed by the [safety] classifier — disclose it
client.setTranscripts(false);                // stop receiving captions (survives reconnect)
client.transcriptsEnabled;                   // true by default

// [tts] configured on the server (GET /v1/tts/voices lists voices and limits)
const req = await client.speak('Enemy spotted at B', { destination: 'channel', voice: 'nova' });
client.on('ttsStatus', (s) => console.log(s.clientRef, s.state, s.durationMs, s.message));
const final = await req.done;                // finished | cancelled | failed
await client.speak('Reading your message…', { destination: 'local' }); // only you hear it
client.cancelSpeech();                       // drops everything still queued or playing
```

Transcripts arrive only for channels the operator marked `transcription: true`, only from
participants you would hear (local mute, block and zero gain suppress their captions), never
for end-to-end-encrypted audio, and are not stored by the server — the event is your only copy.
`speak()` resolves once the server queued the request (rejects with `<CODE>: <message>` for
`FEATURE_DISABLED`, `AUTH_DENIED` not a member, `USER_MUTED`, `VALIDATION_ERROR` too long /
unknown voice / control characters / ambiguous channel, `RATE_LIMIT_EXCEEDED` queue or per-minute
budget, `MESSAGE_BLOCKED` by the content filter); `channelId` may be omitted when the session
transmits to exactly one channel. Synthesized speech is routed exactly like your microphone
(transmission mode, mutes, blocks, focus, other nodes) and reaches browsers inside the mixed
downlink; statuses go to the requesting session only. Disconnecting cancels pending requests.

### Live translation

```ts
// server: [translation] configured; the node advertises what it offers
const t = client.sessionInfo?.translation;         // { speech, languages } or undefined
if (t && (t.languages.length === 0 || t.languages.includes('de'))) {
  client.setTranslation('de', { spokenLanguage: 'en', speech: t.speech });
}
client.on('translationChanged', (p) => console.log(p.language, p.spokenLanguage, p.speech));
client.on('transcript', (t) => {
  if (t.original) captions.append(t.userId, t.text, t.language, `(${t.original.language}: ${t.original.text})`);
  else captions.append(t.userId, t.text, t.language);
});
client.translationPrefs;                           // as applied by the server (normalised tags)
client.setTranslation(undefined);                  // back to originals only
```

`setTranslation` translates the captions *you* receive; the speaker and listeners of other
languages keep getting theirs. A segment already in your language, one the provider could not
translate in time, or one longer than the node's limit arrives as the original (no `original`
field). With `speech: true` the translation is also spoken to you alone inside your WebRTC
downlink. Tags are BCP-47 (`DE_de` → `de-de`); the server answers `VALIDATION_ERROR` for a
language it does not offer and `TRANSLATION_DISABLED` when translation is off. The preference
survives reconnects and failover.

### Statistics & network quality

```ts
const s = await client.getStats();      // RTCPeerConnection.getStats(), normalized
s.bars;                                 // 1 (unusable) … 5 (excellent), from s.rFactor
s.rFactor; s.mos;                       // simplified E-model rating (0..100) and MOS (1..4.5)
s.rttMs; s.rttMinMs; s.rttAvgMs; s.rttMaxMs; // application RTT (Ping/Pong), session min/avg/max
s.iceRttMs;                             // ICE candidate-pair RTT
s.jitterMs; s.lossPercent;              // downlink: RFC 3550 jitter, loss of the last period (0..100)
s.packetsReceived; s.packetsLost; s.bytesReceived; s.packetsDiscarded; s.concealedSamples;
s.jitterBufferDelayMs;                  // average jitter-buffer delay
s.packetsSent; s.bytesSent; s.remoteLossPercent; s.remoteJitterMs; // uplink, from the SFU's RTCP
s.server;                               // last server-side NetworkQuality (both directions), if any

client.on('stats', (s) => hud.setBars(s.bars));          // before each periodic QualityReport
client.on('networkQuality', (q) => hud.setServerBars(q.bars, q.uplinkLossPercent));
client.networkQuality;                                   // last server report
```

Every `qualityReportIntervalMs` (5 s; `0` disables) the client samples the peer connection,
fires `stats` and sends a `QualityReport` (`rtt_ms`, `jitter_ms`, `packet_loss` **in percent**),
which drives the server's adaptive bitrate and its own `NetworkQuality` message. The server
merges that downlink view with what the SFU measures on your uplink (sequence gaps, RFC 3550
jitter, bitrate) and picks the worse direction: `R ≥ 80` → 5 bars, `≥ 70` → 4, `≥ 60` → 3,
`≥ 50` → 2, else 1. `bars`/`rFactor`/`mos`/`lossPercent` describe the last period; packet/byte
counters are cumulative for the peer connection. The same helpers (`rFactor`, `mosFromR`,
`barsFromR`, `assembleClientStats`) are exported for HUDs that read raw stats themselves.

### Opus controls (what a browser lets you set)

```ts
const client = new AurixClient({
  // ...
  opus: {
    maxBitrateBps: 32_000,   // ceiling: RTCRtpSender maxBitrate + fmtp maxaveragebitrate
    fec: true,               // fmtp useinbandfec
    dtx: false,              // fmtp usedtx
    maxBandwidth: 'wideband',// fmtp maxplaybackrate=16000
    cbr: false,              // fmtp cbr (local only; default VBR)
    stereo: false,           // fmtp stereo=1 + 2-channel mic track (music sources; needs a stereo channel policy)
    followChannelPolicy: true, // unset fields come from the server's channel policy (default)
  },
});

client.on('audioPolicy', (p) => {
  // merged policy of all joined channels: bitrateBps, minBitrateBps, fec, dtx, maxBandwidth,
  // complexity?, signal, stereo — as configured by the operator (ChannelJoinAck.audio / ChannelAudioPolicy)
  console.log(p, client.opusPreferences, client.negotiatedOpus);
});
client.on('bitrate', (kbps, reason, expectedLossPercent) => { /* server adaptation */ });

client.setOpusOptions({ maxBandwidth: 'superwideband' }); // bitrate part applies live …
await client.renegotiateMedia();                          // … fmtp part at the next negotiation
```

Everything goes through WebRTC, so only these controls exist: the bitrate ceiling (live, via
`setParameters`, and `maxaveragebitrate` at negotiation), in-band FEC, DTX, maximum bandwidth
, CBR and stereo — all `fmtp` parameters of the Opus payload in the server's answer (RFC 7587: the
receiver states what the sender should do). **Complexity, signal mode, VBR mode and expected
loss are owned by the browser** and cannot be set; the policy still exposes them for parity with
the native SDKs. The server's `BitrateCommand` moves the ceiling within the policy's
`minBitrateBps..=bitrateBps`; the browser's congestion control keeps running underneath. Helpers
(`parseAudioPolicy`, `mergeAudioPolicies`, `resolveOpusSenderPreferences`,
`applyOpusSenderPreferences`, `negotiatedOpusPreferences`, `defaultAudioConstraints`) are
exported for custom pipelines.

`opus.stereo: true` is for music sources (a DJ deck, a stereo interface, a bot): it is honoured
only when the channel policy has `stereo: true` (or `followChannelPolicy: false`), rewrites
`stereo=1` into the answer and — unless you pass `audioConstraints` — opens the microphone with
`channelCount: {ideal: 2}` and echo cancellation / noise suppression / auto-gain **off**, since the
browser's voice processing downmixes to mono. Takes effect at the next negotiation
(`renegotiateMedia()`); whether two channels really go out also depends on the device and the
browser's Opus implementation. Voice channels stay mono whatever the client asks.

## How it maps to the server

| SDK | server |
|---|---|
| `new WebSocket(wsUrl, ['aurix', 'bearer.<jwt>'])` | JWT authenticated at upgrade; `SessionInitAck` carries `session_id`/`ssrc` |
| `connect()` → `WebRtcOffer` over WS | `SfuNode::attach_webrtc` (str0m, ICE-lite, host candidate = `media.external_ip:media.port`) |
| `GET /v1/me/turn-credentials` (optional) | time-limited TURN credentials for the browser's own relay candidates |
| `discoverRegions()` → `GET /v1/me/regions` + `GET <probe_url>` | `NodeManager::regions`: healthy, non-saturated nodes with a public `ws_url`, least-loaded node per region |
| `joinChannel()` → `ChannelJoin{channel_id, token}` | membership check against the token's channel claims |
| `setMuted()` → track `enabled` + `MuteStateChanged` | broadcast to channel members |
| `setTransmission()` → `SetTransmission{mode}` | `MediaSession::set_transmission`; routers drop frames outside the policy |
| `setChannelFocus()` → `SetChannelFocus{channel_id}` | `ReceiverPrefs::set_focus`; unfocused channels scaled in the per-receiver gain |
| `getStats()` / `reportQuality()` → `QualityReport` (loss in %) | adaptive `BitrateCommand` (applied via `RTCRtpSender.setParameters`, clamped to the channel policy) + `NetworkQuality` merged with the SFU's uplink measurements |
| Opus `fmtp` rewrite of the answer (`useinbandfec`, `usedtx`, `maxplaybackrate`, `maxaveragebitrate`, `cbr`) | `ChannelJoinAck.audio` / `ChannelAudioPolicy` derived from `ChannelConfig` |
| `Ping`/`Pong` | keepalive + `roundTripMs` |
| reconnect with `['aurix', 'bearer.<jwt>', 'resume.<session_id>.<resume_token>']` | `SessionInitAck{resumed: true}` + replayed `ChannelJoinAck`s within `server.session_resume_grace_secs` |

Native AURX-over-UDP is not available from browsers (no raw UDP); the native path is used by the
Unity SDK (`sdk/unity`) and the Rust load generator.

## Standalone bundle and the handle bridge (Unity WebGL, non-JS hosts)

`npm run build` also emits `dist/aurix-web-sdk.js`: the whole SDK as one dependency-free classic
script (no module system) that defines `window.AurixWebSdk` (`AurixClient`, `AurixBridge`,
`version`, the wire helpers). It is what the Unity WebGL plugin loads
(`sdk/unity/Runtime/Plugins/WebGL/AurixWebGL.jslib`) and what any host that cannot import ES modules
can use with a plain `<script>` tag. The file is generated, not committed.

`AurixBridge` is a façade for code that can only exchange strings — wasm/Emscripten hosts, engine
scripting layers, `postMessage` — instead of holding JS objects:

```js
const bridge = new AurixWebSdk.AurixBridge();          // { maxQueuedEvents, document, createClient }
const h = bridge.create(JSON.stringify({ apiUrl, wsUrl, token, refreshToken: true }));
bridge.invoke(h, 'connect', '{}', 1);                  // → {"ok":true,"pending":true}
bridge.invoke(h, 'isMuted', '{}');                     // → {"ok":true,"value":false}
bridge.drain(h);                                       // → JSON array of queued events, oldest first
// [{"type":"connectionState","state":"connecting"}, {"type":"result","rid":1,"ok":true,"value":{…session…}},
//  {"type":"tokenRequest","requestId":1,"kind":"refresh","channelId":null}, …]
bridge.invoke(h, 'provideToken', JSON.stringify({ requestId: 1, token: freshJwt }));
bridge.destroy(h);
```

* `invoke(handle, method, argsJson, rid?)` covers every `AurixClient` method by name with camelCase
  JSON arguments (`{"channelId": "...", "joinToken": "..."}`); synchronous methods return
  `{"ok":true,"value":…}` immediately, promise-returning ones return `{"ok":true,"pending":true}` and
  later queue `{"type":"result","rid":N,"ok":true|false,…}`. Errors are
  `{"ok":false,"error":{"message","name","code?"}}`.
* `drain(handle)` returns the ordered event queue (one entry per client event, same names and
  payloads as `client.on(...)`), plus `result`, `tokenRequest`, `remoteAudio` (playback state of the
  hidden `<audio>` element the bridge attaches to the remote stream; `resumeAudio` retries after an
  autoplay block) and `overflow` (`{"dropped":N}` when the bounded queue, default 4096, wrapped).
  `pending(handle)` is the queue length.
* `refreshToken: true` / `joinToken: true` in the options invert the token callbacks: the client
  queues a `tokenRequest` and waits for `provideToken` (`{"requestId","token"}` or
  `{"requestId","error"}`) — the host supplies tokens, the bridge never holds credentials logic.

The `AurixBridge` contract is covered by `test/bridge.test.mjs`; `test/unity-jslib.test.mjs` runs the
Unity plugin against the built bundle under an Emscripten-like harness.

## Requirements

* The API origin must list the page origin in `AURIX__SERVER__CORS_ORIGINS`.
* Browsers only allow `getUserMedia` on `https://` or `http://localhost`.
* `media.external_ip` must be reachable from the browser (UDP `media.port`), or a TURN server
  (built-in `AURIX__TURN__*`) must be reachable.
