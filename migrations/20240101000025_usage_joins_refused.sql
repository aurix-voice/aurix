-- Connects/joins refused by a per-application cap, metered per 5-minute bucket.
ALTER TABLE usage_app_buckets ADD COLUMN joins_refused BIGINT NOT NULL DEFAULT 0;
