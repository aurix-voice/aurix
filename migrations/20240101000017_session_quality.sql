-- Per-session E-model quality in analytics.
--
-- * `usage_app_buckets` gains the metered quality aggregates a node adds as it rates sessions
--   (one sample per session per `media.quality_interval_ms`): `quality_samples`, integer-scaled
--   sums (`mos_sum_milli` = MOS × 1000, `rtt_sum_ms`, `jitter_sum_ms`, `loss_sum_permille` =
--   loss % × 10) and `poor_quality_samples` (bars ≤ 2). Averages are `sum / quality_samples`;
--   the sums stay additive across buckets, roll-ups and nodes.
-- * `sessions.quality_stats` (JSONB, since the initial schema) now holds the running
--   `QualitySummary` of the session, written periodically (`quality.persist_interval_secs`)
--   and on disconnect, and carried over on cross-node takeover. The index serves the
--   `GET /v1/analytics/sessions` range scan.
ALTER TABLE usage_app_buckets
    ADD COLUMN IF NOT EXISTS quality_samples BIGINT NOT NULL DEFAULT 0,
    ADD COLUMN IF NOT EXISTS mos_sum_milli BIGINT NOT NULL DEFAULT 0,
    ADD COLUMN IF NOT EXISTS rtt_sum_ms BIGINT NOT NULL DEFAULT 0,
    ADD COLUMN IF NOT EXISTS jitter_sum_ms BIGINT NOT NULL DEFAULT 0,
    ADD COLUMN IF NOT EXISTS loss_sum_permille BIGINT NOT NULL DEFAULT 0,
    ADD COLUMN IF NOT EXISTS poor_quality_samples BIGINT NOT NULL DEFAULT 0;

CREATE INDEX IF NOT EXISTS idx_sessions_app_connected ON sessions(app_id, connected_at DESC);
