-- Priority speakers (ChannelConfig.ducking): a member whose channel grant carries
-- `priority: true`, or whom a moderator promoted at runtime, attenuates every other voice
-- while talking. Kept on the membership so nodes learning the roster from the database see
-- who ducks whom.
ALTER TABLE channel_memberships ADD COLUMN IF NOT EXISTS is_priority BOOLEAN NOT NULL DEFAULT FALSE;
