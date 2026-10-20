//! Read paths. Search returns coordinates and a snippet, never a full
//! conversation: the caller decides whether a hit is worth spending context on.

use anyhow::Result;
use rusqlite::{params, Connection, OptionalExtension};

#[derive(Debug, Clone)]
pub struct SessionRow {
    pub id: String,
    pub runtime_id: String,
    pub cwd: Option<String>,
    pub title: Option<String>,
    pub last_turn_at: Option<i64>,
    pub turn_count: i64,
}

#[derive(Debug, Clone)]
pub struct TurnRow {
    pub seq: i64,
    pub role: String,
    pub ts: Option<i64>,
    pub text: String,
    /// `None` unless the caller explicitly asked for it: thinking is opt-in at
    /// read time even when it has been indexed.
    pub thinking: Option<String>,
    /// Reasoning happened and is unreadable (`thinking_state = 'opaque'`).
    /// Selected unconditionally, unlike `thinking`: the fact that a turn
    /// reasoned is one flag, and an index read must not assert "no reasoning"
    /// where a scan of the same file says otherwise.
    pub thinking_opaque: bool,
    /// Whether this turn is still live context (`turn.surface`). Selected
    /// unconditionally for the same reason as `thinking_opaque`: an index read
    /// and a scan of the same file must agree.
    pub surface: tp_core::turn::Surface,
    /// Names of the tools this turn invoked. Always selected, so an index read
    /// carries what a scan does.
    pub tool_calls: Vec<tp_core::turn::ToolCallDigest>,
    /// Source identity/lineage/cost. Round-tripped so an index read carries
    /// what a scan does.
    pub prov: tp_core::turn::Provenance,
}

#[derive(Debug, Clone)]
pub struct SearchHit {
    pub session_id: String,
    pub machine_id: String,
    pub seq: i64,
    pub ts: Option<i64>,
    pub uuid: Option<String>,
    pub role: String,
    pub snippet: String,
    pub rank: f64,
    pub sidechain: bool,
    pub surface: tp_core::turn::Surface,
}

/// What narrows a search, as one value. A struct cannot be partially passed,
/// so a filter cannot be forgotten one call site at a time.
///
/// Deliberately not `tp_core::Scope`: that type carries a `since: Duration`
/// resolved against a clock, and this layer wants the resolved bounds. Taking
/// `Scope` here would put the clock in tp-db.
#[derive(Debug, Clone, Copy, Default)]
pub struct SearchFilters<'a> {
    pub since_ms: Option<i64>,
    pub until_ms: Option<i64>,
    pub cwd: Option<&'a str>,
    /// Empty means "every runtime", matching `Scope::runtimes`.
    pub runtimes: &'a [String],
}

// ── Federation / pairing ─────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct MachineRow {
    pub id: String,
    pub name: String,
    pub trust: String,
    pub pubkey: Option<Vec<u8>>,
    pub addr: Option<String>,
    pub last_seen_at: Option<tp_core::Millis>,
}

/// Every session in the window that claims a transcript on disk, with how many
/// turns it holds: the input to "how much of this window can a scan see".
///
/// Returns the claim, not the answer: whether each path still exists is a
/// filesystem question, and the caller stats them, so storage never reaches
/// into the filesystem and the cost is visible where it is paid.
pub fn sessions_claiming_a_file(
    conn: &Connection,
    since_ms: i64,
    until_ms: Option<i64>,
) -> Result<Vec<(String, i64)>> {
    let mut stmt = conn.prepare(
        "SELECT source_path, turn_count FROM session
          WHERE source_path IS NOT NULL
            AND last_turn_at IS NOT NULL
            AND last_turn_at >= ?1
            AND (?2 IS NULL OR last_turn_at < ?2)",
    )?;
    let rows = stmt
        .query_map(params![since_ms, until_ms], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<rusqlite::Result<_>>()?;
    Ok(rows)
}

/// The other half of the same question: sessions in the window that never had a
/// transcript at all. Push-ingested runtimes write no file, so there is nothing
/// to stat and nothing for a scan to find. Counted rather than listed because
/// the caller has no filesystem work to do on them.
pub fn sessions_without_a_file(
    conn: &Connection,
    since_ms: i64,
    until_ms: Option<i64>,
) -> Result<(usize, i64)> {
    let (n, turns): (i64, Option<i64>) = conn.query_row(
        "SELECT COUNT(*), SUM(turn_count) FROM session
          WHERE source_path IS NULL
            AND last_turn_at IS NOT NULL
            AND last_turn_at >= ?1
            AND (?2 IS NULL OR last_turn_at < ?2)",
        params![since_ms, until_ms],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    Ok((n as usize, turns.unwrap_or(0)))
}

/// What the resident daemon published about itself when it started.
///
/// `None` means no daemon has recorded a start, not that none is running; a
/// caller must not report the second.
#[derive(Debug, Clone)]
pub struct DaemonStatus {
    pub version: String,
    pub pid: i64,
    pub started_at: tp_core::Millis,
}

pub fn daemon_status(conn: &Connection) -> Result<Option<DaemonStatus>> {
    conn.query_row(
        "SELECT version, pid, started_at FROM daemon_status WHERE id = 1",
        [],
        |r| {
            Ok(DaemonStatus {
                version: r.get(0)?,
                pid: r.get(1)?,
                started_at: tp_core::Millis::new(r.get(2)?),
            })
        },
    )
    .optional()
    .map_err(Into::into)
}

/// The last backup, if one was ever taken.
#[derive(Debug, Clone)]
pub struct BackupStatus {
    pub taken_at: tp_core::Millis,
    pub dest: String,
    pub turn_count: i64,
    pub bytes: i64,
}

/// `None` means no backup has ever been recorded, which for an index holding
/// the only copy of many turns is the state that needs action; the caller must
/// not render it as "0 days ago".
pub fn backup_status(conn: &Connection) -> Result<Option<BackupStatus>> {
    conn.query_row(
        "SELECT taken_at, dest, turn_count, bytes FROM backup_status WHERE id = 1",
        [],
        |r| {
            Ok(BackupStatus {
                taken_at: tp_core::Millis::new(r.get(0)?),
                dest: r.get(1)?,
                turn_count: r.get(2)?,
                bytes: r.get(3)?,
            })
        },
    )
    .optional()
    .map_err(Into::into)
}

pub fn machine(conn: &Connection, id: &str) -> Result<Option<MachineRow>> {
    conn.query_row(
        "SELECT id, name, trust, pubkey, addr, last_seen_at FROM machine WHERE id = ?1",
        [id],
        |r| {
            Ok(MachineRow {
                id: r.get(0)?,
                name: r.get(1)?,
                trust: r.get(2)?,
                pubkey: r.get(3)?,
                addr: r.get(4)?,
                last_seen_at: r.get::<_, Option<i64>>(5)?.map(tp_core::Millis::new),
            })
        },
    )
    .optional()
    .map_err(Into::into)
}

/// Record where a peer was last reached. Discovery is not trust: this only
/// updates a machine we already have a relationship with, and never inserts.
/// Otherwise anyone broadcasting on the network could write rows into every
/// listener's `machine` table.
pub fn touch_peer(conn: &Connection, device_id: &str, addr: &str) -> Result<bool> {
    let now = tp_core::now_ms().get();
    let n = conn.execute(
        "UPDATE machine SET addr = ?1, last_seen_at = ?2 WHERE id = ?3 AND is_self = 0",
        params![addr, now, device_id],
    )?;
    Ok(n > 0)
}

/// A human name for a working directory; the panel shows it instead of a long
/// path. The accessors live here so the schema has one owner.
pub fn terminal_alias(conn: &Connection, cwd: &str) -> Result<Option<String>> {
    Ok(conn
        .query_row(
            "SELECT alias FROM terminal_alias WHERE cwd = ?1",
            [cwd],
            |r| r.get(0),
        )
        .optional()?)
}
/// One setting, or `None` when the user has never chosen. Absent resolves to
/// the default, and a caller must be able to tell that apart from an explicit
/// choice of the same value.
pub fn setting(conn: &Connection, key: &str) -> Result<Option<String>> {
    Ok(conn
        .query_row("SELECT value FROM setting WHERE key = ?1", [key], |r| {
            r.get(0)
        })
        .optional()?)
}

pub fn set_setting(conn: &Connection, key: &str, value: &str, now: tp_core::Millis) -> Result<()> {
    conn.execute(
        "INSERT INTO setting(key, value, updated_at) VALUES(?1, ?2, ?3)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value, updated_at = excluded.updated_at",
        rusqlite::params![key, value, now.get()],
    )?;
    Ok(())
}

pub fn set_terminal_alias(
    conn: &Connection,
    cwd: &str,
    alias: &str,
    now: tp_core::Millis,
) -> Result<()> {
    conn.execute(
        "INSERT INTO terminal_alias(cwd, alias, updated_at) VALUES(?1, ?2, ?3)
         ON CONFLICT(cwd) DO UPDATE SET alias = excluded.alias, updated_at = excluded.updated_at",
        rusqlite::params![cwd, alias, now.get()],
    )?;
    Ok(())
}

/// Every machine we have a relationship with, in any trust state.
/// `trusted_peers` is the query-time subset.
pub fn all_peers(conn: &Connection) -> Result<Vec<MachineRow>> {
    let mut stmt = conn.prepare(
        "SELECT id, name, trust, pubkey, addr, last_seen_at FROM machine
         WHERE is_self = 0 ORDER BY trust, name",
    )?;
    let rows = stmt
        .query_map([], |r| {
            Ok(MachineRow {
                id: r.get(0)?,
                name: r.get(1)?,
                trust: r.get(2)?,
                pubkey: r.get(3)?,
                addr: r.get(4)?,
                last_seen_at: r.get::<_, Option<i64>>(5)?.map(tp_core::Millis::new),
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

/// Peers we can query (trusted). Never includes self.
pub fn trusted_peers(conn: &Connection) -> Result<Vec<MachineRow>> {
    let mut stmt = conn.prepare(
        "SELECT id, name, trust, pubkey, addr, last_seen_at FROM machine
         WHERE trust = 'trusted' AND is_self = 0",
    )?;
    let rows = stmt
        .query_map([], |r| {
            Ok(MachineRow {
                id: r.get(0)?,
                name: r.get(1)?,
                trust: r.get(2)?,
                pubkey: r.get(3)?,
                addr: r.get(4)?,
                last_seen_at: r.get::<_, Option<i64>>(5)?.map(tp_core::Millis::new),
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

/// Every session row with the path it claims and how many turns it holds.
///
/// Returns the claim, not the judgement: whether a path still resolves is a
/// filesystem question, so the caller stats them and storage never reaches into
/// the filesystem.
pub fn sessions_with_source_paths(conn: &Connection) -> Result<Vec<(String, Option<String>, i64)>> {
    let mut stmt = conn.prepare("SELECT id, source_path, turn_count FROM session")?;
    let rows = stmt
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
        .collect::<std::result::Result<_, _>>()?;
    Ok(rows)
}

/// SQLite's own verdict plus the three structural counts SQLite cannot check,
/// which are floonet's invariants rather than the engine's.
pub struct VerifyCounts {
    pub pragma: String,
    pub pragma_result: String,
    pub miscounted: i64,
    pub orphans: i64,
    pub unindexed: i64,
}

/// `full` selects `integrity_check` over `quick_check`: the page-by-page pass is
/// for a result being doubted rather than sampled.
pub fn verify_counts(conn: &Connection, full: bool) -> Result<VerifyCounts> {
    let pragma = if full {
        "integrity_check"
    } else {
        "quick_check"
    };
    let pragma_result: String = conn.query_row(&format!("PRAGMA {pragma}"), [], |r| r.get(0))?;
    let count = |sql: &str| -> Result<i64> { Ok(conn.query_row(sql, [], |r| r.get(0))?) };
    Ok(VerifyCounts {
        pragma: pragma.to_string(),
        pragma_result,
        miscounted: count(
            "SELECT count(*) FROM session s
              WHERE s.turn_count != (SELECT count(*) FROM turn t WHERE t.session_id = s.id)",
        )?,
        orphans: count(
            "SELECT count(*) FROM turn t
              WHERE NOT EXISTS (SELECT 1 FROM session s WHERE s.id = t.session_id)",
        )?,
        unindexed: count("SELECT count(*) FROM turn")? - count("SELECT count(*) FROM turn_fts")?,
    })
}

/// Turns at or after `cutoff`. The cutoff is the caller's arithmetic.
pub fn turns_since(conn: &Connection, cutoff: tp_core::Millis) -> Result<i64> {
    Ok(conn.query_row(
        "SELECT count(*) FROM turn WHERE ts >= ?1",
        [cutoff.get()],
        |r| r.get(0),
    )?)
}

/// `None` when no turn carries a usable timestamp.
pub fn oldest_turn_ms(conn: &Connection) -> Result<Option<i64>> {
    Ok(conn.query_row("SELECT min(ts) FROM turn WHERE ts > 0", [], |r| r.get(0))?)
}

pub fn turn_count(conn: &Connection) -> Result<i64> {
    Ok(conn.query_row("SELECT count(*) FROM turn", [], |r| r.get(0))?)
}

#[cfg(test)]
mod turn_columns_tests {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Db;

    /// A session row that cannot be read is not a session with no file. The
    /// caller stats what it is handed and reports the rest as unscannable, so
    /// a dropped row becomes a claim that the window was fully covered.
    #[test]
    fn a_row_that_cannot_be_read_is_an_error_not_a_short_list() {
        let db = Db::open_in_memory().unwrap();
        db.ensure_self_machine("m1", "TestMac").unwrap();
        db.ensure_runtime("codex", "/tmp").unwrap();
        let conn = db.conn();
        let session = |id: &str, path_sql: &str| {
            conn.execute(
                &format!(
                    "INSERT INTO session(id, machine_id, runtime_id, native_id, source_path,
                                         last_turn_at, turn_count)
                     VALUES ('{id}', 'm1', 'codex', '{id}', {path_sql}, 10, 3)"
                ),
                [],
            )
            .unwrap();
        };
        session("good", "'/tmp/good.jsonl'");
        // Text that is not UTF-8: storable, unreadable as a Rust `String`.
        session("bad", "CAST(X'FF' AS TEXT)");

        assert!(
            sessions_claiming_a_file(conn, 0, None).is_err(),
            "an unreadable claim must be reported, not dropped from the answer"
        );
    }
}
