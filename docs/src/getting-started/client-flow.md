# Client flow

The SDKs hide all of this; read it to understand what they do, or to write your own client.
The full message set is the `ControlMessage` enum in `crates/aurix-common/src/protocol.rs`
(see [WebSocket control plane](../api/websocket.md)).

1. **Open the control channel.** Connect to `ws://host:8081/ws` with the player JWT
   (`Authorization: Bearer`, the `Sec-WebSocket-Protocol: aurix, bearer.<jwt>` sub-protocol for
   browsers, or `?token=` as a last resort) or with a one-time `login` action token. The server
   replies `SessionInitAck { session_id, ssrc, media_addr, media_key, resume_token, resume_grace_ms }`.
2. **Native clients** send an authenticated `SessionBind` datagram to `media_addr`
   (`AurixPacket::session_bind(...).encode_authenticated(media_key)`), wait for
   `SessionBindAck` / `MediaBound`, then `ChannelJoin { channel_id, token }` over WebSocket and
   start sending `Audio` packets (Opus, 20 ms, `encode_authenticated`). `token` is either the
   player JWT (must list the channel) or a one-time `join` action token for exactly that channel.
   Details: [Native AURX media](../api/aurx.md).
3. **Browsers** send `ChannelJoin`, then `WebRtcOffer { sdp }` → `WebRtcAnswer { sdp }`
   (or `POST /v1/webrtc/offer` with the player JWT); ICE servers come from
   `GET /v1/me/turn-credentials`.
4. **Positional channels**: send `PositionUpdate` (own position + orientation; moderators may move
   anyone) — see [Channels and audio routing](../features/channels.md). `SpeakingStateChanged`,
   `ParticipantJoined/Left`, `RecordingNotification`, `Kick`, `SessionClose` arrive as events.
5. **In-game moderation**: `ModerateParticipant { channel_id, user_id, action, token }` with a
   `kick`/`mute`/`unmute` action token minted by your backend for the acting player →
   `ModerateParticipantAck` (or `Error`). The token binds actor, channel and target, so a
   client cannot redirect it.
6. **Reconnect.** If the WebSocket drops without a Close frame the session stays alive for
   `resume_grace_ms` (`AURIX__SERVER__SESSION_RESUME_GRACE_SECS`, default 30). Reconnect with the
   same JWT plus `X-Aurix-Resume: <session_id>.<resume_token>` (browsers: sub-protocol
   `resume.<session_id>.<resume_token>`) and the server answers `SessionInitAck { resumed: true }`
   with the same session, SSRC and media key, followed by one `ChannelJoinAck` per channel still
   joined — peers never see a leave. Resume tokens are one-time (rotated on every ack), bound to
   the user/app of the JWT and useless once the server closed the session (kick/ban/shutdown) or
   the grace period expired; then the same handshake simply yields a fresh session. Native
   clients re-send `SessionBind` from their (possibly new) UDP port. The SDKs do this
   automatically with exponential backoff and expose `Recovering` / `Recovered` /
   `FailedToRecover` events.
7. **Receiver-local mute / volume / block**: `SetParticipantMute { user_id, channel_id?, muted }`
   silences one player for *you only* (in one channel or, with `channel_id: null`, everywhere),
   `SetParticipantVolume { user_id, volume }` scales them for you (`0.0`–`2.0`, `1.0` = unity,
   multiplied with positional attenuation into the per-packet volume byte / WebRTC gain), and
   `SetUserBlock { user_id, blocked }` is a persistent, mutual cross-mute stored in `user_blocks`
   per application: neither side hears the other in any channel, on any node, in any future
   session. All three are enforced on the server before media is forwarded (native, WebRTC and
   cascade paths alike) and never reach the affected sender — no `MuteStateChanged` is
   broadcast, unlike the sender-side `SetMute` and the moderator mute. A fresh session starts
   with `ReceiverPreferences { blocked_users, local_mutes, volumes }` (blocks come from the
   database; mutes/volumes are replayed by the SDKs); the blocker gets `UserBlockChanged` acks.
8. **Text chat (lite)**: `ChatSend`, `ChatSendDirect`, `ChatTyping` → `ChatMessageReceived`,
   `ParticipantTyping`; see [Text chat](../features/chat.md).
9. **Audio energy / voice activity**: clients label frames with their microphone level; the
   server emits `SpeakingStateChanged` and periodic `ChannelEnergy`; see
   [Channels and audio routing](../features/channels.md#speaking-energy-and-roster).
10. **Devices, input gain, speaker mute** are client-side only (nothing on the wire) — see the
    SDK chapters.
11. **Quality**: clients send `QualityReport` every 5 s and receive `NetworkQuality` /
    `BitrateCommand`; see [Network quality and statistics](../features/quality.md).
12. **Leaving**: `ChannelLeave`, then close the WebSocket with a Close frame (a plain drop keeps
    the session in the resume grace period). `SessionClose { reason }` from the server means the
    session is gone for good (kick, ban, erasure, shutdown).

## Who talks to whom

```
 game backend ──API key──▶ REST: apps, channels, tokens, moderation, recordings, webhooks
 game client  ──player JWT──▶ WebSocket control plane + UDP/WebRTC media
 game server  ──API key──▶ SSE /v1/events or signed webhooks ◀── Aurix
```

API keys never leave your backend. Players only ever hold a short-lived JWT (or one-time action
tokens) scoped to the channels you granted.
