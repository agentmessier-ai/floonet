-- What is actually running, written by the daemon itself at startup.
--
-- Every other version signal describes a file: the binary on disk, the panel
-- bundle last installed. Installing new binaries does not restart a
-- LaunchAgent, so the resident daemon can be older than every file on disk.
--
-- Written by tpd, read by the panel. A single row: this is the state of the
-- one supervised daemon, not a history of runs. `CHECK (id = 1)` makes a
-- second row unrepresentable, so a future caller cannot turn it into a log
-- nobody prunes.
CREATE TABLE daemon_status (
  id          INTEGER PRIMARY KEY CHECK (id = 1),
  version     TEXT    NOT NULL,
  pid         INTEGER NOT NULL,
  started_at  INTEGER NOT NULL
) STRICT;
