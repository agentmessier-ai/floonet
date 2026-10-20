-- `retired_at` was read by the checkpoint lookup and written by nothing, so
-- the filter was always true. Dropped rather than inventing the retire
-- protocol it gestures at. SQLite supports DROP COLUMN in place, and
-- `ingest_state` holds one row per source file, so the rewrite is small.
ALTER TABLE ingest_state DROP COLUMN retired_at;
