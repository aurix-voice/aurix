-- Optional server-side transcript storage (stt.persist = true). One row per delivered live
-- transcript (the `Transcript` event), written by the node that transcribed the segment; the
-- machine translations the fleet made of it (one per target language, whichever node made it
-- first) hang off it. Swept by stt.retention_days and removed with the user.
CREATE TABLE transcripts (
    id UUID PRIMARY KEY,
    app_id UUID NOT NULL REFERENCES apps(id) ON DELETE CASCADE,
    channel_id UUID NOT NULL,
    user_id UUID NOT NULL,
    text TEXT NOT NULL,
    language TEXT,
    started_at TIMESTAMPTZ NOT NULL,
    duration_ms INTEGER NOT NULL,
    -- Word timings as delivered (only with stt.include_words), else NULL.
    words JSONB,
    node_id UUID,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);
CREATE INDEX idx_transcripts_channel ON transcripts(app_id, channel_id, started_at DESC, id DESC);
CREATE INDEX idx_transcripts_user ON transcripts(app_id, user_id, started_at DESC, id DESC);
CREATE INDEX idx_transcripts_started_at ON transcripts(started_at);

CREATE TABLE transcript_translations (
    transcript_id UUID NOT NULL REFERENCES transcripts(id) ON DELETE CASCADE,
    language TEXT NOT NULL,
    text TEXT NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    PRIMARY KEY (transcript_id, language)
);
