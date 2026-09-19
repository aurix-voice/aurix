# WebSocket control plane

Every player holds one WebSocket to `ws(s)://host:8081/ws`. Messages are JSON objects
`{"type": "<Variant>", "data": {…}}` mirroring the `ControlMessage` enum in
`crates/aurix-common/src/protocol.rs` — that file is the normative reference; this chapter
explains the lifecycle and lists the messages by purpose.

## Authentication and handshake

The credential is a player JWT (`POST /v1/tokens`) or a one-time `login` action token, passed as
(in order of preference):

1. `Authorization: Bearer <token>` header (native clients);
2. `Sec-WebSocket-Protocol: aurix, bearer.<token>` sub-protocol (browsers cannot set headers);
3. `?token=<token>` query parameter — avoid, it ends up in proxy logs.

Immediately after the upgrade the server sends

```json
{"type":"SessionInitAck","data":{"session_id":"…","ssrc":123456,"media_addr":"203.0.113.10:10000",
  "media_key":"<base64, 32 bytes>","resume_token":"…","resume_grace_ms":30000,"resumed":false}}
```

`media_key` is the master secret for the [AURX media path](aurx.md); `media_addr` is the UDP
endpoint of *this* node. With `AURIX__AUTH__REQUIRE_ACTION_TOKENS=true` a plain player JWT is
refused for the handshake (`ACTION_TOKEN_REQUIRED`).

`Ping { nonce }` / `Pong { nonce }` keep the connection alive; the server also drops sessions
whose media path stops heart-beating.

## Resume

Reconnect within `resume_grace_ms` with the same JWT plus `X-Aurix-Resume: <session_id>.<resume_token>`
(browsers: sub-protocol `resume.<session_id>.<resume_token>`, query `resume=` as a fallback).
On success `SessionInitAck.resumed` is `true`, session id / SSRC / media key are unchanged, one
`ChannelJoinAck` per still-joined channel follows and a fresh `resume_token` is issued (tokens are
single-use). On failure — grace expired, session closed by the server, token mismatch — the same
connection simply yields a new session and the client must re-join its channels.

## Messages by purpose

| Client → server | Server → client | Notes |
| --- | --- | --- |
| `ChannelJoin { channel_id, token }` | `ChannelJoinAck { channel_id, participants, transcription }` | `token` = player JWT listing the channel, or a `join` action token |
| `ChannelLeave { channel_id }` | `ParticipantJoined { channel_id, user_id, display_name, ssrc }`, `ParticipantLeft` | roster; `ssrc` identifies the sender's AURX packets |
| — | `MediaBound { session_id }` | the UDP `SessionBind` was accepted |
| — | `SessionClose { session_id, reason }`, `Kick { channel_id, user_id, reason }` | session is gone / removed from a channel |
| — | `MuteStateChanged { channel_id, user_id, muted, server_muted }` | sender-side and moderator mutes (never receiver-local ones) |
| `SetParticipantMute { user_id, channel_id?, muted }`, `SetParticipantVolume { user_id, volume }`, `SetUserBlock { user_id, blocked }` | `ReceiverPreferences {…}` on session start, `UserBlockChanged` | receiver-local preferences, enforced server-side |
| `SetTransmission { mode }`, `SetChannelFocus { channel_id? }` | `TransmissionChanged`, `ChannelFocusChanged` | `mode` = `none` / `single { channel_id }` / `all`; server resets both when the target channel is left |
| — | `SpeakingStateChanged { channel_id, user_id, speaking }`, `ChannelEnergy { channel_id, levels }` | voice activity, see [Channels](../features/channels.md#speaking-energy-and-roster) |
| `PositionUpdate { channel_id, positions }`, `OcclusionUpdate`, `ReverbZoneUpdate` | — | positional channels; players may only move themselves unless they hold a moderator role |
| `QualityReport { rtt_ms, jitter_ms, packet_loss }` | `NetworkQuality { quality }`, `BitrateCommand { target_bitrate_kbps, reason }` | see [Network quality](../features/quality.md) |
| `RecordingConsentResponse { recording_id, consent }` | `RecordingNotification { channel_id, recording_id, active, initiated_by, live }` | consent gating for recordings and live streams |
| `ModerateParticipant { channel_id, user_id, action, token, reason? }` | `ModerateParticipantAck` | in-game kick/mute/unmute with a one-time action token |
| `ChatSend { channel_id, text, metadata?, client_ref? }`, `ChatSendDirect { user_id, … }`, `ChatTyping { channel_id, typing }` | `ChatMessageReceived { message }`, `ParticipantTyping` | see [Text chat](../features/chat.md) |
| `SetTranscripts { enabled }` | `Transcript { transcript }` | opt-out of transcript delivery |
| `TtsSpeak { channel_id?, text, voice?, destination, client_ref? }`, `TtsCancel` | `TtsStatus { request_id, client_ref, state, duration_ms?, message? }` | see [Transcripts and TTS](../features/speech.md) |
| `WebRtcOffer { sdp }` | `WebRtcAnswer { sdp }` | browsers; alternative to `POST /v1/webrtc/offer` |
| `Ping { nonce }` | `Pong { nonce }`, `Error { code, message, client_ref? }` | errors reuse the REST codes; `client_ref` echoes the request that failed when it had one |

`SessionInit` exists in the enum for symmetry with the native protocol; over WebSocket the
credential in the upgrade request replaces it.

## Rate limits and back-pressure

Chat and typing have per-session anti-flood limits, and the per-connection outbound queue is
bounded: a client that stops reading loses events rather than stalling the node, so treat the
roster from `ChannelJoinAck` (or a re-join) as the way to resynchronise after a long stall.
Messages the server refuses come back as `Error` with the offending `client_ref` where
applicable.

## Multi-node behaviour

A session lives on the node that accepted the WebSocket. Roster, speaking, energy, chat,
moderation and receiver-preference events for a channel spread across nodes over the Redis bus,
and media between nodes flows over the encrypted [cascade](../operations/scaling.md). Clients
never need to know which node their peers are on.
