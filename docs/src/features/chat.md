# Text chat (lite)

Aurix ships a deliberately small text layer for the situations a voice session already covers:
party/squad chat, `/commands`, pings in a positional channel, and "my mic is broken". Out of the
box it is **live-only**; with `chat.persist = true` a deployment also gets paged history, directed
messages that wait for an offline recipient, read markers with unread counts, edits and
deletions, reactions and full-text search (see
[Stored chat](#stored-chat-history-offline-delivery-read-markers)). There are still no
attachments, threads or friend lists — anything that looks like a social network belongs in
your game backend.

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

### Edits and deletions

A stored message keeps its `id` and its `sent_at` position for life; an edit changes its text
and metadata, a deletion turns it into a **tombstone** — same id, same position, empty `text`,
no `metadata`, no reactions, `deleted_at` and `deleted_by` set. History pages keep tombstones
in place so a client can replace the bubble with "message deleted"; search, offline replay and
unread counts ignore them.

```toml
[chat]
edits = true                   # false: players cannot edit (NOT_IMPLEMENTED); deletions and REST still work
edit_window_secs = 900         # authors may edit / delete this long after sending; 0 = forever
```

| Client → server | Server → client |
| --- | --- |
| `ChatEdit {message_id, text, metadata?, client_ref?}` | `ChatMessageUpdated {message}` (`edited_at` set) to everyone who can see the message — the channel members on every node, or both sides of a direct thread — the actor's copy carries the `client_ref` |
| `ChatDelete {message_id, client_ref?}` | `ChatMessageUpdated {message}` with the tombstone, same fan-out; deleting a tombstone again is idempotent: the actor gets it back with the `client_ref`, nobody else hears anything |

* **Who may**: the author, within `edit_window_secs` of `sent_at`; additionally a channel
  moderator (`moderate` grant or role) may delete — not edit — any message of their channel.
  Direct messages are only ever the author's. Everything else is `AUTH_DENIED`; the same code
  answers a non-member of the channel.
* The caller must currently be in the channel (member session) or be a participant of the
  direct thread, otherwise the message does not exist for them: `NOT_FOUND`. Tombstones cannot
  be edited or reacted to (`NOT_FOUND`).
* An edit runs through the same size limit, [content filter](#content-filter) and flood limit
  as a new message (`VALIDATION_ERROR`, `MESSAGE_BLOCKED`, `RATE_LIMIT_EXCEEDED`); replacing the
  metadata with `null` clears it.
* REST (`chat:write`) acts as the **operator**: `PATCH /v1/messages/{id} {text, metadata?}`
  edits any stored message of the application without the author / window rule and without
  the filter, `DELETE /v1/messages/{id}` tombstones it with `deleted_by` = the nil system user;
  both return the updated message and fan out `ChatMessageUpdated` exactly like a player's
  action. `GET /v1/messages/{id}` (`chat:read`) returns one stored message with its tallies,
  tombstones included.
* Webhooks / SSE see `chat.message_updated` (edit or tombstone) with the message.

### Reactions

A reaction is a short token — an emoji, `+1`, `gg`, your own sticker id — at most 32 bytes,
no whitespace or control characters (`VALIDATION_ERROR`), set at most once per user per
message. Every message carries its tallies:

```json
"reactions":[{"reaction":"🔥","count":3,"user_ids":["…","…","…"]}]
```

`count` is exact; `user_ids` lists the first 20 reactors (`REACTION_USERS_SHOWN`) with the
reader's own id first when present, so a client can render "you and 2 others" without another
round-trip. A message accepts `chat.reactions_per_message` (20) **distinct** reactions; the
21st distinct one is `VALIDATION_ERROR`, joining an existing tally is always allowed.

| Client → server | Server → client |
| --- | --- |
| `ChatReact {message_id, reaction, add}` | `ChatReactionChanged {message_id, channel_id?, message_from_user_id, message_to_user_id?, user_id, reaction, added, count, timestamp}` to everyone who sees the message, the actor included |

Adding a reaction you already set, or removing one you never set, changes nothing and sends
nothing — clients can fire optimistically and stay idempotent across retries. The caller must
see the message (channel member session / direct participant, `NOT_FOUND` otherwise) and be
allowed to write (blocks, mutes and the flood limit apply). Deleting a message drops its
reactions; erasing a user drops theirs.

REST (`chat:write`): `PUT /v1/messages/{id}/reactions/{reaction} {user_id}` sets and
`DELETE /v1/messages/{id}/reactions/{reaction}?user_id=` clears the reaction **on behalf of**
that user of the application (the API key is the operator's credential — it may act for any of
its users, exactly as it may send messages for them; `404` for a user id the application never
issued a token for). Both return `{changed, count}` and fan out the same `ChatReactionChanged`.
Webhooks / SSE see `chat.reaction`.

### Search

Full-text search over the stored history of one conversation, powered by PostgreSQL's
`tsvector` (`simple` dictionary — exact words, no stemming, any language) with web-search
syntax: `words`, `"a phrase"`, `-excluded`, `or`. Deleted messages never match; edited ones
match their current text. Results come **newest first** as a page with the same keyset cursor
as history (`before` / `next_before`), so paging through matches is stable while people keep
chatting.

```toml
[chat]
search = true                  # false: ChatSearch answers NOT_IMPLEMENTED
searches_per_minute = 30       # per session
```

| Client → server | Server → client |
| --- | --- |
| `ChatSearch {channel_id? \| user_id?, query, from_user_id?, before?, limit?, client_ref?}` | `ChatSearchResult {channel_id?, user_id?, query, messages, next_before?, client_ref?}` |

* `channel_id` searches a channel the caller is a member of (`AUTH_DENIED` otherwise);
  `user_id` searches the caller's direct thread with that user; **neither** searches every
  direct conversation of the caller. Nobody can search someone else's direct threads.
* `query` is 1..=256 bytes and must contain at least one word (`VALIDATION_ERROR`);
  `from_user_id` keeps only that author's messages; `limit` is clamped to
  `1..=chat.history_page_max`.
* `RATE_LIMIT_EXCEEDED` after `searches_per_minute` searches from one session.
* REST (`chat:read`): `GET /v1/channels/{id}/messages/search?q=&from_user_id=&before=&limit=`
  and `GET /v1/users/{id}/messages/search?q=&peer=` (moderation view — without `peer` every
  direct message the user sent or received) return the same `{messages, next_before?}`.

## SDK surface

| | Web | Unity | Native / Unreal / Godot |
| --- | --- | --- | --- |
| send | `sendMessage(channelId, text, {metadata, clientRef})`, `sendDirectMessage(userId, …)` | `SendMessageAsync`, `SendDirectMessageAsync` | `aurix_client_send_chat`, `aurix_client_send_direct_chat` |
| typing | `setTyping(channelId, bool)` | `SetTypingAsync` | `aurix_client_set_typing` |
| history | `history({channelId} \| {userId}, {before, after, limit})` → `{messages, nextBefore?, nextAfter?}` | `HistoryAsync(channelId, …)`, `DirectHistoryAsync(userId, …)` → `ChatHistoryPage` | `aurix_client_chat_history` → `AURIX_EVENT_CHAT_HISTORY` (`aurix_event_chat_history`, `aurix_event_chat_history_message`; `AurixChatMessage.cursor`); Unreal `ChannelHistory` / `DirectHistory` → `OnChatHistory` |
| read markers | `markRead(scope, messageId)`, `readMarkers(scope)` → `{markers, unreadCount}` | `MarkReadAsync` / `MarkDirectReadAsync`, `ReadMarkersAsync` / `DirectReadMarkersAsync` | `aurix_client_mark_chat_read`, `aurix_client_chat_read_markers` → `AURIX_EVENT_CHAT_READ_MARKERS` (`aurix_event_read_marker_at`); Unreal `MarkChannelRead` / `MarkDirectRead`, `ChannelReadMarkers` / `DirectReadMarkers` → `OnChatReadMarkers` |
| edit / delete | `editMessage(messageId, text, {metadata})`, `deleteMessage(messageId)` → the updated `ChatMessage` | `EditMessageAsync`, `DeleteMessageAsync` → `ChatMessage` | `aurix_client_edit_chat`, `aurix_client_delete_chat` → `AURIX_EVENT_CHAT_MESSAGE_UPDATED` with the request id; Unreal `EditChat` / `DeleteChat` → `OnChatMessageUpdated`; Godot `edit_chat` / `delete_chat` → `chat_message_updated` |
| reactions | `react(messageId, reaction, add)` | `ReactAsync(messageId, reaction, add)` | `aurix_client_react_chat`; Unreal `ReactChat`; Godot `react_chat` |
| search | `search({channelId} \| {userId} \| {}, query, {fromUserId, before, limit})` → `{messages, nextBefore?}` | `SearchAsync(channelId, query, …)`, `SearchDirectAsync(userId?, query, …)` → `ChatHistoryPage` | `aurix_client_search_chat` → `AURIX_EVENT_CHAT_SEARCH_RESULT` (same accessors as history); Unreal `SearchChannelChat` / `SearchDirectChat` → `OnChatSearchResult`; Godot `search_channel_chat` / `search_direct_chat` → `chat_search_result` |
| events | `chatMessage` (`message.offline`), `chatMessageUpdated` (`editedAt` / `deletedAt`, `reactions`), `chatReactionChanged`, `chatReadMarker`, `chatInboxSynced`, `participantTyping` | `OnChatMessage` (`Offline`), `OnChatMessageUpdated`, `OnChatReactionChanged`, `OnChatReadMarker`, `OnChatInboxSynced`, `OnParticipantTyping` | `AURIX_EVENT_CHAT_MESSAGE` (`offline`), `AURIX_EVENT_CHAT_MESSAGE_UPDATED`, `AURIX_EVENT_CHAT_REACTION_CHANGED` (`aurix_event_reaction`), `AURIX_EVENT_CHAT_READ_MARKER`, `AURIX_EVENT_CHAT_INBOX_SYNCED`, `AURIX_EVENT_PARTICIPANT_TYPING`; Unreal `OnChatMessage`, `OnChatMessageUpdated`, `OnChatReactionChanged`, `OnChatReadMarker`, `OnChatInboxSynced`; Godot signals of the same names in `snake_case` |

Exact names are in the SDK chapters; the quick-start sample scenes for [Web](../sdk/web.md) and
[Unity](../sdk/unity.md) include a chat panel.
