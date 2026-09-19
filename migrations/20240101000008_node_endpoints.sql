-- Region discovery: nodes advertise the public endpoints clients should use and,
-- optionally, where they are. Additive, so nodes on the previous schema keep heartbeating
-- during a rolling upgrade; rows without ws_url are simply not advertised.
ALTER TABLE media_nodes
    ADD COLUMN ws_url TEXT,
    ADD COLUMN api_url TEXT,
    ADD COLUMN latitude DOUBLE PRECISION,
    ADD COLUMN longitude DOUBLE PRECISION;
