-- Chat history pagination, offline delivery of directed messages and read markers.

-- Keyset pagination orders by (sent_at, id) so equal timestamps cannot skip or repeat rows.
CREATE INDEX idx_chat_messages_channel_cursor ON chat_messages(app_id, channel_id, sent_at DESC, id DESC)
    WHERE channel_id IS NOT NULL;
CREATE INDEX idx_chat_messages_to_cursor ON chat_messages(app_id, to_user_id, sent_at DESC, id DESC)
    WHERE to_user_id IS NOT NULL;
CREATE INDEX idx_chat_messages_from_cursor ON chat_messages(app_id, from_user_id, sent_at DESC, id DESC);
DROP INDEX IF EXISTS idx_chat_messages_channel;
DROP INDEX IF EXISTS idx_chat_messages_to;
DROP INDEX IF EXISTS idx_chat_messages_from;

-- Directed messages accepted while the recipient had no active session (chat.offline_delivery).
-- They are replayed on the recipient's next connect until read (see chat_read_markers).
ALTER TABLE chat_messages ADD COLUMN offline BOOLEAN NOT NULL DEFAULT false;
CREATE INDEX idx_chat_messages_offline ON chat_messages(app_id, to_user_id, sent_at, id)
    WHERE offline;

-- Last message a user has read per conversation: a channel (`kind = 'channel'`,
-- `conversation_id` = channel id) or a direct conversation with one peer (`kind = 'direct'`,
-- `conversation_id` = peer user id). Markers only move forward.
CREATE TABLE chat_read_markers (
    app_id UUID NOT NULL REFERENCES apps(id) ON DELETE CASCADE,
    user_id UUID NOT NULL,
    kind TEXT NOT NULL CHECK (kind IN ('channel', 'direct')),
    conversation_id UUID NOT NULL,
    message_id UUID NOT NULL,
    message_sent_at TIMESTAMPTZ NOT NULL,
    read_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    PRIMARY KEY (app_id, user_id, kind, conversation_id)
);
CREATE INDEX idx_chat_read_markers_conversation ON chat_read_markers(app_id, kind, conversation_id);
