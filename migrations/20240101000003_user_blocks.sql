-- Persistent cross-mute ("block"): neither party hears the other in any channel, in every
-- session, until the block is removed. blocked_user_id is not a FK: a player may block someone
-- who has not logged in on this node yet.
CREATE TABLE user_blocks (
    app_id UUID NOT NULL REFERENCES apps(id) ON DELETE CASCADE,
    user_id UUID NOT NULL,
    blocked_user_id UUID NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    PRIMARY KEY (app_id, user_id, blocked_user_id),
    CHECK (user_id <> blocked_user_id)
);
CREATE INDEX idx_user_blocks_blocked ON user_blocks(app_id, blocked_user_id);
