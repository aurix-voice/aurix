-- Aurix Voice Platform — Initial Schema

CREATE EXTENSION IF NOT EXISTS "uuid-ossp";

CREATE TABLE apps (
    id UUID PRIMARY KEY,
    name TEXT NOT NULL,
    description TEXT,
    owner_id UUID NOT NULL,
    api_key_hash TEXT NOT NULL,
    api_secret_hash TEXT NOT NULL,
    active BOOLEAN NOT NULL DEFAULT true,
    max_channels INTEGER NOT NULL DEFAULT 10000,
    max_participants_per_channel INTEGER NOT NULL DEFAULT 256,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);
CREATE INDEX idx_apps_owner ON apps(owner_id);
CREATE INDEX idx_apps_active ON apps(active) WHERE active = true;

CREATE TABLE users (
    id UUID PRIMARY KEY,
    app_id UUID NOT NULL REFERENCES apps(id),
    external_id TEXT NOT NULL,
    display_name TEXT NOT NULL,
    metadata JSONB,
    is_banned BOOLEAN NOT NULL DEFAULT false,
    ban_reason TEXT,
    ban_expires_at TIMESTAMPTZ,
    device_ids TEXT[] NOT NULL DEFAULT '{}',
    total_session_minutes BIGINT NOT NULL DEFAULT 0,
    last_seen_at TIMESTAMPTZ,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    UNIQUE(app_id, external_id)
);
CREATE INDEX idx_users_app ON users(app_id);
CREATE INDEX idx_users_display ON users(app_id, display_name);
CREATE INDEX idx_users_banned ON users(app_id, is_banned) WHERE is_banned = true;

CREATE TABLE channels (
    id UUID PRIMARY KEY,
    app_id UUID NOT NULL REFERENCES apps(id),
    name TEXT NOT NULL,
    channel_type TEXT NOT NULL,
    config JSONB NOT NULL DEFAULT '{}',
    max_participants INTEGER NOT NULL DEFAULT 256,
    is_persistent BOOLEAN NOT NULL DEFAULT false,
    active_participants INTEGER NOT NULL DEFAULT 0,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    deleted_at TIMESTAMPTZ
);
CREATE INDEX idx_channels_app ON channels(app_id);
CREATE INDEX idx_channels_active ON channels(app_id, active_participants) WHERE deleted_at IS NULL AND active_participants > 0;

CREATE TABLE sessions (
    id UUID PRIMARY KEY,
    user_id UUID NOT NULL REFERENCES users(id),
    app_id UUID NOT NULL REFERENCES apps(id),
    media_node_id UUID NOT NULL,
    ip_address TEXT NOT NULL,
    user_agent TEXT,
    connected_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    disconnected_at TIMESTAMPTZ,
    disconnect_reason TEXT,
    quality_stats JSONB
);
CREATE INDEX idx_sessions_user ON sessions(user_id);
CREATE INDEX idx_sessions_app ON sessions(app_id);
CREATE INDEX idx_sessions_active ON sessions(app_id, disconnected_at) WHERE disconnected_at IS NULL;
CREATE INDEX idx_sessions_node ON sessions(media_node_id);

CREATE TABLE channel_memberships (
    id UUID PRIMARY KEY,
    channel_id UUID NOT NULL REFERENCES channels(id),
    user_id UUID NOT NULL REFERENCES users(id),
    session_id UUID NOT NULL REFERENCES sessions(id),
    role TEXT NOT NULL DEFAULT 'speaker',
    is_muted BOOLEAN NOT NULL DEFAULT false,
    is_server_muted BOOLEAN NOT NULL DEFAULT false,
    ssrc BIGINT NOT NULL,
    joined_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    left_at TIMESTAMPTZ
);
CREATE INDEX idx_cm_channel ON channel_memberships(channel_id) WHERE left_at IS NULL;
CREATE INDEX idx_cm_user ON channel_memberships(user_id) WHERE left_at IS NULL;
CREATE INDEX idx_cm_session ON channel_memberships(session_id);

CREATE TABLE bans (
    id UUID PRIMARY KEY,
    app_id UUID NOT NULL REFERENCES apps(id),
    user_id UUID,
    device_id TEXT,
    ip_address TEXT,
    scope TEXT NOT NULL,
    reason TEXT NOT NULL,
    issued_by UUID NOT NULL,
    expires_at TIMESTAMPTZ,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    revoked_at TIMESTAMPTZ,
    revoked_by UUID
);
CREATE INDEX idx_bans_user ON bans(app_id, user_id) WHERE revoked_at IS NULL;
CREATE INDEX idx_bans_device ON bans(device_id) WHERE device_id IS NOT NULL AND revoked_at IS NULL;
CREATE INDEX idx_bans_ip ON bans(ip_address) WHERE ip_address IS NOT NULL AND revoked_at IS NULL;

CREATE TABLE moderation_events (
    id UUID PRIMARY KEY,
    app_id UUID NOT NULL REFERENCES apps(id),
    channel_id UUID,
    target_user_id UUID NOT NULL,
    reporter_user_id UUID,
    moderator_user_id UUID,
    event_type TEXT NOT NULL,
    reason TEXT NOT NULL,
    evidence JSONB,
    recording_id UUID,
    status TEXT NOT NULL DEFAULT 'pending',
    resolution TEXT,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    resolved_at TIMESTAMPTZ
);
CREATE INDEX idx_mod_app ON moderation_events(app_id);
CREATE INDEX idx_mod_status ON moderation_events(app_id, status);
CREATE INDEX idx_mod_target ON moderation_events(target_user_id);

CREATE TABLE recordings (
    id UUID PRIMARY KEY,
    app_id UUID NOT NULL REFERENCES apps(id),
    channel_id UUID NOT NULL,
    session_id UUID NOT NULL,
    user_id UUID NOT NULL,
    file_path TEXT NOT NULL,
    file_size_bytes BIGINT NOT NULL DEFAULT 0,
    duration_secs DOUBLE PRECISION NOT NULL DEFAULT 0,
    format TEXT NOT NULL DEFAULT 'ogg_opus',
    encrypted BOOLEAN NOT NULL DEFAULT false,
    encryption_key_id TEXT,
    started_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    ended_at TIMESTAMPTZ,
    expires_at TIMESTAMPTZ NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);
CREATE INDEX idx_rec_app_ch ON recordings(app_id, channel_id);
CREATE INDEX idx_rec_user ON recordings(user_id);
CREATE INDEX idx_rec_expires ON recordings(expires_at);

CREATE TABLE media_nodes (
    id UUID PRIMARY KEY,
    region TEXT NOT NULL,
    address TEXT NOT NULL,
    media_port INTEGER NOT NULL,
    api_port INTEGER NOT NULL,
    capacity INTEGER NOT NULL DEFAULT 50000,
    active_channels INTEGER NOT NULL DEFAULT 0,
    active_participants INTEGER NOT NULL DEFAULT 0,
    cpu_usage DOUBLE PRECISION NOT NULL DEFAULT 0,
    memory_usage DOUBLE PRECISION NOT NULL DEFAULT 0,
    bandwidth_in_mbps DOUBLE PRECISION NOT NULL DEFAULT 0,
    bandwidth_out_mbps DOUBLE PRECISION NOT NULL DEFAULT 0,
    healthy BOOLEAN NOT NULL DEFAULT true,
    version TEXT NOT NULL DEFAULT '1.0.0',
    last_heartbeat TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    registered_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);
CREATE INDEX idx_nodes_region ON media_nodes(region, healthy) WHERE healthy = true;

CREATE TABLE audit_log (
    id UUID PRIMARY KEY,
    app_id UUID,
    actor_id UUID NOT NULL,
    action TEXT NOT NULL,
    target_type TEXT NOT NULL,
    target_id TEXT NOT NULL,
    details JSONB NOT NULL DEFAULT '{}',
    ip_address TEXT,
    previous_hash TEXT NOT NULL,
    hash TEXT NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);
CREATE INDEX idx_audit_app ON audit_log(app_id, created_at DESC);
CREATE INDEX idx_audit_actor ON audit_log(actor_id);

CREATE TABLE api_keys (
    id UUID PRIMARY KEY,
    app_id UUID NOT NULL REFERENCES apps(id),
    name TEXT NOT NULL,
    key_prefix TEXT NOT NULL,
    key_hash TEXT NOT NULL,
    permissions JSONB NOT NULL DEFAULT '{}',
    rate_limit INTEGER NOT NULL DEFAULT 100,
    active BOOLEAN NOT NULL DEFAULT true,
    last_used_at TIMESTAMPTZ,
    expires_at TIMESTAMPTZ,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    revoked_at TIMESTAMPTZ
);
CREATE UNIQUE INDEX idx_apikeys_prefix ON api_keys(key_prefix) WHERE active = true;
CREATE INDEX idx_apikeys_app ON api_keys(app_id);

CREATE TABLE analytics_snapshots (
    id UUID PRIMARY KEY,
    app_id UUID NOT NULL REFERENCES apps(id),
    timestamp TIMESTAMPTZ NOT NULL,
    active_users BIGINT NOT NULL DEFAULT 0,
    active_channels BIGINT NOT NULL DEFAULT 0,
    peak_concurrent BIGINT NOT NULL DEFAULT 0,
    total_minutes DOUBLE PRECISION NOT NULL DEFAULT 0,
    bandwidth_gb DOUBLE PRECISION NOT NULL DEFAULT 0,
    avg_latency_ms DOUBLE PRECISION NOT NULL DEFAULT 0,
    avg_packet_loss DOUBLE PRECISION NOT NULL DEFAULT 0,
    error_count BIGINT NOT NULL DEFAULT 0
);
CREATE INDEX idx_analytics_app_ts ON analytics_snapshots(app_id, timestamp);

CREATE TABLE admin_users (
    id UUID PRIMARY KEY,
    email TEXT NOT NULL UNIQUE,
    password_hash TEXT NOT NULL,
    display_name TEXT NOT NULL,
    role TEXT NOT NULL DEFAULT 'admin',
    active BOOLEAN NOT NULL DEFAULT true,
    last_login_at TIMESTAMPTZ,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);
CREATE UNIQUE INDEX idx_admin_email ON admin_users(email) WHERE active = true;