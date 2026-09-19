-- Optional text-chat history (chat.persist = true). Rows are written by the node that accepted
-- the message and swept by chat.retention_days. channel_id is NULL for directed messages;
-- from_user_id is the nil UUID for server/system messages posted through the REST API.
CREATE TABLE chat_messages (
    id UUID PRIMARY KEY,
    app_id UUID NOT NULL REFERENCES apps(id) ON DELETE CASCADE,
    channel_id UUID,
    from_user_id UUID NOT NULL,
    display_name TEXT NOT NULL,
    to_user_id UUID,
    text TEXT NOT NULL,
    metadata JSONB,
    sent_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    CHECK ((channel_id IS NULL) <> (to_user_id IS NULL))
);
CREATE INDEX idx_chat_messages_channel ON chat_messages(app_id, channel_id, sent_at DESC)
    WHERE channel_id IS NOT NULL;
CREATE INDEX idx_chat_messages_from ON chat_messages(app_id, from_user_id, sent_at DESC);
CREATE INDEX idx_chat_messages_to ON chat_messages(app_id, to_user_id, sent_at DESC)
    WHERE to_user_id IS NOT NULL;
CREATE INDEX idx_chat_messages_sent_at ON chat_messages(sent_at);
