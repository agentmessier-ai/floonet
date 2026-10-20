pub mod query;
pub mod reach;

use anyhow::Result;
use rusqlite::Connection;
use std::path::Path;

pub use query::{SearchHit, SessionRow, TurnRow};

/// Re-exported so a caller can name the handle `conn()` hands out without a
/// direct rusqlite dependency, which keeps hand-written SQL out of the crates
/// above this one.
pub use rusqlite::Connection as DbConnection;

const MIGRATIONS: &[(&str, &str)] = &[
    ("0001_init", include_str!("../migrations/0001_init.sql")),
    ("0002_reach", include_str!("../migrations/0002_reach.sql")),
    ("0003_scan", include_str!("../migrations/0003_scan.sql")),
    ("0004_panel", include_str!("../migrations/0004_panel.sql")),
    (
        "0005_provenance",
        include_str!("../migrations/0005_provenance.sql"),
    ),
    (
        "0006_presence",
        include_str!("../migrations/0006_presence.sql"),
    ),
    (
        "0007_conversation",
        include_str!("../migrations/0007_conversation.sql"),
    ),
    (
        "0008_conversation_pid_start",
        include_str!("../migrations/0008_conversation_pid_start.sql"),
    ),
    ("0009_ack", include_str!("../migrations/0009_ack.sql")),
    (
        "0010_drop_dismissed_states",
        include_str!("../migrations/0010_drop_dismissed_states.sql"),
    ),
    (
        "0011_daemon_status",
        include_str!("../migrations/0011_daemon_status.sql"),
    ),
    (
        "0012_surface_and_title_provenance",
        include_str!("../migrations/0012_surface_and_title_provenance.sql"),
    ),
    (
        "0013_backup_status",
        include_str!("../migrations/0013_backup_status.sql"),
    ),
    (
        "0014_time_units_to_millis",
        include_str!("../migrations/0014_time_units_to_millis.sql"),
    ),
    (
        "0015_enum_checks",
        include_str!("../migrations/0015_enum_checks.sql"),
    ),
    (
        "0016_drop_ingest_state_retired_at",
        include_str!("../migrations/0016_drop_ingest_state_retired_at.sql"),
    ),
    (
        "0017_drop_ingest_state",
        include_str!("../migrations/0017_drop_ingest_state.sql"),
    ),
    (
        "0018_setting",
        include_str!("../migrations/0018_setting.sql"),
    ),
    (
        "0019_message_envelope",
        include_str!("../migrations/0019_message_envelope.sql"),
    ),
];

/// The clock, unwrapped once for SQL. `Millis` has no `ToSql` impl: the orphan
/// rule keeps it out of both crates.
fn now_ms_i64() -> i64 {
    tp_core::now_ms().get()
}

/// Where floonet keeps its state. Defined once, here, because this crate owns
/// the database and every binary that opens one already depends on it; two
/// binaries deciding the path independently can quietly disagree.
///
/// Without `HOME` there is no answer, and no fallback is a safe guess: `/`
/// would put the store at `/.teleport`, which `restrict` then chmods to 0700,
/// and an existing install's index would look empty rather than missing. The
/// process stops instead of writing somewhere nobody asked for.
pub fn teleport_dir() -> std::path::PathBuf {
    let home = std::env::var_os("HOME")
        .filter(|h| !h.is_empty())
        .expect("HOME is unset: floonet keeps its index and key in $HOME/.teleport");
    std::path::PathBuf::from(home).join(".teleport")
}

pub fn default_db_path() -> std::path::PathBuf {
    // `TP_DB` points every command at another index, so a copy is readable
    // with the existing tools rather than needing its own query surface.
    if let Ok(p) = std::env::var("TP_DB") {
        return std::path::PathBuf::from(p);
    }
    daemon_db_path()
}

/// Where the daemon's local API socket lives: next to the database it serves.
///
/// It follows `TP_DB` because a socket that did not would let a daemon pointed
/// at another index unlink and rebind the socket the installed daemon is
/// serving on. One database, one socket, one daemon.
pub fn daemon_socket_path() -> std::path::PathBuf {
    let db = default_db_path();
    match db.parent() {
        Some(dir) if !dir.as_os_str().is_empty() => dir.join("tpd.sock"),
        _ => teleport_dir().join("tpd.sock"),
    }
}

/// The index the daemon writes; `TP_DB` deliberately does not move it.
///
/// The distinction matters to anything that refuses to run while tpd is live:
/// pointed at a copy, there is no race to avoid.
pub fn daemon_db_path() -> std::path::PathBuf {
    teleport_dir().join("teleport.db")
}

pub struct Db {
    conn: Connection,
}

/// Owner-only, on the directory and on all three database files, every open.
///
/// The file mode is the only access control for a process that opens the
/// database directly; `tp-app` gates only calls that go through the code. The
/// database holds turns whose transcripts may no longer exist, so it must not
/// inherit the umask.
///
/// Applied on every open rather than at creation: an existing database is
/// never recreated, and existing installs are the ones that need repairing.
/// Best-effort: a filesystem that cannot express Unix modes must still open,
/// and a warning nobody can act on is worse than the mode staying as it was.
///
/// `chmod` rather than a umask around the open: umask is process-global and
/// would narrow every other thread's file creation in the window. Covering the
/// main file covers `-wal` and `-shm`, since SQLite copies its mode onto both
/// when it creates them. The directory at 0700 is what covers the gap between
/// bind and chmod for the daemon's socket, so that line is load-bearing.
fn restrict(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let dir_mode = std::fs::Permissions::from_mode(0o700);
    let file_mode = std::fs::Permissions::from_mode(0o600);
    if let Some(parent) = path.parent() {
        let _ = std::fs::set_permissions(parent, dir_mode);
    }
    for p in [
        path.to_path_buf(),
        path.with_extension("db-wal"),
        path.with_extension("db-shm"),
    ] {
        if p.exists() {
            let _ = std::fs::set_permissions(&p, file_mode.clone());
        }
    }
}

impl Db {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let conn = Connection::open(path)?;
        // Before the migration: `-wal`/`-shm` are created at the first write
        // and inherit the main file's mode, so one restrict reaches all three.
        restrict(path);
        // WAL for concurrent readers + one writer.
        //
        // `synchronous = NORMAL` under WAL risks only the last transactions on
        // a power loss, never corruption. The rows that exist nowhere else are
        // old ones, committed and checkpointed long before their transcript
        // aged out. Disk failure and accidental deletion are covered by
        // `fl backup` and `fl verify`, not by this setting.
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.pragma_update(None, "busy_timeout", 5000)?;
        conn.pragma_update(None, "foreign_keys", true)?;
        let mut db = Self { conn };
        db.migrate()?;
        Ok(db)
    }

    /// In-memory DB for tests — same schema/pragma path as `open`, minus WAL
    /// (which requires a real file).
    pub fn open_in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory()?;
        conn.pragma_update(None, "foreign_keys", true)?;
        let mut db = Self { conn };
        db.migrate()?;
        Ok(db)
    }

    fn migrate(&mut self) -> Result<()> {
        self.conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS schema_migration (name TEXT PRIMARY KEY, applied_at INTEGER NOT NULL)",
        )?;
        for (name, sql) in MIGRATIONS {
            let already: bool = self
                .conn
                .query_row(
                    "SELECT 1 FROM schema_migration WHERE name = ?1",
                    [name],
                    |_| Ok(true),
                )
                .unwrap_or(false);
            if already {
                continue;
            }
            // IMMEDIATE: a migration is a write, and a deferred transaction
            // that promotes can lose the race with SQLITE_BUSY_SNAPSHOT, which
            // busy_timeout does not retry.
            let tx = self
                .conn
                .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
            tx.execute_batch(sql)?;
            tx.execute(
                "INSERT INTO schema_migration(name, applied_at) VALUES (?1, unixepoch())",
                [name],
            )?;
            tx.commit()?;
        }
        Ok(())
    }

    /// Register (or refresh) this machine's own `machine` row with `trust='self'`.
    pub fn ensure_self_machine(&self, machine_id: &str, name: &str) -> Result<()> {
        self.conn.execute(
            "INSERT INTO machine(id, name, is_self, trust, created_at)
             VALUES (?1, ?2, 1, 'self', ?3)
             ON CONFLICT(id) DO UPDATE SET name = excluded.name",
            rusqlite::params![machine_id, name, crate::now_ms_i64()],
        )?;
        Ok(())
    }

    /// Record that a backup was taken. One row, overwritten each time: the
    /// question is "how long since", not "how many". Written after the copy
    /// lands, so a failed `VACUUM INTO` never leaves a claim that a backup
    /// exists.
    pub fn record_backup(&self, dest: &str, turn_count: i64, bytes: u64) -> Result<()> {
        self.conn.execute(
            "INSERT INTO backup_status(id, taken_at, dest, turn_count, bytes)
             VALUES (1, ?1, ?2, ?3, ?4)
             ON CONFLICT(id) DO UPDATE SET
                 taken_at = excluded.taken_at,
                 dest = excluded.dest,
                 turn_count = excluded.turn_count,
                 bytes = excluded.bytes",
            rusqlite::params![tp_core::now_ms().get(), dest, turn_count, bytes as i64],
        )?;
        Ok(())
    }

    /// Record which build of the daemon is now serving, and from which pid.
    /// Overwrites rather than appends: the question is "what is running right
    /// now", and a previous run's row is not an answer to it.
    pub fn record_daemon_start(&self, version: &str, pid: u32) -> Result<()> {
        self.conn.execute(
            "INSERT INTO daemon_status(id, version, pid, started_at)
             VALUES (1, ?1, ?2, ?3)
             ON CONFLICT(id) DO UPDATE SET
                 version = excluded.version,
                 pid = excluded.pid,
                 started_at = excluded.started_at",
            rusqlite::params![version, pid, crate::now_ms_i64()],
        )?;
        Ok(())
    }

    pub fn ensure_runtime(&self, id: &str, root: &str) -> Result<()> {
        self.conn.execute(
            "INSERT INTO runtime(id, root) VALUES (?1, ?2)
             ON CONFLICT(id) DO UPDATE SET root = excluded.root",
            rusqlite::params![id, root],
        )?;
        Ok(())
    }

    pub fn conn(&self) -> &Connection {
        &self.conn
    }

    pub fn conn_mut(&mut self) -> &mut Connection {
        &mut self.conn
    }

    /// A transaction that takes the write lock from its first statement.
    ///
    /// The deferred default promotes on the first write, and under WAL that
    /// promotion can fail with SQLITE_BUSY_SNAPSHOT, which `busy_timeout` does
    /// not retry. Exposed here so multi-statement writers in other crates get
    /// it without naming `rusqlite`; only this crate names the type.
    pub fn begin_immediate(&mut self) -> Result<rusqlite::Transaction<'_>> {
        Ok(self
            .conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?)
    }
}
