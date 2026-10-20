-- Source identity / lineage / cost per turn.
--
-- Additive: every column is nullable and old rows keep NULL. New ingests
-- populate them from the source JSONL; a re-ingest backfills any session whose
-- file still exists. Captured eagerly because the fields are unrecoverable
-- once the runtime deletes the transcript.
--
-- `uuid` is the sound turn coordinate the format carries; (session_id, ts)
-- collides.
ALTER TABLE turn ADD COLUMN uuid TEXT;
ALTER TABLE turn ADD COLUMN parent_uuid TEXT;
ALTER TABLE turn ADD COLUMN model TEXT;
ALTER TABLE turn ADD COLUMN cache_read_tokens INTEGER;
ALTER TABLE turn ADD COLUMN cache_creation_tokens INTEGER;

-- Look up a turn by its source uuid within a session. Partial: only turns
-- that have one.
CREATE INDEX turn_uuid ON turn(session_id, uuid) WHERE uuid IS NOT NULL;
