-- Audio rating (what the client actually played) metered next to the network rating.
ALTER TABLE usage_app_buckets ADD COLUMN audio_quality_samples BIGINT NOT NULL DEFAULT 0;
ALTER TABLE usage_app_buckets ADD COLUMN audio_mos_sum_milli BIGINT NOT NULL DEFAULT 0;
ALTER TABLE usage_app_buckets ADD COLUMN concealed_sum_permille BIGINT NOT NULL DEFAULT 0;
