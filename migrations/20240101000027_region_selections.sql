-- Region-selection telemetry: what the SDK finally picked after its RTT probe vs what the
-- node recommended, so GeoIP/hint accuracy and "players in the wrong region" are measurable.
CREATE TABLE region_selections (
    id UUID PRIMARY KEY,
    app_id UUID NOT NULL,
    user_id UUID NOT NULL,
    reporting_node_id TEXT NOT NULL,
    recommended_region TEXT,
    recommended_node_id TEXT,
    chosen_region TEXT NOT NULL,
    chosen_node_id TEXT NOT NULL,
    location_source TEXT NOT NULL,
    probes JSONB NOT NULL DEFAULT '[]'::jsonb,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX region_selections_app_created_idx ON region_selections (app_id, created_at DESC);
