//! Who is live right now: the `live_session` table.
//!
//! One decision — how presence is established, renewed and given up — so that
//! changing when a row goes stale touches this file and nothing else.

use anyhow::Result;
use rusqlite::{params, Connection, OptionalExtension};

// ---------------------------------------------------------------- live_session

/// One `live_session` row as the scan's dedupe pass needs it.
#[derive(Debug, Clone)]
pub struct ScanRow {
    pub session_id: String,
    pub source: String,
    pub registered_at: tp_core::Millis,
}

/// A `live_session` row as `fl live` displays it.
#[derive(Debug, Clone)]
pub struct LiveRow {
    pub session_id: String,
    pub pid: i32,
    pub tty: Option<String>,
    pub cwd: Option<String>,
    pub source: String,
    pub last_seen_at: tp_core::Millis,
}

/// What `resolve` needs to pick a delivery target.
#[derive(Debug, Clone)]
pub struct TargetRow {
    pub pid: i32,
    pub tty: Option<String>,
    pub deliver: Option<String>,
    pub stale_at: Option<i64>,
}

/// Every `scan`-owned row for a pid.
///
/// Scoped to `presence = 'scan'` on purpose: a `declared` row may legitimately
/// share a pid with many others (one dsh host serves many sessions), so the
/// caller's keep-exactly-one rule must never see them.
pub fn scan_rows_for_pid(conn: &Connection, pid: i32) -> Result<Vec<ScanRow>> {
    let rows = conn
        .prepare(
            "SELECT session_id, source, registered_at FROM live_session
              WHERE pid = ?1 AND presence = 'scan'",
        )?
        .query_map([pid], |r| {
            Ok(ScanRow {
                session_id: r.get(0)?,
                source: r.get(1)?,
                registered_at: tp_core::Millis::new(r.get(2)?),
            })
        })?
        .collect::<rusqlite::Result<_>>()?;
    Ok(rows)
}

/// Is this (runtime, tty) pane already held by a hook registration?
///
/// A pane runs one interactive session, but a session is not one process: a
/// runtime that supervises its own TUI is two pids, and the scan sees the
/// child while the hook sees the parent. Dedupe keyed on pid cannot merge
/// them; asking about the pane can. Scoped to `hook` rows because only a hook
/// carries a real session id.
/// Hands a registering session the placeholder the scan minted for its pane
/// before it registered: unread messages sent to that address are re-addressed
/// to `session_id`, and the placeholder row goes. Matched the way `reconcile`
/// matches them — the same runtime, and the same pid or the same pane — and
/// done at registration rather than by the next reconcile, because the drain
/// that a wake triggers follows registration at once. Returns how many
/// messages moved.
pub fn adopt_placeholders(
    conn: &Connection,
    session_id: &str,
    pid: i32,
    tty: Option<&str>,
) -> Result<usize> {
    let mut parts = session_id.splitn(3, '/');
    let (Some(machine), Some(runtime)) = (parts.next(), parts.next()) else {
        return Ok(0);
    };
    let prefix = format!("{machine}/{runtime}/scan-pid-");
    // Stored bare (`ttys004`); a registration may pass `/dev/ttys004`. An
    // empty tty is "no pane", not "the same pane", as in `hook_row_on_tty`.
    let tty = tty
        .map(|t| t.trim_start_matches("/dev/"))
        .filter(|t| !t.is_empty() && *t != "(none)")
        .unwrap_or("");
    let placeholders: Vec<String> = conn
        .prepare(
            "SELECT session_id FROM live_session
              WHERE substr(session_id, 1, length(?1)) = ?1
                AND (pid = ?2 OR (?3 <> '' AND tty = ?3))",
        )?
        .query_map(rusqlite::params![prefix, pid, tty], |r| r.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    let mut moved = 0;
    for ph in placeholders {
        moved += conn.execute(
            "UPDATE message SET to_session = ?1 WHERE to_session = ?2 AND read_at IS NULL",
            rusqlite::params![session_id, ph],
        )?;
        conn.execute("DELETE FROM live_session WHERE session_id = ?1", [&ph])?;
    }
    Ok(moved)
}

pub fn hook_row_on_tty(conn: &Connection, runtime_id: &str, tty: &str) -> Result<bool> {
    // An empty tty is "no pane", not "the same pane": matching on it would let
    // one tty-less session suppress the next.
    if tty.is_empty() || tty == "(none)" {
        return Ok(false);
    }
    Ok(conn
        .query_row(
            "SELECT 1 FROM live_session
          WHERE source = 'hook' AND tty = ?1 AND runtime_id = ?2 LIMIT 1",
            rusqlite::params![tty, runtime_id],
            |_| Ok(()),
        )
        .optional()?
        .is_some())
}

/// `(session_id, runtime_id)` for every hook registration, for a sweep that
/// asks each transcript what its session is. Rows with no runtime recorded
/// are skipped rather than guessed at.
pub fn hook_sessions(conn: &Connection) -> Result<Vec<(String, String)>> {
    let rows = conn
        .prepare(
            "SELECT session_id, runtime_id FROM live_session
              WHERE source = 'hook' AND runtime_id IS NOT NULL",
        )?
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<rusqlite::Result<_>>()?;
    Ok(rows)
}

/// `(session_id, presence)` for every row on a pid, regardless of presence:
/// the raw material for "which session am I".
pub fn rows_for_pid(conn: &Connection, pid: i32) -> Result<Vec<(String, String)>> {
    let rows = conn
        .prepare("SELECT session_id, presence FROM live_session WHERE pid = ?1")?
        .query_map([pid], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<rusqlite::Result<_>>()?;
    Ok(rows)
}

/// Every known live session, most recently seen first — the `fl live` listing.
pub fn list_live(conn: &Connection) -> Result<Vec<LiveRow>> {
    let rows = conn
        .prepare(
            "SELECT session_id, pid, tty, cwd, source, last_seen_at
               FROM live_session ORDER BY last_seen_at DESC",
        )?
        .query_map([], |r| {
            Ok(LiveRow {
                session_id: r.get(0)?,
                pid: r.get(1)?,
                tty: r.get(2)?,
                cwd: r.get(3)?,
                source: r.get(4)?,
                last_seen_at: tp_core::Millis::new(r.get(5)?),
            })
        })?
        .collect::<rusqlite::Result<_>>()?;
    Ok(rows)
}

/// Which runtimes a bare native id is registered under, right now.
///
/// A bare native id has to be composed into `<machine>/<runtime>/<native>`
/// before it is an address, and the runtime segment cannot be inferred from
/// the id's shape. Returns every match: one is an answer and two are not, and
/// the caller decides what to do with an ambiguous or absent one.
pub fn runtimes_for_native(conn: &Connection, native_id: &str) -> Result<Vec<String>> {
    let like = format!("%/{native_id}");
    let rows = conn
        .prepare("SELECT DISTINCT session_id FROM live_session WHERE session_id LIKE ?1")?
        .query_map([&like], |r| r.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let mut out: Vec<String> = rows
        .iter()
        .filter_map(|sid| {
            // Split on the first two separators only: a native id may itself
            // contain `/`, and everything after the runtime segment is it.
            let mut parts = sid.splitn(3, '/');
            let _machine = parts.next()?;
            let runtime = parts.next()?;
            let native = parts.next()?;
            // LIKE '%/x' also matches a native id that merely ends in x.
            (native == native_id).then(|| runtime.to_string())
        })
        .collect();
    out.sort();
    out.dedup();
    Ok(out)
}

pub fn target_row(conn: &Connection, session_id: &str) -> Result<Option<TargetRow>> {
    let row = conn
        .query_row(
            "SELECT pid, tty, deliver, stale_at FROM live_session WHERE session_id = ?1",
            [session_id],
            |r| {
                Ok(TargetRow {
                    pid: r.get(0)?,
                    tty: r.get(1)?,
                    deliver: r.get(2)?,
                    stale_at: r.get(3)?,
                })
            },
        )
        .optional()?;
    Ok(row)
}

/// Upsert a hook/runtime-provided registration. `source` is always `'hook'`:
/// an id the session states about itself outranks anything the scan inferred.
#[allow(clippy::too_many_arguments)]
pub fn upsert_registration(
    conn: &Connection,
    session_id: &str,
    pid: i32,
    tty: Option<&str>,
    cwd: Option<&str>,
    presence: &str,
    deliver: Option<&str>,
    runtime_id: Option<&str>,
    now: tp_core::Millis,
) -> Result<()> {
    conn.execute(
        "INSERT INTO live_session(session_id, pid, tty, cwd, source, registered_at, last_seen_at,
                                  presence, deliver, runtime_id)
         VALUES (?1, ?2, ?3, ?4, 'hook', ?5, ?5, ?6, ?7, ?8)
         ON CONFLICT(session_id) DO UPDATE SET
             pid = excluded.pid, tty = excluded.tty, cwd = excluded.cwd, source = 'hook',
             last_seen_at = excluded.last_seen_at, presence = excluded.presence,
             deliver = excluded.deliver, runtime_id = excluded.runtime_id,
             stale_at = NULL",
        params![
            session_id,
            pid,
            tty,
            cwd,
            now.get(),
            presence,
            deliver,
            runtime_id
        ],
    )?;
    Ok(())
}

/// Insert a row the scan discovered. Never overwrites an existing row's
/// `session_id` — only its location and liveness.
pub fn insert_scanned(
    conn: &Connection,
    session_id: &str,
    pid: i32,
    tty: Option<&str>,
    cwd: Option<&str>,
    now: tp_core::Millis,
) -> Result<()> {
    conn.execute(
        "INSERT INTO live_session(session_id, pid, tty, cwd, source, registered_at, last_seen_at)
         VALUES (?1, ?2, ?3, ?4, 'scan', ?5, ?5)
         ON CONFLICT(session_id) DO UPDATE SET
             pid = excluded.pid, tty = excluded.tty, cwd = excluded.cwd,
             last_seen_at = excluded.last_seen_at",
        params![session_id, pid, tty, cwd, now.get()],
    )?;
    Ok(())
}

/// Refresh an existing row's location and liveness, never its `session_id`.
pub fn touch_location(
    conn: &Connection,
    session_id: &str,
    tty: Option<&str>,
    cwd: Option<&str>,
    now: tp_core::Millis,
) -> Result<()> {
    conn.execute(
        "UPDATE live_session SET tty = ?2, cwd = ?3, last_seen_at = ?4 WHERE session_id = ?1",
        params![session_id, tty, cwd, now.get()],
    )?;
    Ok(())
}

/// Returns rows removed, so a caller can tell "gone" from "was never there".
pub fn delete_session(conn: &Connection, session_id: &str) -> Result<usize> {
    Ok(conn.execute(
        "DELETE FROM live_session WHERE session_id = ?1",
        [session_id],
    )?)
}

/// Delete only if the row still belongs to `pid`, so a newer incarnation that
/// reused the same `session_id` is not unregistered. Returns rows removed:
/// zero is the pin doing its job, and a caller that discards it cannot tell
/// that from the row not existing.
pub fn delete_session_pinned(conn: &Connection, session_id: &str, pid: i32) -> Result<usize> {
    Ok(conn.execute(
        "DELETE FROM live_session WHERE session_id = ?1 AND pid = ?2",
        params![session_id, pid],
    )?)
}

/// Delete every `scan` row whose pid is not in `live_pids`. `declared` rows are
/// untouched: they are owned by their runtime and expire on heartbeat timeout.
/// The only statement in this module built at runtime, and only to expand the
/// placeholder list; the shape of the query never varies.
pub fn prune_scan_rows(conn: &Connection, live_pids: &[i32]) -> Result<()> {
    if live_pids.is_empty() {
        conn.execute("DELETE FROM live_session WHERE presence = 'scan'", [])?;
        return Ok(());
    }
    let placeholders = std::iter::repeat_n("?", live_pids.len())
        .collect::<Vec<_>>()
        .join(",");
    conn.execute(
        &format!(
            "DELETE FROM live_session WHERE presence = 'scan' AND pid NOT IN ({placeholders})"
        ),
        rusqlite::params_from_iter(live_pids.iter()),
    )?;
    Ok(())
}

/// Renew liveness and clear any stale mark. Returns rows touched, so a runtime
/// beating into a session floonet already evicted learns it must re-register.
pub fn touch_heartbeat(conn: &Connection, session_id: &str, now: tp_core::Millis) -> Result<usize> {
    Ok(conn.execute(
        "UPDATE live_session SET last_seen_at = ?2, stale_at = NULL WHERE session_id = ?1",
        params![session_id, now.get()],
    )?)
}

/// Stage one of declared expiry: mark rows silent since `silent_before`.
/// `registered_at` is checked too so a row gets one full TTL to send its first
/// beat before it can be marked.
pub fn mark_stale(
    conn: &Connection,
    now: tp_core::Millis,
    silent_before: tp_core::Millis,
) -> Result<usize> {
    Ok(conn.execute(
        "UPDATE live_session SET stale_at = ?1
          WHERE presence = 'declared' AND stale_at IS NULL
            AND last_seen_at  < ?2
            AND registered_at < ?2",
        params![now.get(), silent_before.get()],
    )?)
}

/// Stage two: actually delete rows marked stale before `marked_before`.
pub fn evict_stale(conn: &Connection, marked_before: tp_core::Millis) -> Result<usize> {
    Ok(conn.execute(
        "DELETE FROM live_session
          WHERE presence = 'declared' AND stale_at IS NOT NULL AND stale_at < ?1",
        [marked_before.get()],
    )?)
}

pub fn last_wake_at(conn: &Connection, session_id: &str) -> Result<Option<tp_core::Millis>> {
    Ok(conn
        .query_row(
            "SELECT last_wake_at FROM live_session WHERE session_id = ?1",
            [session_id],
            |r| r.get::<_, Option<i64>>(0),
        )
        .optional()?
        .flatten()
        .map(tp_core::Millis::new))
}

pub fn set_last_wake_at(conn: &Connection, session_id: &str, ts: tp_core::Millis) -> Result<()> {
    conn.execute(
        "UPDATE live_session SET last_wake_at = ?2 WHERE session_id = ?1",
        params![session_id, ts.get()],
    )?;
    Ok(())
}
