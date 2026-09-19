# Text chat (lite)

Aurix ships a deliberately small text layer for the situations a voice session already covers:
party/squad chat, `/commands`, pings in a positional channel, and "my mic is broken". It is
**live-only** — there is no offline delivery, no conversations, no read markers, no attachments;
history is an opt-in per deployment. Anything that looks like a social network belongs in your
game backend.

## Sending and receiving

All of it runs over the control WebSocket:

| Client → server | Server → client |
| --- | --- |
| `ChatSend {channel_id, text, metadata?, client_ref?}` | `ChatMessageReceived {message}` to every member of the channel on every node |
| `ChatSendDirect {user_id, text, metadata?, client_ref?}` | `ChatMessageReceived {message}` to the target (must be online in the same application) and echoed to the sender |
| `ChatTyping {channel_id, typing}` | `ParticipantTyping {channel_id, user_id, typing}` |

```json
{"type":"ChatMessageReceived","data":{"message":{
  "id":"0192…","channel_id":"…","from_user_id":"…","display_name":"alice",
  "text":"push B","metadata":{"kind":"ping","x":12.5,"y":0,"z":-3.2},
  "sent_at":"2025-01-27T18:26:40Z","client_ref":"m-17"}}}
```

* `metadata` is free-form JSON for your own message kinds (pings, emotes, commands); the size
  limit `chat.max_message_bytes` (1024, max 16384) counts text **and** metadata.
* `client_ref` is echoed on the delivered message and on any `Error` produced by the send, so
  the UI can mark its optimistic message as sent or failed.
* Rejections arrive as `Error {code, message, client_ref}`: `AUTH_DENIED` (not a member,
  blocked either way), `USER_MUTED` (server-side mute), `USER_OFFLINE` (directed message to
  someone without a live session in this application), `VALIDATION_ERROR` (empty, too large,
  messaging yourself), `RATE_LIMIT_EXCEEDED` (`chat.messages_per_second` /
  `chat.message_burst`, 2 / 10 per session), `MESSAGE_BLOCKED` (content filter),
  `CHAT_DISABLED`.
* Typing indicators are throttled to one per `chat.typing_interval_ms` (1500) per channel and
  are never persisted or exported unless a webhook/SSE consumer asks for `participant.typing`.

## The same rules as voice

Membership decides who can post into a channel; a directed message needs the target to be online
in the same application (no offline delivery). [Blocks](channels.md#receiver-side-controls)
suppress text in both directions, and a moderator's server-side mute silences text too unless
`chat.server_mute_blocks_text = false`. Erasing a user removes the messages they sent and
received when persistence is on.

## Content filter

With `[safety]` enabled, messages first pass the lexicon and the classifier described in
[Content safety](safety.md) (masking, blocking, incidents, automatic mute/kick); the webhook
below runs last and sees the masked text. When `chat.filter_webhook` is set, every player message (not system messages) is `POST`ed there
as JSON before delivery:

```json
{"app_id":"…","channel_id":"…"|null,"from_user_id":"…","to_user_id":null|"…",
 "display_name":"alice","text":"…","metadata":{…}|null}
```

Reply `200` with `{"action":"allow"}`, `{"action":"replace","text":"…"}` (deliver the substitute)
or `{"action":"block","reason":"…"}` (the sender gets `MESSAGE_BLOCKED` with that reason). A
non-2xx status, invalid body or timeout (`chat.filter_timeout_ms`, 1500) blocks the message —
fail-closed — unless `chat.filter_fail_open = true`. The same filter runs on text destined for a
channel via [text-to-speech](speech.md).

## Persistence and system messages

Storage is off by default. With `chat.persist = true` messages land in `chat_messages`
(post-filter text, per application) and are swept hourly after `chat.retention_days` (30):

* `GET /v1/channels/{id}/messages?before=<ts>&limit=` (`chat:read`) pages the history
  (`404` when persistence is off);
* `POST /v1/channels/{id}/messages` (`chat:write`) sends a **system** message from the nil user
  to the channel — match countdowns, server notices — delivered live like any other message.

`chat.message` events (webhooks/SSE) carry every delivered message to your game server if you
prefer to keep history there.

## SDK surface

| | Web | Unity | Native / Unreal |
| --- | --- | --- | --- |
| send | `sendMessage(channelId, text, {metadata, clientRef})`, `sendDirectMessage(userId, …)` | `SendMessageAsync`, `SendDirectMessageAsync` | `aurix_client_send_chat`, `aurix_client_send_direct_chat` |
| typing | `setTyping(channelId, bool)` | `SetTypingAsync` | `aurix_client_set_typing` |
| events | `chatMessage`, `participantTyping` | `OnChatMessage`, `OnParticipantTyping` | `ChatMessage`, `ParticipantTyping` events |

Exact names are in the SDK chapters; the quick-start sample scenes for [Web](../sdk/web.md) and
[Unity](../sdk/unity.md) include a chat panel.
