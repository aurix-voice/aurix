-- Cascade relay trees: a node may run as a pure inter-regional relay hub (`relay_only`) that
-- hosts no client sessions but forwards cascade traffic between regions. Such nodes are never
-- selected for clients (they advertise no `ws_url` and zero capacity) and are preferred as
-- regional hubs by the topology planner.
ALTER TABLE media_nodes ADD COLUMN IF NOT EXISTS relay_only BOOLEAN NOT NULL DEFAULT FALSE;
