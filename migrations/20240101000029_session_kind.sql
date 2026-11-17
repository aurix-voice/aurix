-- Participant kind per session: 'user' (counted) or 'service' (bots, recorders — never counted).
ALTER TABLE sessions ADD COLUMN IF NOT EXISTS kind TEXT NOT NULL DEFAULT 'user';
