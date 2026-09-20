-- Public IPv6 address of a media node (dual-stack deployments). Nullable so older nodes in a
-- rolling upgrade keep registering; the cascade peer picker falls back to `address`.
ALTER TABLE media_nodes
    ADD COLUMN address_ipv6 TEXT;
