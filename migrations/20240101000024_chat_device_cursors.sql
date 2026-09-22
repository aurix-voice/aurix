-- Per-device delivery cursors for directed chat messages: a device that identifies itself on
-- connect (`X-Aurix-Device` / `device.<id>` sub-protocol) gets every directed message newer than
-- its acknowledged cursor replayed exactly once, whichever node it connects to. Rows are created
-- by the first `ChatAck` and swept when unused for `chat.device_cursor_max_age_days`.
CREATE TABLE chat_device_cursors (
    app_id UUID NOT NULL REFERENCES apps(id) ON DELETE CASCADE,
    user_id UUID NOT NULL,
    device_id TEXT NOT NULL,
    message_id UUID NOT NULL,
    message_sent_at TIMESTAMPTZ NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    PRIMARY KEY (app_id, user_id, device_id)
);
CREATE INDEX idx_chat_device_cursors_updated ON chat_device_cursors(updated_at);
