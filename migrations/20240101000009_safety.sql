-- Content safety: evidence clips of flagged speech are stored through the recording
-- subsystem as recordings of kind 'evidence' (same encryption, object storage and retention
-- sweep); incidents are moderation_events with event_type 'safety.voice' / 'safety.text'.
ALTER TABLE recordings ADD COLUMN kind TEXT NOT NULL DEFAULT 'recording';
CREATE INDEX idx_recordings_kind ON recordings(app_id, kind, started_at DESC);

CREATE INDEX idx_mod_type ON moderation_events(app_id, event_type, created_at DESC);
CREATE INDEX idx_mod_safety_user ON moderation_events(app_id, target_user_id, created_at DESC)
    WHERE event_type LIKE 'safety.%';
