-- User erasure and data retention.

-- A tombstone outlives the deleted `users` row so that session tokens issued before the
-- deletion cannot open a new session (and silently re-create the user). Rows are pruned by the
-- retention sweep after `retention.tombstones_days`.
CREATE TABLE user_tombstones (
    app_id UUID NOT NULL REFERENCES apps(id) ON DELETE CASCADE,
    user_id UUID NOT NULL,
    deleted_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    PRIMARY KEY (app_id, user_id)
);
CREATE INDEX idx_user_tombstones_deleted ON user_tombstones(deleted_at);

-- Indexes for the retention sweep and for erasure/export lookups.
CREATE INDEX idx_sessions_disconnected ON sessions(disconnected_at) WHERE disconnected_at IS NOT NULL;
CREATE INDEX idx_cm_user_all ON channel_memberships(user_id);
CREATE INDEX idx_mod_resolved ON moderation_events(resolved_at) WHERE resolved_at IS NOT NULL;
CREATE INDEX idx_mod_reporter ON moderation_events(reporter_user_id) WHERE reporter_user_id IS NOT NULL;
CREATE INDEX idx_audit_created ON audit_log(created_at);
CREATE INDEX idx_analytics_ts ON analytics_snapshots(timestamp);
CREATE INDEX idx_chat_messages_sent ON chat_messages(sent_at);
CREATE INDEX idx_users_last_seen ON users(app_id, last_seen_at);
