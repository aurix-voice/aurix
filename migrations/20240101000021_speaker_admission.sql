-- Speaker-slot admission (AudienceConfig.speaker_admission): a member whose grant allows
-- speaking may hold no slot for a while (joined while every `max_speakers` slot was taken, or
-- demoted for an active joiner). The grant stays in `role`; this flag marks the member as an
-- effective listener, so nodes learning the roster from the database count speakers the way
-- the hosting node does.
ALTER TABLE channel_memberships ADD COLUMN IF NOT EXISTS waiting_to_speak BOOLEAN NOT NULL DEFAULT FALSE;
