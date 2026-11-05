-- Bytes relayed through the built-in TURN server, attributed to the application whose
-- time-limited credentials opened the allocation; metered per 5-minute bucket like media bytes.
ALTER TABLE usage_app_buckets ADD COLUMN turn_bytes BIGINT NOT NULL DEFAULT 0;
