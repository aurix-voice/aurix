-- Post-hoc processing of recordings: channel mixdowns derived from per-participant tracks
-- and speech-to-text transcripts of finished recordings.

ALTER TABLE recordings ADD COLUMN status TEXT NOT NULL DEFAULT 'ready';
UPDATE recordings SET status = 'recording' WHERE ended_at IS NULL;
-- When the first packet was written (consent may delay it past started_at); aligns tracks.
ALTER TABLE recordings ADD COLUMN audio_started_at TIMESTAMPTZ;
-- Tracks a mixdown was rendered from (NULL for captured tracks and evidence clips).
ALTER TABLE recordings ADD COLUMN sources UUID[];
-- Node holding the file / running the job.
ALTER TABLE recordings ADD COLUMN node_id UUID;
ALTER TABLE recordings ADD COLUMN error TEXT;

CREATE INDEX idx_recordings_sources ON recordings USING GIN (sources);
CREATE INDEX idx_recordings_status_node ON recordings(node_id, status);

CREATE TABLE recording_transcripts (
    recording_id UUID PRIMARY KEY REFERENCES recordings(id) ON DELETE CASCADE,
    app_id UUID NOT NULL REFERENCES apps(id) ON DELETE CASCADE,
    -- queued | running | ready | failed
    status TEXT NOT NULL,
    provider TEXT,
    language TEXT,
    text TEXT NOT NULL DEFAULT '',
    -- [{speaker, start_ms, end_ms, text, language, confidence, words: [{word, start_ms, end_ms}]}]
    segments JSONB NOT NULL DEFAULT '[]'::jsonb,
    duration_ms BIGINT NOT NULL DEFAULT 0,
    node_id UUID,
    error TEXT,
    requested_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    finished_at TIMESTAMPTZ
);

CREATE INDEX idx_recording_transcripts_node_status ON recording_transcripts(node_id, status);
