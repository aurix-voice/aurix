-- Cascade (node-to-node relay) UDP port advertised by each media node so peers can be
-- discovered automatically instead of being listed in media.cascade_peers.
ALTER TABLE media_nodes ADD COLUMN cascade_port INTEGER;
