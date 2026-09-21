-- Message edits and tombstones, per-user reactions and full-text search over stored chat.

-- Edited / deleted state lives on the row itself so message ids, (sent_at, id) cursors and
-- read markers stay valid. A deleted message keeps its row with empty text and no metadata.
ALTER TABLE chat_messages ADD COLUMN edited_at TIMESTAMPTZ;
ALTER TABLE chat_messages ADD COLUMN deleted_at TIMESTAMPTZ;
ALTER TABLE chat_messages ADD COLUMN deleted_by UUID;

-- Language-neutral full-text index ('simple': lower-cased tokens, no stemming) so search
-- never scans a conversation; tombstones index to an empty vector.
ALTER TABLE chat_messages ADD COLUMN text_search TSVECTOR
    GENERATED ALWAYS AS (to_tsvector('simple', text)) STORED;
CREATE INDEX idx_chat_messages_text_search ON chat_messages USING GIN (text_search);

-- One row per (message, reaction, user); reactions of a purged message go with it.
CREATE TABLE chat_reactions (
    app_id UUID NOT NULL REFERENCES apps(id) ON DELETE CASCADE,
    message_id UUID NOT NULL REFERENCES chat_messages(id) ON DELETE CASCADE,
    user_id UUID NOT NULL,
    reaction TEXT NOT NULL CHECK (reaction <> '' AND octet_length(reaction) <= 32),
    reacted_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    PRIMARY KEY (app_id, message_id, reaction, user_id)
);
CREATE INDEX idx_chat_reactions_user ON chat_reactions(app_id, user_id);
