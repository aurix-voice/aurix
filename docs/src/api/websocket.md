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
  "media_addrs":["203.0.113.10:10000","[2001:db8::10]:10000"],
  "media_key":"<base64, 32 bytes>","resume_token":"…","resume_grace_ms":30000,"resumed":false,
  "migrated":false,"failover":["wss://eu2.voice.example.com/ws","wss://eu3.voice.example.com/ws"],
  "media_tunnel":true,"downlink_mix":true,"webrtc_participant_streams":16,"unfocused_channel_gain":0.3,
  "translation":{"speech":true,"languages":["en","de","fr"]}}}
```

`media_key` is the master secret for the [AURX media path](aurx.md); `media_addr` is the UDP
endpoint of *this* node and `media_addrs` every public endpoint of it in preference order (IPv4
first, IPv6 bracketed; the first entry equals `media_addr`; absent from older nodes —
clients try the candidates in order and stick with the family that answers `SessionBind`);
`media_tunnel` says the node also accepts AURX packets as **binary
frames on this very connection** when UDP is blocked
([tunnel](aurx.md#tunnel-aurx-over-the-control-websocket)) — text frames are always control
messages, binary frames are always media; `downlink_mix` says native sessions may ask for a
[server-mixed downlink](../features/channels.md#server-mix-for-native-clients); `translation`
(absent when the node does not translate) lists the target languages listeners may request with
`SetTranslation` and whether translations can also be spoken to them
([live translation](../features/speech.md#live-translation)); `webrtc_participant_streams`
(absent/`0` on older nodes) is how many [per-participant WebRTC tracks](../features/channels.md#per-participant-tracks-for-browsers)
a browser may negotiate next to its mixed track, and `unfocused_channel_gain` the factor the node
applies to channels other than the focused one, so a browser can reproduce it on those tracks. With
`AURIX__AUTH__REQUIRE_ACTION_TOKENS=true` a
plain player JWT is refused for the handshake (`ACTION_TOKEN_REQUIRED`).
`failover` lists other healthy nodes (same region first, least loaded first; up to
`cluster.failover_endpoints`, empty when none advertise a public `wss://` URL) — a client that
loses this node should rotate through them on its reconnect attempts (below).

`Ping { nonce }` / `Pong { nonce }` keep the connection alive; the server also drops sessions
whose media path stops heart-beating.

## Resume

Reconnect within `resume_grace_ms` with the same JWT plus `X-Aurix-Resume: <session_id>.<resume_token>`
(browsers: sub-protocol `resume.<session_id>.<resume_token>`, query `resume=` as a fallback).
On success `SessionInitAck.resumed` is `true`, session id / SSRC / media key are unchanged, one
`ChannelJoinAck` per still-joined channel follows and a fresh `resume_token` is issued (tokens are
single-use). On failure — grace expired, session closed by the server, token mismatch — the same
connection simply yields a new session and the client must re-join its channels.

### Resume on another node (failover)

When the resume reaches a node that does not host the session and the fleet runs with Redis
session mirrors (`cluster.session_mirror`, default on), that node **takes the session over**:
`resumed` and `migrated` are both `true`, session id and SSRC are unchanged, but `media_addr`
and `media_key` are new — the client must re-derive its media keys, re-send `SessionBind` to the
new endpoint (or continue on the tunnel of the new connection) and reset its own receive-side
replay windows and jitter buffers. Channels (`ChannelJoinAck` each), roles, local mutes, gains,
blocks, transmission mode, focus, codec, downlink mode and transcript preference are restored
from the mirror; other participants see no leave/join, only the downlink audio sequence jumps
forward (by 65 536) so their anti-replay windows keep accepting the stream and jitter buffers
resynchronise. The takeover is fenced with an atomic ownership claim in Redis, so two
simultaneous reconnects cannot both adopt the session; the loser (and any resume whose mirror
expired after `cluster.session_mirror_ttl_secs`, default 180 s) gets a fresh session.

Reconnect order recommended for clients (all SDKs do this): attempt 1 → the node you were on,
attempt 2 → `failover[0]`, attempt 3 → `failover[1]`, …, then wrap around, with the usual
backoff between attempts. Once a failover node answers, treat it as the current node and use
the `failover` list of the new ack from then on. `SessionClose {reason: "server_shutdown"}`
is final: do not resume, open a fresh session (on the next endpoint) and re-join.
See [High availability](../operations/high-availability.md#cross-node-session-failover).

## Messages by purpose

| Client → server | Server → client | Notes |
| --- | --- | --- |
| `ChannelJoin { channel_id, token }` | `ChannelJoinAck { channel_id, participants, role, participant_count, hidden_listeners, transcription, safety_voice, audio, positional?, roster_radius?, text_radius? }` | `token` = player JWT listing the channel, or a `join` action token; `role` is yours (`listener` = receive-only), `participant_count` the headcount across nodes including listeners hidden from `participants` when `hidden_listeners` is set ([audiences](../features/channels.md#large-channels-and-audiences)); `positional` (the channel's distance/direction settings, present for positional channels) lets a client reproduce the node's attenuation on [per-participant tracks](../features/channels.md#per-participant-tracks-for-browsers); the radii are present only for [radius-scoped](../features/channels.md#radius-scoped-presence-and-text) positional channels |
| `ChannelLeave { channel_id }` | `ParticipantJoined { channel_id, user_id, display_name, ssrc, role, is_muted }`, `ParticipantLeft` | roster; `ssrc` identifies the sender's AURX packets; in a radius-scoped channel these also report players moving in and out of `roster_radius` |
| — | `MediaBound { session_id, transport }` | the `SessionBind` was accepted; `transport` = `udp` or `tunnel` (the WebSocket itself) |
| — | `SessionClose { session_id, reason }`, `Kick { channel_id, user_id, reason }` | session is gone / removed from a channel |
| — | `MuteStateChanged { channel_id, user_id, muted, server_muted }` | sender-side and moderator mutes (never receiver-local ones) |
| `SetParticipantMute { user_id, channel_id?, muted }`, `SetParticipantVolume { user_id, volume }`, `SetUserBlock { user_id, blocked }` | `ReceiverPreferences {…, codec, downlink}` on session start, `UserBlockChanged` | receiver-local preferences, enforced server-side |
| `SetAudioCodec { codec }` | `AudioCodecChanged { codec }` | native AURX only; `codec` = `opus` (default) / `pcmu` — G.711 fallback transcoded by the node, see [codecs](../features/channels.md#codecs-opus-and-the-pcmu-fallback); `CODEC_NOT_AVAILABLE` when `media.pcmu_fallback` is off or the session is WebRTC |
| `SetDownlinkMode { mode }` | `DownlinkModeChanged { mode }` | native AURX only; `mode` = `streams` (default, one stream per speaker) / `mixed` (one server-mixed stereo stream per channel, `PacketFlags::Mixed`), see [server mix](../features/channels.md#server-mix-for-native-clients); `VALIDATION_ERROR` when `media.downlink_mix` is off or the session is WebRTC; frames already in flight may still be of the previous kind |
| `SetParticipantStreams { pinned }` | `ParticipantStreams { streams: [{ mid, user_id? }] }` | WebRTC only, see [per-participant tracks](../features/channels.md#per-participant-tracks-for-browsers); `streams` is the full current layout of the browser's dedicated tracks (SDP `mid` → who is forwarded on it, `null` = idle), sent once the tracks are negotiated and whenever it changes; `pinned` names users that keep a track while audible (at most `webrtc_participant_streams`, else `VALIDATION_ERROR`); `VALIDATION_ERROR` on a native session |
| `SetTransmission { mode }`, `SetChannelFocus { channel_id? }` | `TransmissionChanged`, `ChannelFocusChanged` | `mode` = `none` / `single { channel_id }` / `all`; server resets both when the target channel is left |
| — | `SpeakingStateChanged { channel_id, user_id, speaking }`, `ChannelEnergy { channel_id, levels }` | voice activity, see [Channels](../features/channels.md#speaking-energy-and-roster) |
| `PositionUpdate { channel_id, positions }`, `OcclusionUpdate`, `ReverbZoneUpdate` | — | positional channels; players may only move themselves unless they hold a moderator role |
| `QualityReport { rtt_ms, jitter_ms, packet_loss }` | `NetworkQuality { quality }`, `BitrateCommand { target_bitrate_kbps, reason, expected_loss_percent }` | see [Network quality](../features/quality.md) |
| — | `ChannelAudioPolicy { channel_id, audio }` | an operator edited the channel's Opus settings; `audio` has the same shape as `ChannelJoinAck.audio` ([channels](../features/channels.md#configuration)) |
| `RecordingConsentResponse { recording_id, consent }` | `RecordingNotification { channel_id, recording_id, active, initiated_by, live }` | consent gating for recordings and live streams |
| `ModerateParticipant { channel_id, user_id, action, token, reason? }` | `ModerateParticipantAck` | in-game kick/mute/unmute with a one-time action token |
| `ChatSend { channel_id, text, metadata?, client_ref? }`, `ChatSendDirect { user_id, … }`, `ChatTyping { channel_id, typing }` | `ChatMessageReceived { message }` (`message.offline` on a queued directed message), `ParticipantTyping` | see [Text chat](../features/chat.md) |
| `ChatHistory { channel_id \| user_id, before?, after?, limit?, client_ref? }` | `ChatHistoryResult { …, messages, next_before?, next_after? }` | stored chat only; opaque `(sent_at, id)` cursors ([history pages](../features/chat.md#history-pages)) |
| `ChatMarkRead { channel_id \| user_id, message_id }`, `ChatReadMarkers { channel_id \| user_id }` | `ChatReadMarker { marker }`, `ChatReadMarkersResult { …, markers, unread_count }`, `ChatInboxSynced { delivered, truncated }` after `SessionInitAck` | stored chat only; see [read markers](../features/chat.md#read-markers-and-unread-counts) and [offline delivery](../features/chat.md#offline-delivery-of-directed-messages) |
| `SetTranscripts { enabled }` | `Transcript { transcript }` | opt-out of transcript delivery |
| `SetTranslation { language?, spoken_language?, speech }` | `TranslationChanged { language?, spoken_language?, speech }` | receive transcripts translated into `language` (normalised BCP-47 tag, `null` = stop); translated `Transcript`s carry `original {text, language?}`; `speech` = also spoken privately to this session; `VALIDATION_ERROR` for unknown / unoffered tags, `TRANSLATION_DISABLED` when the node does not translate — see [live translation](../features/speech.md#live-translation) |
| `TtsSpeak { channel_id?, text, voice?, destination, client_ref? }`, `TtsCancel` | `TtsStatus { request_id, client_ref, state, duration_ms?, message? }` | see [Transcripts and TTS](../features/speech.md) |
| `E2eeHello { channel_id?, public_key }` | `E2eeHello { channel_id, user_id, public_key }` | without `channel_id`: announce E2EE capability for the session (prerequisite for joining `e2ee` channels — `E2EE_REQUIRED`); with it: "I am in this channel", relayed to the other members with the sender's `user_id` — see [End-to-end encryption](../features/e2ee.md) |
| `E2eeSenderKey { channel_id, to, public_key, generation, key }` | `E2eeSenderKey { channel_id, from, public_key, generation, key }` | sender key wrapped for one member; relayed to `to` only after membership / tenant / channel-state checks (`AUTH_DENIED`, `VALIDATION_ERROR`), rate-limited by `rate_limiting.e2ee_messages_per_minute` — the node cannot open it |
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
