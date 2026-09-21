-- Operator-initiated node drain (maintenance): a draining node keeps its current sessions and
-- lets them resume after a blip, but is never selected for fresh sessions, failover or region
-- discovery. The flag is owned by the operator, not the heartbeat, so it survives restarts.
ALTER TABLE media_nodes ADD COLUMN IF NOT EXISTS draining BOOLEAN NOT NULL DEFAULT FALSE;
ALTER TABLE media_nodes ADD COLUMN IF NOT EXISTS drain_reason TEXT;
ALTER TABLE media_nodes ADD COLUMN IF NOT EXISTS draining_since TIMESTAMPTZ;
ALTER TABLE media_nodes ADD COLUMN IF NOT EXISTS drained_by UUID;
