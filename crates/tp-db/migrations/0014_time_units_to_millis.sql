-- One unit for the whole schema: unix milliseconds.
--
-- Some columns were stored in seconds while the rest were in milliseconds, and
-- nothing declared which. i64 accepts either, so a wrong unit never errors:
-- seconds read as milliseconds land in 1970, milliseconds read as seconds in
-- the far future, and both render as a plausible date.
--
-- `schema_migration.applied_at` is not converted: it is written by the
-- migration machinery itself, and a migration must not mutate its own
-- bookkeeping.
--
-- The guard is load-bearing: `< 100000000000` is what makes this idempotent
-- and survivable if partially applied. A value already in milliseconds is far
-- above the bound; a value in seconds cannot reach it for millennia. Re-running
-- multiplies nothing.

UPDATE machine
   SET created_at = created_at * 1000
 WHERE created_at IS NOT NULL AND created_at < 100000000000;

UPDATE machine
   SET paired_at = paired_at * 1000
 WHERE paired_at IS NOT NULL AND paired_at < 100000000000;

UPDATE machine
   SET last_seen_at = last_seen_at * 1000
 WHERE last_seen_at IS NOT NULL AND last_seen_at < 100000000000;

UPDATE daemon_status
   SET started_at = started_at * 1000
 WHERE started_at IS NOT NULL AND started_at < 100000000000;
