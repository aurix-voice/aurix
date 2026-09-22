-- Fleet directory of live audio streams. Every node publishes the streams it owns (and
-- refreshes their status); the cascade planner treats a node with a stream on a channel like
-- a node hosting that channel, so the channel's audio reaches the stream wherever it was
-- opened, and the operator API on any node lists/stops streams of the whole fleet.
CREATE TABLE IF NOT EXISTS live_streams (
    id UUID PRIMARY KEY,
    node_id UUID NOT NULL REFERENCES media_nodes(id) ON DELETE CASCADE,
    app_id UUID NOT NULL REFERENCES apps(id) ON DELETE CASCADE,
    channel_id UUID NOT NULL,
    mode TEXT NOT NULL CHECK (mode IN ('pull', 'push')),
    format TEXT NOT NULL,
    mix BOOLEAN NOT NULL DEFAULT FALSE,
    state TEXT NOT NULL,
    users JSONB,
    label TEXT,
    push_url TEXT,
    started_at TIMESTAMPTZ NOT NULL,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    frames_sent BIGINT NOT NULL DEFAULT 0,
    frames_dropped BIGINT NOT NULL DEFAULT 0,
    reconnects INTEGER NOT NULL DEFAULT 0,
    consent JSONB NOT NULL DEFAULT '{}'::jsonb
);
CREATE INDEX IF NOT EXISTS idx_live_streams_channel ON live_streams (channel_id);
CREATE INDEX IF NOT EXISTS idx_live_streams_app ON live_streams (app_id, started_at);
CREATE INDEX IF NOT EXISTS idx_live_streams_node ON live_streams (node_id);
