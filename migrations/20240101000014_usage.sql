-- Tenant usage accounting: time-bucketed CCU / minutes / metered counters per application and
-- per channel, plus the per-application quotas they enforce.
--
-- * `usage_app_buckets`     one row per application per 5-minute bucket. The aggregator derives
--                           `peak_sessions`, `session_minutes`, `sessions_started`,
--                           `unique_users`, `peak_participants`, `participant_minutes`,
--                           `active_channels` and `recording_seconds` from `sessions`,
--                           `channel_memberships` and `recordings` once the bucket has closed;
--                           nodes add the metered counters (`media_bytes_*`, `chat_messages`,
--                           `tts_*`, `stt_audio_ms`) as they go. A bucket is final once the
--                           `usage_watermarks` row for its scope has moved past it.
-- * `usage_channel_buckets` the same per channel at hourly resolution (channels are many).
-- * `usage_watermarks`      how far the aggregator has finalized (`app`, `channel`).
--
-- `analytics_snapshots` (5-minute point samples with mostly unpopulated columns) is superseded;
-- it is kept for the retention sweep of existing rows and is no longer written to.
CREATE TABLE usage_app_buckets (
    app_id UUID NOT NULL REFERENCES apps(id) ON DELETE CASCADE,
    bucket TIMESTAMPTZ NOT NULL,
    peak_sessions BIGINT NOT NULL DEFAULT 0,
    session_minutes DOUBLE PRECISION NOT NULL DEFAULT 0,
    sessions_started BIGINT NOT NULL DEFAULT 0,
    unique_users BIGINT NOT NULL DEFAULT 0,
    peak_participants BIGINT NOT NULL DEFAULT 0,
    participant_minutes DOUBLE PRECISION NOT NULL DEFAULT 0,
    active_channels BIGINT NOT NULL DEFAULT 0,
    recording_seconds DOUBLE PRECISION NOT NULL DEFAULT 0,
    media_bytes_in BIGINT NOT NULL DEFAULT 0,
    media_bytes_out BIGINT NOT NULL DEFAULT 0,
    chat_messages BIGINT NOT NULL DEFAULT 0,
    tts_requests BIGINT NOT NULL DEFAULT 0,
    tts_characters BIGINT NOT NULL DEFAULT 0,
    stt_audio_ms BIGINT NOT NULL DEFAULT 0,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    PRIMARY KEY (app_id, bucket)
);
CREATE INDEX idx_usage_app_bucket ON usage_app_buckets(bucket);

CREATE TABLE usage_channel_buckets (
    app_id UUID NOT NULL REFERENCES apps(id) ON DELETE CASCADE,
    channel_id UUID NOT NULL,
    bucket TIMESTAMPTZ NOT NULL,
    peak_participants BIGINT NOT NULL DEFAULT 0,
    participant_minutes DOUBLE PRECISION NOT NULL DEFAULT 0,
    joins BIGINT NOT NULL DEFAULT 0,
    unique_users BIGINT NOT NULL DEFAULT 0,
    chat_messages BIGINT NOT NULL DEFAULT 0,
    tts_requests BIGINT NOT NULL DEFAULT 0,
    tts_characters BIGINT NOT NULL DEFAULT 0,
    stt_audio_ms BIGINT NOT NULL DEFAULT 0,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    PRIMARY KEY (app_id, channel_id, bucket)
);
CREATE INDEX idx_usage_channel_app_bucket ON usage_channel_buckets(app_id, bucket);
CREATE INDEX idx_usage_channel_bucket ON usage_channel_buckets(bucket);

CREATE TABLE usage_watermarks (
    scope TEXT PRIMARY KEY,
    bucket TIMESTAMPTZ NOT NULL,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

-- Per-application quotas (0 = unlimited).
-- * `max_concurrent_sessions`     fleet-wide cap on simultaneously connected sessions; a
--                                 session over the cap is refused at connect (`QUOTA_EXCEEDED`).
-- * `monthly_participant_minutes` cap on channel participant-minutes per calendar month (UTC);
--                                 once reached, channel joins are refused until the month rolls
--                                 over. Sessions already in channels are not cut off.
ALTER TABLE apps
    ADD COLUMN max_concurrent_sessions INTEGER NOT NULL DEFAULT 0,
    ADD COLUMN monthly_participant_minutes BIGINT NOT NULL DEFAULT 0;

-- The aggregator scans sessions and memberships by time; the original indexes cover the
-- open rows and (idx_sessions_disconnected, migration 7) closed sessions only.
CREATE INDEX idx_sessions_connected ON sessions(connected_at);
CREATE INDEX idx_cm_joined ON channel_memberships(joined_at);
CREATE INDEX idx_cm_left ON channel_memberships(left_at) WHERE left_at IS NOT NULL;
