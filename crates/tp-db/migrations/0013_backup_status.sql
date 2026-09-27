-- When a backup was last taken, and of what.
--
-- On a long-running install the index is the only copy of turns whose
-- transcripts the runtime has since deleted. `fl verify` suggests `fl backup`;
-- this is what lets it say whether the suggestion was ever taken.
--
-- Not a filesystem scan: `tp backup <dest>` writes wherever the caller says,
-- so there is no directory to look in.
--
-- One row, like daemon_status: the question is "how long since the last one",
-- not the history.
--
-- Restore is the one confusing case: this row lives in the database, so a
-- snapshot carries the timestamp of the backup that produced it, and a
-- restored database reports the backup as older than the copy. That is the
-- honest reading: a restored database has not been backed up since it became
-- this database.
CREATE TABLE backup_status (
  id          INTEGER PRIMARY KEY CHECK (id = 1),
  taken_at    INTEGER NOT NULL,   -- unix ms
  dest        TEXT    NOT NULL,   -- where it went, so "which copy" is answerable
  turn_count  INTEGER NOT NULL,   -- what was in it, to see drift since
  bytes       INTEGER NOT NULL
) STRICT;
