//! Active-scan discovery, run periodically by `fld`: find every live agent
//! process on this machine and reconcile `live_session` against exactly what
//! is found this cycle. Liveness comes from `ps` and nothing else; whether a
//! session can be woken is decided per session at wake time by `wake`.
//!
//! The scan runs alongside hook-based registration (`resolve::register`) and
//! is authoritative for liveness: it adds sessions the hooks never saw and
//! prunes rows whose `SessionEnd` never ran. Every hook-registerable runtime
//! must be recognizable here, since `reconcile` prunes any row whose pid it
//! does not find. A process with no row gets a `scan-pid-N` placeholder tagged
//! `source = 'scan'`, which never overwrites a `'hook'` row's session_id.

use crate::mailbox::now_ms;
use anyhow::Result;
use std::collections::HashMap;
use tp_db::reach;
use tp_db::DbConnection as Connection;

pub const SCAN_INTERVAL_SECS: u64 = 60;

/// A live agent process the scan found. Says nothing about reachability —
/// `tty` is where it runs, not a promise that anything can write there.
#[derive(Debug, Clone)]
pub struct ScannedProcess {
    pub pid: i32,
    pub tty: String,
    /// Floonet runtime id this process belongs to (see `recognize_runtime`).
    pub runtime: String,
    /// Best-effort, via `lsof` per matched pid; `None` if that failed or the
    /// process exited between the tty sweep and this lookup.
    pub cwd: Option<String>,
    /// Other pids of this runtime on the same tty. One pane is one session, but
    /// a session is not one process: codex runs a supervisor and spawns a
    /// child, both matching `process_match`. `pid` answers "which process
    /// represents this pane"; this answers "which processes are alive", and
    /// pruning on the first alone deletes the hook row registered on the other.
    pub siblings: Vec<i32>,
}

/// Every live agent process on this machine, whatever terminal it sits in.
///
/// Liveness is `ps` and nothing else. Intersecting with the ttys tmux or
/// iTerm2 report would make an unintegrated terminal indistinguishable from a
/// dead process, and `reconcile` prunes on this answer. Wakeability is looked
/// up by tty at wake time (`wake::terminal_write_text`,
/// `resolve::terminal_owns_tty`), so an unreachable session stays visible and
/// fails at wake time instead of ceasing to exist.
pub fn scan_all(sigs: &[ProcessSignature]) -> Vec<ScannedProcess> {
    let Ok(out) = std::process::Command::new("ps")
        .args(["-eo", "pid=,tty=,comm="])
        .output()
    else {
        return Vec::new();
    };
    let found = agents_from_ps(&String::from_utf8_lossy(&out.stdout), sigs);
    found
        .into_iter()
        .map(|(tty, pid, runtime, siblings)| ScannedProcess {
            cwd: cwd_of_pid(pid),
            pid,
            tty,
            runtime,
            siblings,
        })
        .collect()
}

/// The pure half of `scan_all`: `ps` output in, recognized agents out. Split
/// out so the selection rule is testable without spawning anything. Keyed by
/// normalized tty (`ttys007`, no `/dev/` prefix), the form `wake` and
/// `resolve` match against.
fn agents_from_ps(
    ps_output: &str,
    sigs: &[ProcessSignature],
) -> Vec<(String, i32, String, Vec<i32>)> {
    // One entry per pane, plus every pid that fed it. Last writer wins for
    // which process represents the pane; the overwritten pids are kept because
    // downstream treats "not in this list" as "exited".
    let mut seen: HashMap<String, (i32, String, Vec<i32>)> = HashMap::new();
    for line in ps_output.lines() {
        let mut parts = line.split_whitespace();
        let (Some(pid), Some(tty), Some(comm)) = (parts.next(), parts.next(), parts.next()) else {
            continue;
        };
        let Some(runtime) = recognize_runtime(comm, sigs) else {
            continue;
        };
        // No controlling terminal: nothing `wake` could address. A tty-less
        // harness registers through the `exec:`/loopback path instead
        // (`resolve::register_with`), which the scan is not authoritative for.
        if tty == "??" || tty == "?" {
            continue;
        }
        let Ok(pid) = pid.parse::<i32>() else {
            continue;
        };
        let key = tty.trim_start_matches("/dev/").to_string();
        let entry = seen
            .entry(key)
            .or_insert_with(|| (pid, runtime.to_string(), Vec::new()));
        // The previous representative becomes a sibling rather than vanishing.
        if entry.0 != pid {
            let prev = entry.0;
            entry.2.push(prev);
            entry.0 = pid;
            entry.1 = runtime.to_string();
        }
    }
    seen.into_iter()
        .map(|(tty, (pid, runtime, siblings))| (tty, pid, runtime, siblings))
        .collect()
}

/// How a scannable harness is recognized in `ps` output. Supplied by the
/// caller from each harness descriptor's `capabilities.process_match` rather
/// than hardcoded, so adding a scannable runtime is not a Rust change. A
/// harness that registers itself needs no signature; this is only how one is
/// found without cooperation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessSignature {
    pub runtime_id: String,
    /// `comm` pattern. A leading `=` anchors an exact match; otherwise it is a
    /// case-insensitive substring. `claude` must be a substring (a dev build
    /// named `claude-local` must match); `pi` must be exact (a substring would
    /// hit `pip` and `gpio-tool`).
    pub pattern: String,
}

impl ProcessSignature {
    fn matches(&self, comm: &str) -> bool {
        let comm = comm.to_lowercase();
        match self.pattern.strip_prefix('=') {
            Some(exact) => comm == exact.to_lowercase(),
            None => comm.contains(&self.pattern.to_lowercase()),
        }
    }
}

/// Map a process's `comm` to a runtime id using the supplied signatures.
/// First match wins, so caller order is the precedence.
fn recognize_runtime<'a>(comm: &str, sigs: &'a [ProcessSignature]) -> Option<&'a str> {
    sigs.iter()
        .find(|s| s.matches(comm))
        .map(|s| s.runtime_id.as_str())
}

/// `lsof -a -d cwd -p <pid> -Fn` → the `n`-prefixed line is the path. macOS
/// has no `/proc`, so this is the standard way to get another process's cwd.
fn cwd_of_pid(pid: i32) -> Option<String> {
    let out = std::process::Command::new("lsof")
        .args(["-a", "-d", "cwd", "-p", &pid.to_string(), "-Fn"])
        .output()
        .ok()?;
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .find_map(|l| l.strip_prefix('n'))
        .map(str::to_string)
}

/// What an empty scan result is allowed to mean. `scan_all` degrades to an
/// empty list when `ps` fails, so an empty result is two facts in one shape:
/// "nothing is running" and "I could not look". Reading it as the first would
/// delete every scan-presence row on the machine for a transient failure and
/// rebuild them a cycle later with `source` downgraded from `hook` to `scan`,
/// since only a hook firing writes that column back.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EmptyScan {
    /// The caller has established that finding nothing means nothing is there.
    /// `fld` requires two consecutive empty cycles before claiming this, the
    /// same grace-then-act shape `sweep_declared` uses: a transient failure
    /// must cost a delay, never state.
    Authoritative,
    /// Finding nothing may just mean the scan could not see. Refresh what was
    /// found and prune nothing.
    Unverified,
}

/// Reconcile `live_session` against exactly what `scan_all()` found this
/// cycle: refresh or create a row for every process found, and delete any
/// scan-presence row, hook- or scan-sourced, whose pid is not among them. The
/// scan is authoritative for liveness: a hook-registered row whose process is
/// gone (crashed, killed, slept through its SessionEnd) is exactly as stale as
/// an unmatched scan row.
pub fn reconcile(
    conn: &Connection,
    machine_id: &str,
    found: &[ScannedProcess],
    empty: EmptyScan,
) -> Result<()> {
    let now = now_ms();
    let mut live_pids = Vec::with_capacity(found.len());

    for p in found {
        live_pids.push(p.pid);
        // Siblings are alive too; pruning without them deletes a registration
        // the scan simply did not choose to represent the pane.
        live_pids.extend(p.siblings.iter().copied());
        // `session_id` is the primary key, not `pid`: a pid owns as many rows
        // as it has sessions. Claude Code fires SessionStart for a pane's
        // conversation and its internal queue sessions in one process, so two
        // `hook` rows on one pid is a correct state and a hook row is never
        // deleted here. Only `scan-pid-N` placeholders are, and only once
        // something real has claimed the pid. `presence = 'scan'` only: a
        // declared row may legitimately share a pid with many others (one dsh
        // host process serves many sessions).
        let mut existing = reach::scan_rows_for_pid(conn, p.pid)?;
        let any_hook = existing.iter().any(|r| r.source == "hook");
        if any_hook {
            for row in existing.iter().filter(|r| r.source != "hook") {
                reach::delete_session(conn, &row.session_id)?;
            }
            existing.retain(|r| r.source == "hook");
        } else {
            // Only placeholders. Keep the newest and drop the rest — two
            // scan-minted ids for one pid are the same process seen twice.
            existing.sort_by_key(|r| std::cmp::Reverse(r.registered_at));
            for dupe in existing.iter().skip(1) {
                reach::delete_session(conn, &dupe.session_id)?;
            }
            existing.truncate(1);
        }

        // One pane, one session, even when it is two processes: codex runs a
        // supervisor and the TUI it spawns, both matching `process_match`, so
        // the scan finds the child while the hook's ancestor walk registered
        // the parent. Keyed on pid these can never merge, so the pane is
        // matched by tty. The mint is skipped rather than the row adopted: the
        // hook row's pid is the parent, which the scan also visits and
        // refreshes, and rewriting its pid would point a real registration at
        // whichever process the scan happened to enumerate first.
        if reach::hook_row_on_tty(conn, &p.runtime, &p.tty)? {
            // Also remove a placeholder this pane collected before the hook
            // row existed; left alone it is touched forever.
            for row in existing.iter().filter(|r| r.source != "hook") {
                reach::delete_session(conn, &row.session_id)?;
            }
            existing.retain(|r| r.source == "hook");
            if existing.is_empty() {
                continue;
            }
        }

        if existing.is_empty() {
            let sid = infer_session_id(machine_id, p)?;
            reach::insert_scanned(conn, &sid, p.pid, Some(&p.tty), p.cwd.as_deref(), now)?;
            bind_scanned_to_conversation(conn, machine_id, &sid, p, now);
        } else {
            // Every row on this pid: they are all sessions of one live process,
            // and the sweep below prunes on `last_seen_at`. Liveness and
            // location only; `session_id` is never reassigned, because that
            // would overwrite a real, hook-provided id.
            for row in &existing {
                reach::touch_location(conn, &row.session_id, Some(&p.tty), p.cwd.as_deref(), now)?;
                // A row the scan is merely touching may still predate its
                // process's conversation: live and wakeable, drained by nobody.
                bind_scanned_to_conversation(conn, machine_id, &row.session_id, p, now);
            }
        }

        // Sibling pids belong to the same pane and also hold rows; the loop
        // above visits only representatives, so without this a sibling's
        // `last_seen_at` freezes and its location is never corrected, and
        // `hook_row_on_tty` only recognises a pane when the row names a tty.
        // Location only: `ConversationKey` includes the pid, and binding a
        // sibling's row under the representative's pid would file it in a
        // conversation that does not describe it.
        for sibling in &p.siblings {
            for row in reach::scan_rows_for_pid(conn, *sibling)? {
                reach::touch_location(conn, &row.session_id, Some(&p.tty), p.cwd.as_deref(), now)?;
            }
        }
    }

    // The scan's delete authority is scoped to rows that declared themselves
    // scannable: the scan is authoritative only for sessions it said it could
    // see. A `declared` row is owned by its runtime and expires on a heartbeat
    // timeout instead — pruning it here would delete a correct registration the
    // scan was never able to observe.
    if live_pids.is_empty() && empty == EmptyScan::Unverified {
        return Ok(());
    }
    reach::prune_scan_rows(conn, &live_pids)
}

/// Attach a scanned row to whatever conversation already owns its process.
///
/// Never fatal: discovery's job is to keep `live_session` true, and a session
/// that is reachable but not yet grouped is strictly better than one the scan
/// dropped over a bookkeeping error.
fn bind_scanned_to_conversation(
    conn: &Connection,
    machine_id: &str,
    session_id: &str,
    p: &ScannedProcess,
    now: tp_core::Millis,
) {
    let start = crate::resolve::process_start(p.pid);
    let key = reach::ConversationKey {
        machine_id,
        runtime_id: &p.runtime,
        pid: p.pid,
        pid_start: start.as_deref(),
        cwd: p.cwd.as_deref(),
    };
    if let Err(e) = reach::join_existing_conversation(conn, session_id, key, now) {
        tp_core::log_warn!("scan could not bind {session_id} to a conversation: {e:#}");
    }
}

/// A tracking id for a scanned process whose session floonet does not know.
///
/// The scan does not guess a session id from the cwd: two panes in one folder
/// would share an address, and a folder with subagent traffic would answer
/// with a subagent's transcript id, which belongs to no pane. A message to
/// either is accepted and never delivered, which is worse than an id that is
/// visibly not a session. The pane's own SessionStart hook is the only thing
/// that knows its conversation, and `reconcile` prefers a hook row over a scan
/// row on the same pid.
fn infer_session_id(machine_id: &str, p: &ScannedProcess) -> Result<String> {
    Ok(format!("{machine_id}/{}/scan-pid-{}", p.runtime, p.pid))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tp_db::Db;

    fn setup() -> Db {
        let db = Db::open_in_memory().unwrap();
        db.ensure_self_machine("m1", "TestMac").unwrap();
        db.ensure_runtime("claude_code", "/root").unwrap();
        db
    }

    fn scanned(pid: i32, tty: &str, runtime: &str, cwd: Option<&str>) -> ScannedProcess {
        ScannedProcess {
            pid,
            tty: tty.to_string(),
            runtime: runtime.to_string(),
            cwd: cwd.map(str::to_string),
            siblings: Vec::new(),
        }
    }

    fn sigs() -> Vec<ProcessSignature> {
        vec![
            ProcessSignature {
                runtime_id: "claude_code".into(),
                pattern: "claude".into(),
            },
            ProcessSignature {
                runtime_id: "pi".into(),
                pattern: "=pi".into(),
            },
        ]
    }

    /// Liveness must come from `ps` alone: a terminal floonet cannot inject
    /// into is a session it cannot wake, never a session that is not running.
    /// `reconcile` deletes any row the scan does not return, so anything
    /// narrower prunes every agent in an unintegrated terminal.
    #[test]
    fn an_agent_in_an_unintegrated_terminal_is_still_alive() {
        // ttys013 is a tty no integrated terminal would report.
        let ps = "77810 ttys013  pi\n98275 ttys002  claude\n";
        let found = agents_from_ps(ps, &sigs());

        let ttys: Vec<&str> = found.iter().map(|(t, _, _, _)| t.as_str()).collect();
        assert!(
            ttys.contains(&"ttys013"),
            "an agent in a terminal floonet has no integration for must still \
             count as alive — it is unwakeable, not dead: {found:?}"
        );
        assert!(ttys.contains(&"ttys002"), "{found:?}");
        assert_eq!(found.len(), 2);
    }

    /// A process with no controlling terminal has nothing `wake` could ever
    /// address, and registers through the `exec:`/loopback path instead.
    #[test]
    fn a_process_with_no_tty_is_not_scanned() {
        let ps = "999 ??  claude\n1000 ?  pi\n77810 ttys013  pi\n";
        let found = agents_from_ps(ps, &sigs());
        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!(found[0].0, "ttys013");
    }

    #[test]
    fn unrecognized_processes_are_ignored_and_dev_ttys_are_normalized() {
        let ps = "1 ttys001  bash\n2 /dev/ttys004  claude\n";
        let found = agents_from_ps(ps, &sigs());
        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!(found[0].0, "ttys004", "the /dev/ prefix must be stripped");
        assert_eq!(found[0].2, "claude_code");
    }

    #[test]
    fn recognize_runtime_matches_dev_builds_by_substring_but_pi_by_exact_name() {
        // The patterns come from descriptors; this asserts the mechanism
        // honours both modes. The shipped patterns are asserted in tp-ingest.
        let sigs = vec![
            ProcessSignature {
                runtime_id: "claude_code".into(),
                pattern: "claude".into(), // substring
            },
            ProcessSignature {
                runtime_id: "pi".into(),
                pattern: "=pi".into(), // exact
            },
        ];
        let r = |comm: &str| recognize_runtime(comm, &sigs);

        // Substring: a dev build named `claude-local` must still match.
        assert_eq!(r("claude"), Some("claude_code"));
        assert_eq!(r("claude-local"), Some("claude_code"));
        assert_eq!(r("Claude"), Some("claude_code"));

        assert_eq!(r("pi"), Some("pi"));
        // "pi" is exact: too short to substring-match without false positives.
        assert_eq!(r("pip"), None);
        assert_eq!(r("gpio-tool"), None);
        assert_eq!(
            r("node"),
            None,
            "the interpreter alone must not match — comm is agent-named specifically"
        );
    }

    /// A harness that declares no signature is not discoverable without
    /// cooperation; it must not match everything, nor panic.
    #[test]
    fn an_empty_signature_table_recognizes_nothing() {
        assert_eq!(recognize_runtime("claude", &[]), None);
        assert_eq!(recognize_runtime("anything", &[]), None);
    }

    /// A row on a sibling pid must be refreshed, not merely spared: the
    /// reconcile loop visits representatives, so a sibling's row otherwise
    /// freezes. Location is what matters: `hook_row_on_tty` only recognises a
    /// pane when the row names a tty, so a tty-less hook row never suppresses
    /// the placeholder it should.
    #[test]
    fn a_row_on_a_sibling_pid_is_refreshed_and_gains_the_panes_tty() {
        let db = setup();
        db.ensure_runtime("codex", "/root").unwrap();
        // The hook registered the parent, with no tty.
        tp_db::reach::upsert_registration(
            db.conn(),
            "m1/codex/real-session",
            54123,
            None,
            None,
            "scan",
            None,
            Some("codex"),
            tp_core::Millis::new(1_000),
        )
        .unwrap();

        // Both processes on one tty: the child represents the pane, the parent
        // becomes its sibling.
        reconcile(
            db.conn(),
            "m1",
            &[
                scanned(54123, "ttys005", "codex", Some("/work")),
                scanned(56535, "ttys005", "codex", Some("/work")),
            ],
            EmptyScan::Authoritative,
        )
        .unwrap();

        let (tty, seen): (Option<String>, i64) = db
            .conn()
            .query_row(
                "SELECT tty, last_seen_at FROM live_session WHERE session_id = ?1",
                ["m1/codex/real-session"],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(
            tty.as_deref(),
            Some("ttys005"),
            "the sibling's row must learn the pane it is actually in — without \
             this, `hook_row_on_tty` never recognises it and the placeholder it \
             should suppress lives forever"
        );
        assert!(
            seen > 1_000,
            "a row on a live sibling must not have a frozen last_seen_at"
        );

        // Knowing the tty, the pane is claimed, so a second run adds no placeholder.
        reconcile(
            db.conn(),
            "m1",
            &[
                scanned(54123, "ttys005", "codex", Some("/work")),
                scanned(56535, "ttys005", "codex", Some("/work")),
            ],
            EmptyScan::Authoritative,
        )
        .unwrap();
        let n: i64 = db
            .conn()
            .query_row("SELECT COUNT(*) FROM live_session", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 1, "one window is one row");
    }

    /// `ps` shows two codex processes on one tty and the pane map keeps one.
    /// The pid it drops must still count as alive: the hook's ancestor walk
    /// registers the supervisor, the tty map keeps the child, and pruning on
    /// the child alone deletes the real registration.
    #[test]
    fn a_sibling_process_on_the_same_tty_counts_as_alive() {
        let ps = "\
54123 ttys005 /path/to/bin/codex
56535 ttys005 /path/to/bin/codex-code-mode-host
";
        let sigs = vec![ProcessSignature {
            runtime_id: "codex".into(),
            pattern: "codex".into(),
        }];
        let found = agents_from_ps(ps, &sigs);
        assert_eq!(found.len(), 1, "one pane, one entry");
        let (_tty, pid, _rt, siblings) = &found[0];
        let mut alive = vec![*pid];
        alive.extend(siblings.iter().copied());
        alive.sort_unstable();
        assert_eq!(
            alive,
            vec![54123, 56535],
            "both processes are running; whichever one does not represent the \
             pane is still not dead, and treating it as dead deletes the \
             registration that lives on it"
        );
    }

    /// One pane is one session, even when the runtime is two processes. codex
    /// runs a supervisor and the TUI it spawns; both match `process_match`, so
    /// the scan enumerates the child while the hook registered the parent.
    /// Dedupe keyed on pid can never merge them, and a second placeholder is
    /// an address that is deliverable but unreadable.
    #[test]
    fn a_pane_already_held_by_a_hook_gets_no_second_placeholder() {
        let db = setup();
        db.ensure_runtime("codex", "/root").unwrap();
        // The hook registered the parent, which is what the ancestor walk finds.
        tp_db::reach::upsert_registration(
            db.conn(),
            "m1/codex/real-session-id",
            54123,
            Some("ttys005"),
            Some("/work"),
            "scan",
            None,
            Some("codex"),
            tp_core::Millis::new(1_000),
        )
        .unwrap();

        // The scan finds both. Passing only the child would also prune the
        // hook row for having no live process, a different bug.
        reconcile(
            db.conn(),
            "m1",
            &[
                scanned(54123, "ttys005", "codex", Some("/work")),
                scanned(56535, "ttys005", "codex", Some("/work")),
            ],
            EmptyScan::Authoritative,
        )
        .unwrap();

        let ids: Vec<String> = db
            .conn()
            .prepare("SELECT session_id FROM live_session ORDER BY session_id")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert_eq!(
            ids,
            vec!["m1/codex/real-session-id".to_string()],
            "the pane already had a real registration; a second, unreadable \
             placeholder for the same pane is what a sender copies by mistake"
        );

        // A different pane of the same runtime must still be discovered;
        // suppressing by runtime alone would hide every codex session after the first.
        reconcile(
            db.conn(),
            "m1",
            &[
                scanned(54123, "ttys005", "codex", Some("/work")),
                scanned(56535, "ttys005", "codex", Some("/work")),
                scanned(77777, "ttys009", "codex", Some("/other")),
            ],
            EmptyScan::Authoritative,
        )
        .unwrap();
        let n: i64 = db
            .conn()
            .query_row("SELECT COUNT(*) FROM live_session", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 2, "a second pane is a second session");
    }

    /// A single empty scan must not be believed: `scan_all` degrades to an
    /// empty list when `ps` fails, and pruning on it would unregister every
    /// scan-presence session on the machine, hook-registered ones included,
    /// with their provenance downgraded to `scan` when they come back.
    #[test]
    fn an_unverified_empty_scan_prunes_nothing() {
        let db = setup();
        reconcile(
            db.conn(),
            "m1",
            &[scanned(4242, "ttys001", "claude_code", None)],
            EmptyScan::Authoritative,
        )
        .unwrap();
        let before: i64 = db
            .conn()
            .query_row("SELECT COUNT(*) FROM live_session", [], |r| r.get(0))
            .unwrap();
        assert_eq!(before, 1);

        // The scan came back empty because it could not look, not because the
        // machine went idle. Nothing may be deleted on that basis.
        reconcile(db.conn(), "m1", &[], EmptyScan::Unverified).unwrap();
        let after: i64 = db
            .conn()
            .query_row("SELECT COUNT(*) FROM live_session", [], |r| r.get(0))
            .unwrap();
        assert_eq!(
            after, 1,
            "one empty scan is indistinguishable from a failed one — pruning on it \
             unregisters every live session on the machine"
        );

        // Confirmed empty on a second consecutive cycle: now it is a fact.
        reconcile(db.conn(), "m1", &[], EmptyScan::Authoritative).unwrap();
        let confirmed: i64 = db
            .conn()
            .query_row("SELECT COUNT(*) FROM live_session", [], |r| r.get(0))
            .unwrap();
        assert_eq!(
            confirmed, 0,
            "a CONFIRMED empty scan must still prune — the guard is a delay, not an exemption"
        );
    }

    #[test]
    fn scan_only_row_is_pruned_when_no_longer_found() {
        let db = setup();
        let found = vec![scanned(111, "ttys999", "claude_code", None)];
        reconcile(db.conn(), "m1", &found, EmptyScan::Authoritative).unwrap();
        let count: i64 = db
            .conn()
            .query_row("SELECT COUNT(*) FROM live_session", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 1, "a first-seen process must be tracked");

        // Next cycle finds nothing — the scan is authoritative, so it prunes.
        reconcile(db.conn(), "m1", &[], EmptyScan::Authoritative).unwrap();
        let count: i64 = db
            .conn()
            .query_row("SELECT COUNT(*) FROM live_session", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 0, "a process no longer found must be pruned, even though it was never hook-registered or explicitly unregistered");
    }

    #[test]
    fn hook_registered_row_is_pruned_when_the_process_disappears() {
        let db = setup();
        crate::resolve::register(
            db.conn(),
            "m1/claude_code/real-sess",
            222,
            Some("/dev/ttys998"),
            None,
        )
        .unwrap();

        // Scan doesn't see pid 222 this cycle (process crashed, SessionEnd never fired).
        reconcile(db.conn(), "m1", &[], EmptyScan::Authoritative).unwrap();
        let count: i64 = db
            .conn()
            .query_row("SELECT COUNT(*) FROM live_session", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 0, "the scan must prune a hook-registered row too once the process is gone — it's authoritative for liveness, not just for scan-sourced rows");
    }

    /// A harness the scan cannot see by construction (dsh's web profile:
    /// sessions live in a browser, no tty, one host process serves many) has
    /// no signature to add, so the scan must not claim authority it lacks.
    #[test]
    fn a_declared_session_survives_a_scan_that_cannot_see_it() {
        let db = setup();
        crate::resolve::register_with(
            db.conn(),
            "m1/dsh/session-abc",
            4242,
            None, // no tty — this is the whole point
            Some("/w"),
            crate::resolve::Presence::Declared,
            Some("http://127.0.0.1:8125/floonet/wake"),
        )
        .unwrap();

        // A scan cycle that finds something else entirely, and never this pid.
        let found = vec![scanned(999, "ttys000", "claude_code", None)];
        reconcile(db.conn(), "m1", &found, EmptyScan::Authoritative).unwrap();

        let survived: i64 = db
            .conn()
            .query_row(
                "SELECT count(*) FROM live_session WHERE session_id = ?1",
                ["m1/dsh/session-abc"],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            survived, 1,
            "a declared session must not be pruned by a scan that cannot observe it"
        );
    }

    /// A multiplexed host registers many sessions on one pid; a one-row-per-pid
    /// rule would collapse N registrations into 1 here.
    #[test]
    fn declared_sessions_may_share_a_pid() {
        let db = setup();
        for id in ["m1/dsh/s1", "m1/dsh/s2", "m1/dsh/s3"] {
            crate::resolve::register_with(
                db.conn(),
                id,
                7000, // same host process for all three
                None,
                Some("/w"),
                crate::resolve::Presence::Declared,
                None,
            )
            .unwrap();
        }

        reconcile(
            db.conn(),
            "m1",
            &[scanned(7000, "ttys001", "claude_code", None)],
            EmptyScan::Authoritative,
        )
        .unwrap();

        let n: i64 = db
            .conn()
            .query_row(
                "SELECT count(*) FROM live_session WHERE runtime_id = 'dsh'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n, 3, "one host process may own many declared sessions");
    }

    #[test]
    fn a_hook_registered_pi_session_survives_a_scan_cycle() {
        // A hook-registered runtime the scan cannot recognize is pruned within
        // one interval regardless of how its hooks behave.
        let db = setup();
        crate::resolve::register(
            db.conn(),
            "m1/pi/real-pi-sess",
            999,
            Some("/dev/ttys000"),
            None,
        )
        .unwrap();

        let found = vec![scanned(999, "ttys000", "pi", None)];
        reconcile(db.conn(), "m1", &found, EmptyScan::Authoritative).unwrap();

        let count: i64 = db
            .conn()
            .query_row("SELECT COUNT(*) FROM live_session", [], |r| r.get(0))
            .unwrap();
        assert_eq!(
            count, 1,
            "a hook-registered pi session found by the scan must survive"
        );
    }

    #[test]
    fn stale_scan_placeholder_is_cleaned_up_once_the_real_hook_row_appears() {
        // A process can be scan-discovered before its own hook fires, leaving
        // a `scan-pid-N` placeholder; the hook then inserts a separate row
        // under the real id (different primary key, same pid). The next
        // reconcile cycle must collapse the two to the real row.
        let db = setup();
        // Cycle 1: scan discovers the process first, no hook row exists yet.
        let found = vec![scanned(777, "ttys111", "pi", None)];
        reconcile(db.conn(), "m1", &found, EmptyScan::Authoritative).unwrap();
        let placeholder: String = db
            .conn()
            .query_row(
                "SELECT session_id FROM live_session WHERE pid = 777",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(placeholder, "m1/pi/scan-pid-777");

        // The hook now fires for the same pid (its own insert, separate PK).
        crate::resolve::register(
            db.conn(),
            "m1/pi/real-sess-from-hook",
            777,
            Some("/dev/ttys111"),
            None,
        )
        .unwrap();
        let count_before: i64 = db
            .conn()
            .query_row(
                "SELECT COUNT(*) FROM live_session WHERE pid = 777",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            count_before, 2,
            "sanity check: both rows must coexist immediately after the hook's separate INSERT"
        );

        // Cycle 2: the scan runs again and must dedup down to the real row.
        reconcile(db.conn(), "m1", &found, EmptyScan::Authoritative).unwrap();
        let rows: Vec<(String, String)> = db
            .conn()
            .prepare("SELECT session_id, source FROM live_session WHERE pid = 777")
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert_eq!(
            rows,
            vec![("m1/pi/real-sess-from-hook".to_string(), "hook".to_string())],
            "the scan placeholder must be deleted, leaving only the real hook row"
        );
    }

    #[test]
    fn scan_never_overwrites_a_hook_provided_session_id() {
        let db = setup();
        crate::resolve::register(
            db.conn(),
            "m1/claude_code/real-sess",
            333,
            Some("/dev/ttys997"),
            None,
        )
        .unwrap();

        // Same pid shows up in a scan cycle (this IS the hook-registered process).
        let found = vec![scanned(333, "ttys997", "claude_code", Some("/some/dir"))];
        reconcile(db.conn(), "m1", &found, EmptyScan::Authoritative).unwrap();

        let (sid, source): (String, String) = db
            .conn()
            .query_row(
                "SELECT session_id, source FROM live_session WHERE pid = 333",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(
            sid, "m1/claude_code/real-sess",
            "the real hook-provided session_id must survive a scan cycle unchanged"
        );
        assert_eq!(
            source, "hook",
            "source must stay 'hook', not get relabeled 'scan'"
        );
    }

    #[test]
    fn unmatched_cwd_falls_back_to_a_synthetic_but_stable_id_per_runtime() {
        let db = setup();
        let found = vec![scanned(
            444,
            "ttys996",
            "claude_code",
            Some("/no/such/known/project"),
        )];
        reconcile(db.conn(), "m1", &found, EmptyScan::Authoritative).unwrap();
        let sid: String = db
            .conn()
            .query_row(
                "SELECT session_id FROM live_session WHERE pid = 444",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(sid, "m1/claude_code/scan-pid-444");
    }

    #[test]
    fn unmatched_pi_process_gets_its_own_runtime_in_the_synthetic_id() {
        let db = setup();
        let found = vec![scanned(446, "ttys994", "pi", None)];
        reconcile(db.conn(), "m1", &found, EmptyScan::Authoritative).unwrap();
        let sid: String = db
            .conn()
            .query_row(
                "SELECT session_id FROM live_session WHERE pid = 446",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            sid, "m1/pi/scan-pid-446",
            "a pi process must never be composed under the claude_code runtime"
        );
    }

    /// Two real sessions in one process must both survive a scan cycle. Claude
    /// Code runs more than one session per process (a pane's conversation
    /// alongside its internal queue sessions), each firing SessionStart, so
    /// two `hook` rows on one pid is a correct state; keeping one row per pid
    /// would unregister the pane when a sibling session starts.
    #[test]
    fn two_hook_sessions_on_one_pid_both_survive() {
        let db = setup();
        let conn = db.conn();
        for (sid, at) in [
            ("m1/claude_code/pane", 1_000),
            ("m1/claude_code/queue", 2_000),
        ] {
            tp_db::reach::upsert_registration(
                conn,
                sid,
                777,
                Some("ttys900"),
                Some("/w"),
                "scan",
                None,
                Some("claude_code"),
                tp_core::Millis::new(at),
            )
            .unwrap();
        }
        let found = vec![scanned(777, "ttys900", "claude_code", Some("/w"))];
        reconcile(conn, "m1", &found, EmptyScan::Authoritative).unwrap();

        let mut live: Vec<String> = conn
            .prepare("SELECT session_id FROM live_session WHERE pid = 777")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .map(Result::unwrap)
            .collect();
        live.sort();
        assert_eq!(
            live,
            vec!["m1/claude_code/pane", "m1/claude_code/queue"],
            "a later sibling registration must not unregister the earlier one"
        );
    }

    /// A scan placeholder gives way once something real claims the pid.
    #[test]
    fn a_scan_placeholder_is_dropped_when_a_hook_row_exists() {
        let db = setup();
        let conn = db.conn();
        tp_db::reach::insert_scanned(
            conn,
            "m1/claude_code/scan-pid-778",
            778,
            Some("ttys901"),
            Some("/w"),
            tp_core::Millis::new(1_000),
        )
        .unwrap();
        tp_db::reach::upsert_registration(
            conn,
            "m1/claude_code/real",
            778,
            Some("ttys901"),
            Some("/w"),
            "scan",
            None,
            Some("claude_code"),
            tp_core::Millis::new(2_000),
        )
        .unwrap();
        let found = vec![scanned(778, "ttys901", "claude_code", Some("/w"))];
        reconcile(conn, "m1", &found, EmptyScan::Authoritative).unwrap();

        let live: Vec<String> = conn
            .prepare("SELECT session_id FROM live_session WHERE pid = 778")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .map(Result::unwrap)
            .collect();
        assert_eq!(live, vec!["m1/claude_code/real"]);
    }

    #[test]
    fn a_scanned_process_is_not_given_a_session_id_it_cannot_verify() {
        let db = setup();
        db.conn()
            .execute(
                "INSERT INTO session(id, machine_id, runtime_id, native_id, cwd, last_turn_at) VALUES (?1, 'm1', 'claude_code', 'native-abc', '/Users/me/proj', 1000)",
                ["m1/claude_code/native-abc"],
            )
            .unwrap();
        let found = vec![scanned(
            555,
            "ttys995",
            "claude_code",
            Some("/Users/me/proj"),
        )];
        reconcile(db.conn(), "m1", &found, EmptyScan::Authoritative).unwrap();
        let sid: String = db
            .conn()
            .query_row(
                "SELECT session_id FROM live_session WHERE pid = 555",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            sid, "m1/claude_code/scan-pid-555",
            "a same-folder session must not be adopted as this process's identity"
        );
    }

    #[test]
    fn cwd_match_is_scoped_to_the_same_runtime() {
        // A pi process sharing a cwd with an indexed Claude Code session must
        // not be mistaken for that Claude Code session.
        let db = setup();
        db.conn()
            .execute(
                "INSERT INTO session(id, machine_id, runtime_id, native_id, cwd, last_turn_at) VALUES (?1, 'm1', 'claude_code', 'native-abc', '/shared/dir', 1000)",
                ["m1/claude_code/native-abc"],
            )
            .unwrap();
        let found = vec![scanned(666, "ttys993", "pi", Some("/shared/dir"))];
        reconcile(db.conn(), "m1", &found, EmptyScan::Authoritative).unwrap();
        let sid: String = db
            .conn()
            .query_row(
                "SELECT session_id FROM live_session WHERE pid = 666",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            sid, "m1/pi/scan-pid-666",
            "must not cross-match a claude_code session just because the cwd matches"
        );
    }
}
