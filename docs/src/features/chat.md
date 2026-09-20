# Text chat (lite)

Aurix ships a deliberately small text layer for the situations a voice session already covers:
party/squad chat, `/commands`, pings in a positional channel, and "my mic is broken". Out of the
box it is **live-only**; with `chat.persist = true` a deployment also gets paged history, directed
messages that wait for an offline recipient, and read markers with unread counts (see
[Stored chat](#stored-chat-history-offline-delivery-read-markers)). There are still no
attachments, threads, reactions or friend lists — anything that looks like a social network
belongs in your game backend.

## Sending and receiving

All of it runs over the control WebSocket:

| Client → server | Server → client |
| --- | --- |
| `ChatSend {channel_id, text, metadata?, client_ref?}` | `ChatMessageReceived {message}` to every member of the channel on every node |
| `ChatSendDirect {user_id, text, metadata?, client_ref?}` | `ChatMessageReceived {message}` to every live session of the target in the same application and echoed to the sender; with stored chat an offline target gets it on their next connect (`offline: true` on the echo) |
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
  someone without a live session in this application and no offline delivery), `NOT_FOUND`
  (directed message to a user id this application never issued a token for),
  `VALIDATION_ERROR` (empty, too large, messaging yourself), `RATE_LIMIT_EXCEEDED` (`chat.messages_per_second` /
  `chat.message_burst`, 2 / 10 per session), `MESSAGE_BLOCKED` (content filter),
  `CHAT_DISABLED`.
* Typing indicators are throttled to one per `chat.typing_interval_ms` (1500) per channel and
  are never persisted or exported unless a webhook/SSE consumer asks for `participant.typing`.

## The same rules as voice

Membership decides who can post into a channel; a directed message needs a target user of the
same application — live, or (stored chat) known to it. [Blocks](channels.md#receiver-side-controls)
suppress text in both directions, and a moderator's server-side mute silences text too unless
`chat.server_mute_blocks_text = false`. Erasing a user removes the messages they sent and
received and their read markers when persistence is on.

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

## System messages

`POST /v1/channels/{id}/messages` and `POST /v1/users/{id}/messages` (`chat:write`) send a
**system** message from the nil user — match countdowns, server notices — delivered live like
any other message (and stored with the rest when persistence is on). `chat.message` events
(webhooks/SSE) carry every delivered message to your game server if you prefer to keep history
there.

## Stored chat: history, offline delivery, read markers

Storage is off by default. With `chat.persist = true` messages land in `chat_messages`
(post-filter text, per application) and are swept hourly after `chat.retention_days` (30, `0` =
forever). Everything below needs it and answers `404` / `CHAT_DISABLED` otherwise.

```toml
[chat]
persist = true
offline_delivery = true        # queue directed messages for users without a live session
offline_max_messages = 200     # newest unread ones replayed on connect (older stay in history)
offline_max_age_hours = 0      # 0 = anything within retention_days
history_page_max = 200         # hard cap on a page
unread_count_cap = 1000        # unread counters saturate here
read_receipts = true           # channel members / the direct peer see each other's markers
```

### History pages

Every history request answers one page, **newest first**, with opaque cursors for both
directions; a cursor encodes the message's `(sent_at, id)` position (URL-safe base64, the same
string in every SDK — `message.cursor` / `Cursor` / `aurix_chat_message_cursor`), so equal
timestamps can never skip or repeat a row and a page stays stable while new messages arrive.

| Client → server | Server → client |
| --- | --- |
| `ChatHistory {channel_id \| user_id, before?, after?, limit?, client_ref?}` | `ChatHistoryResult {channel_id?, user_id?, messages, next_before?, next_after?, client_ref?}` |

* `channel_id` pages a channel the caller is a member of (`AUTH_DENIED` otherwise); `user_id`
  pages the caller's direct conversation with that user, in both directions.
* `before` walks into the past (pass `next_before` of the previous page, absent when the
  oldest message was reached); `after` walks towards the present from a known position
  (`next_after`, absent when caught up). Cursors are exclusive; a malformed one is
  `VALIDATION_ERROR`.
* `limit` is clamped to `1..=chat.history_page_max`.
* REST: `GET /v1/channels/{id}/messages` and `GET /v1/users/{id}/messages?peer=<user>` take the
  same `before` / `after` / `limit` and return the same `{messages, next_before?, next_after?}`
  (`chat:read`). Without `peer` the user endpoint lists every directed message the user sent or
  received.

### Offline delivery of directed messages

With `offline_delivery = true` a `ChatSendDirect` to a user of the application who has no live
session is **accepted and stored** instead of failing with `USER_OFFLINE`. It goes through
validation, blocks, mutes, flood limits and the content filter first — nothing unfiltered is ever
written — and the sender's echo carries `offline: true` ("queued, not delivered live"). Channel
messages are never queued: they are history plus live fan-out.

When the recipient connects (fresh, resumed or migrated), right after `SessionInitAck`, the
node replays the directed messages that are still **unread** — newer than the recipient's read
marker for that sender — oldest first as ordinary `ChatMessageReceived {message}` with
`offline: true`, then sends `ChatInboxSynced {delivered, truncated}`. At most
`offline_max_messages` (the newest) are replayed and nothing older than `offline_max_age_hours`;
`truncated` says older unread messages exist and can be paged with `ChatHistory`. Replay is driven
by read markers, not by a per-device queue: a second device, or a reconnect before the user read
anything, sees the same messages again — dedupe by `message.id` and advance the marker with
`ChatMarkRead` once the user has actually seen them. Aurix keeps one live session per user and
node, so "another device" means another node in practice. Everything is in PostgreSQL, so the
recipient may connect to any node.

### Read markers and unread counts

A read marker is the user's position in one conversation — a channel, or the direct thread with
one peer — and only ever moves forward.

| Client → server | Server → client |
| --- | --- |
| `ChatMarkRead {channel_id \| user_id, message_id}` | `ChatReadMarker {marker}` to every session of the reader; with `read_receipts` also to the channel members / the peer. Nothing is sent when the marker did not move. |
| `ChatReadMarkers {channel_id \| user_id}` | `ChatReadMarkersResult {channel_id?, user_id?, markers, unread_count}` — the visible markers of that conversation (only your own without receipts) and how many messages from others lie after yours |

```json
{"type":"ChatReadMarker","data":{"marker":{
  "user_id":"…","peer_user_id":"…","message_id":"0192…",
  "message_sent_at":"2025-01-27T18:26:40.123456Z","read_at":"2025-01-27T18:27:02Z"}}}
```

`message_id` must belong to the selected conversation of the caller (`NOT_FOUND` otherwise —
you cannot mark somebody else's thread or a message from another channel), and the caller must
be a member of the channel. Unread counts are capped at `chat.unread_count_cap`. Markers are
shared by all devices of the user and travel across nodes like every other event.

REST (`chat:read` / `chat:write`): `GET /v1/users/{id}/read-markers?channel_id=|peer_user_id=`
returns `{marker, unread_count}` (without a selector: every marker of the user),
`PUT /v1/users/{id}/read-markers {channel_id | peer_user_id, message_id}` advances a marker on the
user's behalf and returns `{marker, unread_count, moved}`, `GET /v1/channels/{id}/read-markers`
lists everyone's position in a channel. Erasing a user (`DELETE /v1/users/{id}`) removes their
markers and the markers others hold for direct threads with them; deleting an application removes
everything.

## SDK surface

| | Web | Unity | Native / Unreal |
| --- | --- | --- | --- |
| send | `sendMessage(channelId, text, {metadata, clientRef})`, `sendDirectMessage(userId, …)` | `SendMessageAsync`, `SendDirectMessageAsync` | `aurix_client_send_chat`, `aurix_client_send_direct_chat` |
| typing | `setTyping(channelId, bool)` | `SetTypingAsync` | `aurix_client_set_typing` |
| history | `history({channelId} \| {userId}, {before, after, limit})` → `{messages, nextBefore?, nextAfter?}` | `HistoryAsync(channelId, …)`, `DirectHistoryAsync(userId, …)` → `ChatHistoryPage` | `aurix_client_chat_history` → `AURIX_EVENT_CHAT_HISTORY` (`aurix_event_chat_history`, `aurix_event_chat_history_message`; `AurixChatMessage.cursor`); Unreal `ChannelHistory` / `DirectHistory` → `OnChatHistory` |
| read markers | `markRead(scope, messageId)`, `readMarkers(scope)` → `{markers, unreadCount}` | `MarkReadAsync` / `MarkDirectReadAsync`, `ReadMarkersAsync` / `DirectReadMarkersAsync` | `aurix_client_mark_chat_read`, `aurix_client_chat_read_markers` → `AURIX_EVENT_CHAT_READ_MARKERS` (`aurix_event_read_marker_at`); Unreal `MarkChannelRead` / `MarkDirectRead`, `ChannelReadMarkers` / `DirectReadMarkers` → `OnChatReadMarkers` |
| events | `chatMessage` (`message.offline`), `chatReadMarker`, `chatInboxSynced`, `participantTyping` | `OnChatMessage` (`Offline`), `OnChatReadMarker`, `OnChatInboxSynced`, `OnParticipantTyping` | `AURIX_EVENT_CHAT_MESSAGE` (`offline`), `AURIX_EVENT_CHAT_READ_MARKER`, `AURIX_EVENT_CHAT_INBOX_SYNCED`, `AURIX_EVENT_PARTICIPANT_TYPING`; Unreal `OnChatMessage`, `OnChatReadMarker`, `OnChatInboxSynced` |

Exact names are in the SDK chapters; the quick-start sample scenes for [Web](../sdk/web.md) and
[Unity](../sdk/unity.md) include a chat panel.
