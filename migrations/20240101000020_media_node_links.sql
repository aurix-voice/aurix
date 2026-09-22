-- Measured cascade links: every node publishes the peers it can currently reach on the
-- cascade port (transport actually in use and smoothed RTT). The planner on every node reads
-- the same rows, so hub election and inter-hub relaying stay deterministic fleet-wide.
CREATE TABLE IF NOT EXISTS media_node_links (
    node_id UUID NOT NULL REFERENCES media_nodes(id) ON DELETE CASCADE,
    peer_id UUID NOT NULL REFERENCES media_nodes(id) ON DELETE CASCADE,
    transport TEXT NOT NULL CHECK (transport IN ('udp', 'tcp')),
    rtt_ms INTEGER NOT NULL CHECK (rtt_ms >= 0),
    measured_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    PRIMARY KEY (node_id, peer_id)
);
CREATE INDEX IF NOT EXISTS idx_media_node_links_measured_at ON media_node_links (measured_at);
-- When a node last published its link table (also when it confirmed no peer at all); nodes
-- that never did are planned with the pre-measurement rules.
ALTER TABLE media_nodes ADD COLUMN IF NOT EXISTS links_reported_at TIMESTAMPTZ;
