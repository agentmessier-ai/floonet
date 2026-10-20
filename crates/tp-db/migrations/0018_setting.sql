-- Settings a user changes and floonet obeys, as opposed to state floonet
-- records (`daemon_status`, `backup_status`: written by the daemon, read by a
-- human).
--
-- A table rather than a config file because the panel already reaches the
-- daemon over the local API, and the local API already serves this database;
-- a file would need a second parser, a second path convention, and a rule for
-- who wins when the two disagree.
--
-- No row means the default, and the default is stated in code rather than
-- seeded here: a row written by a migration is indistinguishable from a row
-- written by the user, and "the user turned this on" is exactly what the peer
-- listener must not guess about.
CREATE TABLE IF NOT EXISTS setting (
  key        TEXT PRIMARY KEY,
  value      TEXT NOT NULL,
  updated_at INTEGER NOT NULL
) STRICT;
