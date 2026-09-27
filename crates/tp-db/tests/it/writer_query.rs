//! The database's own integration tests: the transaction-mode guarantee the
//! daemon depends on, backup status, the millis migration, the seconds-clock
//! gate, and the enum-check migration.

use tp_db::Db;

/// A DEFERRED transaction starts as a read and promotes on its first write; in
/// WAL mode a commit from another connection in between fails that promotion
/// with SQLITE_BUSY_SNAPSHOT, which `busy_timeout` does not cover because it is
/// a snapshot invalidation, not a lock wait. Driven by hand on both behaviours.
#[test]
fn a_deferred_transaction_loses_the_promotion_race_and_immediate_does_not() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("t.db");
    {
        let db = tp_db::Db::open(&path).unwrap();
        db.ensure_self_machine("m1", "TestMac").unwrap();
    }

    let open = || {
        let c = rusqlite::Connection::open(&path).unwrap();
        c.pragma_update(None, "journal_mode", "WAL").unwrap();
        c.pragma_update(None, "busy_timeout", 5000).unwrap();
        c
    };

    // DEFERRED: read first, let another connection commit, then write.
    let mut a = open();
    let b = open();
    let tx = a
        .transaction_with_behavior(rusqlite::TransactionBehavior::Deferred)
        .unwrap();
    tx.query_row("SELECT count(*) FROM machine", [], |r| r.get::<_, i64>(0))
        .unwrap(); // the snapshot is taken here
    b.execute(
        "INSERT INTO machine(id, name, trust, created_at) VALUES ('x','x','trusted',unixepoch() * 1000)",
        [],
    )
    .unwrap();
    let err = tx
        .execute(
            "INSERT INTO machine(id, name, trust, created_at) VALUES ('y','y','trusted',unixepoch() * 1000)",
            [],
        )
        .unwrap_err();
    // Whether the extended "cannot promote" text or the plain "database is
    // locked" surfaces depends on whether extended result codes are enabled;
    // both are the same event, and the wording is rusqlite's, not the race's.
    let msg = err.to_string();
    assert!(
        msg.contains("promote") || msg.contains("locked"),
        "expected the promotion to fail, got: {err}"
    );
    drop(tx);

    // IMMEDIATE: the write lock is taken at BEGIN, so the other connection's
    // commit cannot land underneath; the contention becomes a wait, which
    // busy_timeout does cover.
    let tx = a
        .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
        .unwrap();
    tx.query_row("SELECT count(*) FROM machine", [], |r| r.get::<_, i64>(0))
        .unwrap();
    tx.execute(
        "INSERT INTO machine(id, name, trust, created_at) VALUES ('z','z','trusted',unixepoch() * 1000)",
        [],
    )
    .expect("an IMMEDIATE transaction must not lose a promotion race — it never promotes");
    tx.commit().unwrap();
}

/// "Never backed up" and "backed up today" must not render the same: a missing
/// row shown as "0 days ago" would turn the one state that needs action into
/// the one that needs none.
mod backup_status {
    use tp_db::Db;

    #[test]
    fn absent_is_not_zero_days_ago() {
        let db = Db::open_in_memory().unwrap();
        assert!(
            tp_db::query::backup_status(db.conn()).unwrap().is_none(),
            "a fresh index has never been backed up, and must say so"
        );
    }

    /// One row, overwritten: a log would be a second thing to prune, and the
    /// question is "how long since", not "how many".
    #[test]
    fn recording_twice_keeps_the_latest_only() {
        let db = Db::open_in_memory().unwrap();
        db.record_backup("/first.db", 100, 1_000).unwrap();
        db.record_backup("/second.db", 250, 2_000).unwrap();

        let b = tp_db::query::backup_status(db.conn()).unwrap().unwrap();
        assert_eq!(b.dest, "/second.db");
        assert_eq!(b.turn_count, 250, "the count is the drift baseline");
        assert_eq!(b.bytes, 2_000);

        let rows: i64 = db
            .conn()
            .query_row("SELECT count(*) FROM backup_status", [], |r| r.get(0))
            .unwrap();
        assert_eq!(rows, 1, "one row, enforced by the CHECK on id");
    }

    /// The turn count is stored so age can be read against drift: 40 days is
    /// fine on an idle machine and alarming on one that added 100k turns since.
    #[test]
    fn the_recorded_count_is_the_drift_baseline() {
        let db = Db::open_in_memory().unwrap();
        db.record_backup("/snap.db", 500, 1_234).unwrap();
        let b = tp_db::query::backup_status(db.conn()).unwrap().unwrap();
        assert_eq!(b.turn_count, 500);
        assert!(b.taken_at.get() > 0, "a timestamp, not a placeholder");
    }
}

/// The 0014 migration converts; it must never reinterpret, and it must survive
/// being run twice. The guard (`< 100000000000`) is the design: a value in
/// milliseconds is far above it and a value in seconds cannot reach it for
/// millennia, which makes the migration idempotent and safe to re-apply after
/// a partial failure.
#[test]
fn the_millis_migration_converts_seconds_once_and_only_once() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("t.db");

    // A row as an older build wrote it: seconds.
    {
        let db = Db::open(&path).unwrap();
        db.conn()
            .execute(
                "INSERT INTO machine(id, name, trust, created_at, paired_at)
                 VALUES ('peer', 'peer', 'trusted', 1785727980, 1785727997)",
                [],
            )
            .unwrap();
        // Undo the migration's bookkeeping so reopening re-runs it.
        db.conn()
            .execute(
                "DELETE FROM schema_migration WHERE name = '0014_time_units_to_millis'",
                [],
            )
            .unwrap();
    }

    let read = |p: &std::path::Path| -> (i64, i64) {
        let db = Db::open(p).unwrap();
        db.conn()
            .query_row(
                "SELECT created_at, paired_at FROM machine WHERE id = 'peer'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap()
    };

    let (c1, p1) = read(&path);
    assert_eq!(c1, 1785727980 * 1000, "seconds must be converted exactly");
    assert_eq!(p1, 1785727997 * 1000);

    // Re-running must be a no-op.
    {
        let db = Db::open(&path).unwrap();
        db.conn()
            .execute(
                "DELETE FROM schema_migration WHERE name = '0014_time_units_to_millis'",
                [],
            )
            .unwrap();
    }
    let (c2, p2) = read(&path);
    assert_eq!((c2, p2), (c1, p1), "a second run must multiply nothing");

    // The one column deliberately left in seconds stays there: it is written
    // by the migration machinery itself, so converting it would mean a
    // migration rewriting its own bookkeeping.
    let db = Db::open(&path).unwrap();
    let applied: i64 = db
        .conn()
        .query_row("SELECT MAX(applied_at) FROM schema_migration", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert!(
        applied < 100_000_000_000,
        "schema_migration.applied_at must stay in seconds, got {applied}"
    );
}

/// No SQL statement in this workspace may bind a raw seconds clock.
///
/// The write side is the unchecked half of the one-unit rule: `Millis` is
/// unwrapped with `.get()` for `params!`, so a site that builds an `i64` some
/// other way compiles and stores the wrong unit. A grep-shaped test rather
/// than a type because the type cannot reach here: `tp-core` has no `rusqlite`
/// and the orphan rule stops `tp-db` from implementing `ToSql` for its type.
#[test]
fn no_sql_in_any_crate_writes_a_seconds_clock() {
    // The net is the whole workspace, not this crate. Only each crate's
    // `src/`: test fixtures may write whatever they like.
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join("crates");
    let mut offenders = Vec::new();
    let mut stack: Vec<std::path::PathBuf> = vec![];
    for e in std::fs::read_dir(&root).unwrap().flatten() {
        let src = e.path().join("src");
        if src.is_dir() {
            stack.push(src);
        }
    }
    while let Some(dir) = stack.pop() {
        for e in std::fs::read_dir(&dir).unwrap().flatten() {
            let p = e.path();
            if p.is_dir() {
                stack.push(p);
                continue;
            }
            if p.extension().is_none_or(|x| x != "rs") {
                continue;
            }
            let text = std::fs::read_to_string(&p).unwrap();
            for (i, line) in text.lines().enumerate() {
                // `unixepoch()` is SQL's seconds clock. The one legitimate use
                // is `schema_migration.applied_at`, left in seconds because it
                // is written by the migration machinery itself. Comments may
                // quote an idiom; only code is hunted.
                let code = line.trim_start();
                let in_comment =
                    code.starts_with("//") || code.starts_with("* ") || code.starts_with("*/");
                if !in_comment && line.contains("unixepoch()") && !line.contains("schema_migration")
                {
                    offenders.push(format!("{}:{}  {}", p.display(), i + 1, line.trim()));
                }
                if !in_comment
                    && (line.contains("as_secs() as i64")
                        || line.contains("Utc::now().timestamp()"))
                {
                    offenders.push(format!("{}:{}  {}", p.display(), i + 1, line.trim()));
                }
                // The idiom with its cast taken off: a seconds clock reaching
                // a parameter list.
                if !in_comment && line.contains("params![") && line.contains("as_secs()") {
                    offenders.push(format!("{}:{}  {}", p.display(), i + 1, line.trim()));
                }
            }
        }
    }
    assert!(
        offenders.is_empty(),
        "seconds clock reaching a millisecond column:\n{}",
        offenders.join("\n")
    );
}

/// 0015 rebuilds `live_session` in place; the rebuild must lose nothing.
///
/// SQLite has no ALTER TABLE ADD CONSTRAINT, so a CHECK means
/// create-copy-drop-rename, and every part of the old table that is not a
/// column comes along by hand or not at all: the rows, the partial index the
/// declared-staleness sweep scans, and the constraint itself.
///
/// `machine.trust` is deliberately not covered. Rebuilding that table cascades
/// through `session.machine_id ... ON DELETE CASCADE`, and `PRAGMA
/// foreign_keys = OFF` is a no-op inside the transaction `migrate()` runs
/// every migration in.
#[test]
fn the_enum_check_migration_keeps_rows_index_and_constraint() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("t.db");
    let db = Db::open(&path).unwrap();

    db.conn()
        .execute(
            "INSERT INTO live_session(session_id, pid, registered_at, last_seen_at, source, presence)
             VALUES ('m/claude_code/a', 1, 0, 0, 'hook', 'declared')",
            [],
        )
        .unwrap();

    // The rebuilt table accepts a row. The insert happens after migrate() ran
    // 0015, so this does not test row survival across the rebuild.
    let n: i64 = db
        .conn()
        .query_row("SELECT COUNT(*) FROM live_session", [], |r| r.get(0))
        .unwrap();
    assert_eq!(n, 1);

    // The partial index came back. Losing it turns the declared sweep into a
    // full scan, with no symptom until the table is large.
    let idx: i64 = db
        .conn()
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master
              WHERE type = 'index' AND name = 'live_session_declared'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        idx, 1,
        "the partial index must be recreated after the rebuild"
    );

    // And the constraint is real, in both directions.
    for (col, bad) in [("presence", "scann"), ("source", "Hook")] {
        let e = db
            .conn()
            .execute(&format!("UPDATE live_session SET {col} = '{bad}'"), []);
        assert!(
            e.is_err(),
            "{col} = {bad:?} must be refused, not stored and then read as something else"
        );
    }
    db.conn()
        .execute("UPDATE live_session SET presence = 'declared'", [])
        .expect("a legal value must still be accepted");
}
