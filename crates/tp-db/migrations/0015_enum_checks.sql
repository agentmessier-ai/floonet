-- The closed sets the schema always had, told to SQLite.
--
-- `STRICT` enforces type, not value. Each column below holds one of a handful
-- of strings, and a rule that lives only in Rust is bypassed by a migration,
-- a hand-run UPDATE, or a second writer. The failure is silent in both
-- directions: a value the reader does not recognise is mapped to a default
-- (a paired machine that `WHERE trust = 'trusted'` never selects), and a
-- value a WHERE does not match is invisible to the sweep (a `live_session`
-- row the keep-exactly-one rule never sees, so a second row for its pid is
-- added every cycle and its mailbox is never drained).
--
-- Backfill, not delete, and only where the right value is knowable. An
-- out-of-domain `live_session` row is a live duplicate, so it is re-homed to
-- the value the scan authority can see and the normal sweep reclaims it.
-- `machine.trust` is not backfilled: guessing whether an unknown value meant
-- trusted or pending is guessing about a trust decision, and refusing the
-- migration so a human sees the row is the correct outcome.
--
-- `machine` is not rebuilt. `PRAGMA foreign_keys = OFF` is a no-op inside a
-- transaction, and `migrate()` wraps every migration in an IMMEDIATE one, so
-- `DROP TABLE machine` would cascade through `session.machine_id ... ON
-- DELETE CASCADE`. A CHECK on `machine.trust` needs either a rebuild outside a
-- transaction or a trigger; neither is worth it while the write side is typed
-- (`upsert_peer` takes `PairingStatus`) and `ensure_self_machine`'s ON
-- CONFLICT touches `name` only.
--
-- When that rebuild is written, the constraint is cross-column, because the
-- column carries two domains — `self` is an identity fact, the other three
-- are relationship states:
--
--   CHECK ((is_self = 1 AND trust = 'self')
--       OR (is_self = 0 AND trust IN ('pending_in', 'pending_out', 'trusted')))
--
-- It also needs an escape hatch from `migrate()`: the rebuild recipe must run
-- outside a transaction.

-- `live_session` has no inbound foreign key, so it can be rebuilt inside the
-- migration transaction without a cascade to worry about.
CREATE TABLE live_session_new (
  session_id    TEXT PRIMARY KEY,
  pid           INTEGER NOT NULL,
  tty           TEXT,
  registered_at INTEGER NOT NULL,
  last_seen_at  INTEGER NOT NULL,
  cwd           TEXT,
  last_wake_at  INTEGER,
  source        TEXT NOT NULL DEFAULT 'hook' CHECK (source IN ('hook', 'scan')),
  presence      TEXT NOT NULL DEFAULT 'scan' CHECK (presence IN ('scan', 'declared')),
  deliver       TEXT,
  stale_at      INTEGER,
  runtime_id    TEXT
) STRICT;

-- The re-home happens inside the copy rather than as a preceding UPDATE, so
-- there is no statement ordering to get wrong.
--
-- `scan` is the fail-side: a row wrongly re-homed to `scan` is pruned once by
-- the sweep and re-registers on the next cycle. Re-homed to `declared` it
-- would be orphaned permanently for a scannable runtime — the invisibility the
-- CHECK exists to prevent.
INSERT INTO live_session_new
SELECT session_id, pid, tty, registered_at, last_seen_at, cwd, last_wake_at,
       CASE WHEN source   IN ('hook', 'scan')     THEN source   ELSE 'scan' END,
       CASE WHEN presence IN ('scan', 'declared') THEN presence ELSE 'scan' END,
       deliver, stale_at, runtime_id
  FROM live_session;

DROP TABLE live_session;
ALTER TABLE live_session_new RENAME TO live_session;

-- Dropped with the old table. A partial index, and the predicate is the point:
-- the declared-staleness sweep scans it, and without it that sweep is a full
-- scan of every live row.
CREATE INDEX live_session_declared ON live_session(stale_at, last_seen_at)
  WHERE presence = 'declared';
