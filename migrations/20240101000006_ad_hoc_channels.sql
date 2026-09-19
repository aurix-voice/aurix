-- Channels created on demand by a join grant (`ad_hoc` in the token). They are soft-deleted
-- automatically once the last participant leaves and re-created on the next join. Their id is
-- derived from (app_id, name), so the same name always maps to the same channel.
ALTER TABLE channels ADD COLUMN ad_hoc BOOLEAN NOT NULL DEFAULT false;
