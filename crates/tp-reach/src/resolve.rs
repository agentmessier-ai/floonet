//! Live-session resolution: `session_id → pid → tty → pane`.
//!
//! The cached `live_session.tty` is a hint only: the pid is re-checked for
//! liveness and the tty re-derived at wake time, so a stale cache can never
//! land a message in the wrong terminal. Pane ids are never cached (tmux
//! resurrect/renumber drifts them).

use crate::mailbox;
use crate::terminal;
use anyhow::{bail, Result};
use tp_db::reach;
use tp_db::DbConnection as Connection;

/// A delivery channel a runtime declared for itself. Two forms, deliberately
/// only two: `Exec` is the primitive every harness can satisfy (no socket, no
/// port, no auth); `Http` exists because a harness that already runs a server
/// can serve it for free, where spawning a process per wake would be wasteful.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeliveryChannel {
    /// Spawn this argv. The control string arrives on stdin.
    Exec(Vec<String>),
    /// POST to this loopback URL.
    Http(String),
}

impl DeliveryChannel {
    /// Parse a stored `deliver` value. An `http:` channel must be loopback,
    /// checked here rather than trusted to the descriptor: a channel makes an
    /// agent act, and a routable URL would let anything on the network poke a
    /// session. Rejecting is lossless, since cross-machine reach dispatches
    /// through the peer's own daemon.
    pub fn parse(raw: &str) -> Result<Self> {
        if let Some(argv) = raw.strip_prefix("exec:") {
            let parts: Vec<String> = argv.split_whitespace().map(str::to_string).collect();
            if parts.is_empty() {
                bail!("empty exec: channel");
            }
            return Ok(DeliveryChannel::Exec(parts));
        }
        if raw.starts_with("http://") || raw.starts_with("https://") {
            let host = raw
                .split("://")
                .nth(1)
                .and_then(|rest| rest.split(['/', '?', '#']).next())
                .map(authority_host)
                .unwrap_or_default();
            if !matches!(host, "127.0.0.1" | "localhost" | "[::1]" | "::1") {
                bail!("delivery channel must be loopback, got {host:?} in {raw:?}");
            }
            return Ok(DeliveryChannel::Http(raw.to_string()));
        }
        bail!(
            "unrecognized delivery channel {raw:?} (want `exec:<argv>` or a loopback http:// URL)"
        )
    }
}

/// The host of an authority (`userinfo@host:port`), for the loopback check.
/// A rightmost-colon split would read an IPv6 literal's own colons as a port
/// and would take a userinfo part for the host, either of which decides
/// loopback on a string that is not the host.
fn authority_host(authority: &str) -> &str {
    let hostport = authority.rsplit_once('@').map_or(authority, |(_, h)| h);
    if hostport.starts_with('[') {
        // A bracketed literal is kept whole: the brackets are part of the
        // form the accepted set is written in.
        return match hostport.find(']') {
            Some(end) => &hostport[..=end],
            None => hostport,
        };
    }
    hostport.rsplit_once(':').map_or(hostport, |(h, _)| h)
}

#[derive(Debug, Clone, PartialEq)]
pub enum Target {
    /// A tmux pane id, e.g. `%3`. No TCC required; safe from any process
    /// including `fld`.
    Tmux(String),
    /// A terminal floonet can type into, identified by its tty and by which
    /// descriptor claimed it (`terminal::TerminalConfig::id`); AppleScript
    /// re-matches at wake time, same as tmux. The id is carried rather than
    /// re-derived because resolving asks every descriptor in turn, and asking
    /// again at wake time could answer differently. Requires the calling
    /// process to hold the Automation TCC grant (see `Caller` in `wake.rs`):
    /// safe from a CLI inside a terminal, never from `fld` (a bare LaunchAgent).
    Terminal { id: String, tty: String },
    /// A channel the runtime declared for itself. floonet does not know what is
    /// on the other end and does not need to: it delivers the same fixed control
    /// string a pane would receive, and the runtime drains its own inbox.
    Channel(DeliveryChannel),
    /// A bare tty no backend can inject into; mailbox-only.
    Unreachable,
    /// Not registered or not alive.
    NotLive,
}

/// Register (or refresh) a live session binding: the SessionStart hook path.
/// Always marks the row `source = 'hook'`; a real session_id from the runtime
/// outranks anything the active scan (`discover.rs`) inferred for the same id.
pub fn register(
    conn: &Connection,
    session_id: &str,
    pid: i32,
    tty: Option<&str>,
    cwd: Option<&str>,
) -> Result<()> {
    register_with(conn, session_id, pid, tty, cwd, Presence::Scan, None)
}

/// Which liveness regime governs a session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Presence {
    /// The process scan is authoritative: it may create and prune this row.
    Scan,
    /// The runtime owns its own liveness and renews it by heartbeat. The scan
    /// must neither create nor prune the row; it expires on timeout instead.
    /// For a harness the scan cannot see: a web GUI with no tty, a host
    /// multiplexing many sessions onto one pid.
    Declared,
}

impl Presence {
    fn as_str(self) -> &'static str {
        match self {
            Presence::Scan => "scan",
            Presence::Declared => "declared",
        }
    }
}

/// How long a `declared` session may go without a heartbeat before it is marked
/// stale. Sized well above a sane heartbeat interval so one missed beat (a GC
/// pause, a busy event loop) does not flip a healthy session.
pub const PRESENCE_TTL_MS: i64 = 90_000;

/// How long a session stays marked stale before the row is deleted. Much
/// longer than `PRESENCE_TTL_MS`, and the ratio is the point: a brief stall
/// must not cause real damage, and a laptop waking from sleep must not lose
/// every declared registration to a gap of one TTL.
pub const PRESENCE_EVICT_AFTER_MS: i64 = 10 * 60_000;

/// Renew a declared session's presence: writes `last_seen_at` and clears any
/// stale mark, nothing else. Not a re-registration, since a heartbeat is a
/// liveness signal, not a restatement of cwd, tty and channel. Returns whether
/// a row was renewed, so a runtime heartbeating a session already evicted
/// learns that it must re-register.
pub fn heartbeat(conn: &Connection, session_id: &str) -> Result<bool> {
    Ok(reach::touch_heartbeat(conn, session_id, mailbox::now_ms())? > 0)
}

/// Two-stage expiry for `declared` sessions: mark, then evict much later.
/// Stage one is cheap and reversible (the next heartbeat clears it, and a
/// stale row is still addressable, so a message parks in the mailbox rather
/// than failing); stage two is destructive and waits far longer. A row is
/// never marked before it has had one full TTL to send its first beat.
/// Returns `(marked, evicted)`.
pub fn sweep_declared(conn: &Connection) -> Result<(usize, usize)> {
    let now = mailbox::now_ms();
    let marked = reach::mark_stale(conn, now, now.saturating_sub_ms(PRESENCE_TTL_MS))?;
    let evicted = reach::evict_stale(conn, now.saturating_sub_ms(PRESENCE_EVICT_AFTER_MS))?;
    Ok((marked, evicted))
}

/// Register with an explicit presence regime and delivery channel. `deliver`
/// is `None` for the pane path (a tmux pane or terminal tty is inferred from
/// pid/tty); a harness with no tty declares `exec:<argv>` or a loopback
/// `http://…` instead.
pub fn register_with(
    conn: &Connection,
    session_id: &str,
    pid: i32,
    tty: Option<&str>,
    cwd: Option<&str>,
    presence: Presence,
    deliver: Option<&str>,
) -> Result<()> {
    // `runtime_id` is the composite id's middle segment, stored so the sweep and
    // `fl live` can filter without parsing a composite id in SQL.
    let runtime_id = session_id.split('/').nth(1);
    let now = mailbox::now_ms();
    reach::upsert_registration(
        conn,
        session_id,
        pid,
        tty,
        cwd,
        presence.as_str(),
        deliver,
        runtime_id,
        now,
    )?;

    // Bind this segment to a conversation, the address that survives the next
    // compaction. Registration is the only moment a rotation is observable:
    // transcripts carry no link between the id that ended and the one that
    // replaced it, so continuity comes from the same process re-registering
    // under a new name. A failure here must not fail the registration: losing
    // reachability under the segment id would trade a stale address for none.
    if let (Some(machine_id), Some(runtime_id)) = (session_id.split('/').next(), runtime_id) {
        let minted = format!("{machine_id}/{runtime_id}/conv-{}", uuid::Uuid::new_v4());
        let start = process_start(pid);
        let key = reach::ConversationKey {
            machine_id,
            runtime_id,
            pid,
            pid_start: start.as_deref(),
            cwd,
        };
        if let Err(e) = reach::join_conversation(conn, session_id, key, now, &minted) {
            tp_core::log_warn!("session registered, but conversation binding failed: {e:#}");
        }
    }
    Ok(())
}

/// Unregister a live session binding: the SessionEnd hook path.
///
/// A no-op if the stored binding's `pid` differs from `expected_pid`. A
/// session_id reused across a `/clear` has SessionEnd for the old incarnation
/// racing SessionStart for the new one, so a second registration may already
/// own the row; pinning to the pid means only the process that owned the
/// binding can remove it. `None` skips the check, for callers with no pid to
/// compare against. Returns whether a row was removed: "did nothing" is the
/// expected outcome the pin exists to create, not a success.
pub fn unregister(conn: &Connection, session_id: &str, expected_pid: Option<i32>) -> Result<bool> {
    let n = match expected_pid {
        Some(pid) => reach::delete_session_pinned(conn, session_id, pid)?,
        None => reach::delete_session(conn, session_id)?,
    };
    Ok(n > 0)
}

/// Resolve a session to an injectable target. Always re-verifies: pid alive
/// (`kill(pid, 0)`), pid → tty freshly (`ps -o tty=`, cheap on a /proc-less
/// macOS), then tty → tmux pane before tty → terminal session, never cached.
pub fn resolve(conn: &Connection, session_id: &str) -> Result<Target> {
    let Some(row) = reach::target_row(conn, session_id)? else {
        return Ok(Target::NotLive);
    };

    // A declared channel wins over pane inference: it is the runtime's own
    // statement about how to reach it. A stale row is not woken; the message
    // still lands in the mailbox, and waking a quiet host would spend a
    // delivery attempt on nothing.
    if let Some(raw) = row.deliver {
        if row.stale_at.is_some() {
            return Ok(Target::NotLive);
        }
        // A malformed channel is not a reason to fall back to pane injection;
        // the runtime said it has no pane. Report it.
        return DeliveryChannel::parse(&raw).map(Target::Channel);
    }

    if !process_alive(row.pid) {
        unregister(conn, session_id, None)?;
        return Ok(Target::NotLive);
    }

    let Some(tty) = tty_of_pid(row.pid) else {
        return Ok(Target::Unreachable);
    };
    resolve_tty(&tty)
}

/// Resolve a bare tty directly to an injectable target, for a process with no
/// `live_session` binding at all (e.g. another agent CLI reached via
/// `wake::type_raw`). The same tmux-then-terminal check `resolve()` uses once
/// it has a tty, for callers that already know the tty by other means.
pub fn resolve_tty(tty: &str) -> Result<Target> {
    if let Some(pane) = tmux_pane_for_tty(tty)? {
        return Ok(Target::Tmux(pane));
    }
    // Every declared terminal, in id order, first claim wins. Two terminals
    // cannot own one tty, so the order is fixed only for run-to-run stability.
    for cfg in terminal::all() {
        if terminal_owns_tty(&cfg, tty) {
            return Ok(Target::Terminal {
                id: cfg.id,
                tty: tty.to_string(),
            });
        }
    }
    Ok(Target::Unreachable)
}

/// Ask one terminal whether it owns `tty`. The tty is normalized by
/// `terminal::needle`: AppleScript answers the full `/dev/...` form, so a bare
/// form from `ps -o tty=` would never match without it.
fn terminal_owns_tty(cfg: &terminal::TerminalConfig, tty: &str) -> bool {
    let Some(script) = cfg.applescript_probe(tty) else {
        // A command-driven terminal answers by listing, not by scripting;
        // claiming ownership without asking would be worse than declining.
        return false;
    };
    std::process::Command::new("osascript")
        .args(["-e", &script])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim() == "ok")
        .unwrap_or(false)
}

/// `kill(pid, 0)`: liveness probe without sending a signal.
fn process_alive(pid: i32) -> bool {
    // `0`: exists and signalable. EPERM: exists but owned by another user
    // (root-owned pids answer EPERM to a non-root daemon), so alive. ESRCH:
    // gone. No memory is touched, so the unsafe block carries no obligation
    // beyond libc's FFI declaration; nothing runs between `kill` and the
    // errno read, so it names this call's failure.
    // nosemgrep: rust.lang.security.unsafe-usage.unsafe-usage
    match unsafe { libc::kill(pid, 0) } {
        0 => true,
        _ => std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM),
    }
}

#[cfg(test)]
mod liveness_tests {
    use super::process_alive;

    /// PID 1 always exists: as root the probe succeeds outright, as a normal
    /// user it answers EPERM, which must count as alive.
    #[test]
    fn a_process_we_cannot_signal_is_still_alive() {
        assert!(process_alive(1));
    }
}

/// `ps -o tty= -p <pid>` → `/dev/ttys003`.
fn tty_of_pid(pid: i32) -> Option<String> {
    let out = std::process::Command::new("ps")
        .args(["-o", "tty=", "-p", &pid.to_string()])
        .output()
        .ok()?;
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if s.is_empty() || s == "??" {
        None
    } else {
        // `ps` may return a bare `ttys003`; normalize to /dev/ttys003.
        Some(if s.starts_with('/') {
            s
        } else {
            format!("/dev/{s}")
        })
    }
}

/// `tmux list-panes -a -F '#{pane_tty} #{pane_id}'` → first pane whose tty matches.
fn tmux_pane_for_tty(tty: &str) -> Result<Option<String>> {
    let out = std::process::Command::new("tmux")
        .args(["list-panes", "-a", "-F", "#{pane_tty} #{pane_id}"])
        .output()
        .ok();
    let Some(out) = out else { return Ok(None) };
    if !out.status.success() {
        return Ok(None); // tmux not running
    }
    let needle = tty.trim_start_matches("/dev/").to_string();
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        let mut parts = line.split_whitespace();
        if let (Some(tt), Some(pane)) = (parts.next(), parts.next()) {
            let tt = tt.trim_start_matches("/dev/").to_string();
            if tt == needle {
                return Ok(Some(pane.to_string()));
            }
        }
    }
    Ok(None)
}

/// A process's start time, verbatim from `ps -o lstart=`. Not parsed:
/// identity needs only equality (a reused pid started later and prints a
/// different string), so parsing would add a locale-dependent failure mode.
/// `LC_ALL=C` pins the format because the string is compared against one
/// recorded earlier under a possibly different environment. `None` means
/// "unknown", not "different": callers fall back to the time window.
pub fn process_start(pid: i32) -> Option<String> {
    let out = std::process::Command::new("ps")
        .env("LC_ALL", "C")
        .args(["-o", "lstart=", "-p", &pid.to_string()])
        .output()
        .ok()?;
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!s.is_empty()).then_some(s)
}

/// Turn any address into the session id that should receive mail. A
/// conversation address (`…/conv-<uuid>`) resolves to whichever segment that
/// conversation currently answers on; a session id is followed forward to its
/// conversation's current segment. Anything else is returned unchanged:
/// resolution is not the place to judge an address (`validate_address` and
/// `addressability` do that).
pub fn address_to_session(conn: &Connection, address: &str) -> Result<String> {
    if is_conversation_address(address) {
        return Ok(reach::conversation_current_session(conn, address)?
            .unwrap_or_else(|| address.to_string()));
    }
    // Addressing a segment is a request to reach the correspondent it belonged
    // to, not to write into a particular transcript, and that correspondent may
    // have compacted since the address was copied; replying to the literal
    // segment would park the reply. `fl inbox --session-id <segment>` still
    // reads an old segment's mailbox directly.
    if let Some(conv) = reach::conversation_of(conn, address)? {
        if let Some(current) = reach::conversation_current_session(conn, &conv)? {
            return Ok(current);
        }
    }
    Ok(address.to_string())
}

/// Whether an address names a conversation rather than a transcript segment.
/// The `conv-` prefix on the last segment is minted by floonet, never by a
/// runtime; a native id that happened to start with it is still namespaced
/// under its `runtime_id`, and the lookup that follows finds nothing.
pub fn is_conversation_address(address: &str) -> bool {
    address
        .rsplit('/')
        .next()
        .is_some_and(|last| last.starts_with("conv-"))
}

/// The stable address to publish for a session, if it has one.
pub fn conversation_address(conn: &Connection, session_id: &str) -> Result<Option<String>> {
    reach::conversation_of(conn, session_id)
}

/// Every conversation this session's pane owns, most-recently-seen first.
/// Re-exported so the reach crate stays the one surface the CLI talks to for
/// session identity. Callers want the head (an address to publish) or the
/// whole list (mailboxes to drain).
pub fn conversations_of_pane(conn: &Connection, session_id: &str) -> Result<Vec<String>> {
    reach::conversations_of_pane(conn, session_id)
}

/// Which runtime is hosting this process, by walking up and matching each
/// ancestor's image name against the supplied `(runtime_id, pattern)` pairs.
/// Nearest ancestor wins: a session started from another runtime's session
/// has both in its chain, and the outermost would name the launcher rather
/// than the host. `None` when nothing matches is a real answer, not a guess.
/// A `=`-anchored pattern is an exact `comm`; anything else is a substring,
/// the same rule `discover::ProcessSignature` applies.
pub fn runtime_of_host(from_pid: i32, signatures: &[(String, String)]) -> Option<String> {
    let mut pid = from_pid;
    for _ in 0..16 {
        let (ppid, comm, _) = ps_info(pid)?;
        let comm = comm.to_lowercase();
        for (runtime_id, pattern) in signatures {
            let hit = match pattern.strip_prefix('=') {
                Some(exact) => comm == exact.to_lowercase(),
                None => comm.contains(&pattern.to_lowercase()),
            };
            if hit {
                return Some(runtime_id.clone());
            }
        }
        if ppid <= 0 || ppid == pid {
            break;
        }
        pid = ppid;
    }
    None
}

/// Walk up the parent chain from `from_pid`, returning the first ancestor that
/// has a controlling tty and whose image name contains `needle` (falling back
/// to the first ancestor with any tty). Used by the SessionStart hook: `tp`
/// runs as a child of the session, so one of its ancestors IS the session
/// process and shares its terminal.
pub fn find_session_process(from_pid: i32, needle: &str) -> Option<(i32, String)> {
    let mut pid = from_pid;
    let mut matched_without_tty: Option<i32> = None;
    let mut fallback: Option<(i32, String)> = None;
    for _ in 0..16 {
        let (ppid, comm, tty) = ps_info(pid)?;
        let needle_match = comm.to_lowercase().contains(&needle.to_lowercase());
        if needle_match {
            if let Some(tty) = &tty {
                return Some((pid, tty.clone()));
            }
            // Matched the session process but it has no tty (e.g. a headless
            // runner); remember it, keep walking for a tty-bearing one.
            if matched_without_tty.is_none() {
                matched_without_tty = Some(pid);
            }
        }
        if let Some(tty) = &tty {
            if fallback.is_none() {
                fallback = Some((pid, tty.clone()));
            }
        }
        if ppid <= 0 || ppid == pid {
            break;
        }
        pid = ppid;
    }
    // Prefer a tty-bearing ancestor; fall back to the matched process even
    // without a tty (the caller can still record the binding).
    matched_without_tty
        .map(|p| (p, "/dev/none".to_string()))
        .or(fallback)
}

/// The answer to "which session am I", including why when there is none. Not
/// an `Option`: "several candidates" and "never registered" call for opposite
/// advice, and a type that merges outcomes a caller would branch on decides on
/// the caller's behalf without telling it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OwnSession {
    Resolved(String),
    /// Several sessions are registered on this process and none is
    /// unambiguously "mine". Carries them, because the caller can name one.
    Ambiguous(Vec<String>),
    /// No ancestor process has any registration.
    Unknown,
}

impl OwnSession {
    pub fn resolved(self) -> Option<String> {
        match self {
            OwnSession::Resolved(s) => Some(s),
            _ => None,
        }
    }
}

/// The single-answer view of `own_session`, for a caller that has only one
/// thing to do with an id. `None` is "no ancestor is registered"; ambiguity is
/// an error naming the candidates, because "pick one of these" and "nothing to
/// pick from" call for opposite handling and a caller that can act on the
/// difference should call `own_session` instead.
pub fn session_of_process(conn: &Connection, from_pid: i32) -> Result<Option<String>> {
    match own_session(conn, from_pid)? {
        OwnSession::Resolved(sid) => Ok(Some(sid)),
        OwnSession::Ambiguous(candidates) => bail!(
            "several sessions are registered on this process and none is unambiguously ours: {}",
            candidates.join(", ")
        ),
        OwnSession::Unknown => Ok(None),
    }
}

/// Which registered live session is this process running inside? Walks up the
/// parent chain and asks the registry, not the environment or a process name:
/// the pid is the one identity that cannot fork. A hook-set env var can
/// disagree with what the SessionStart hook registered (after a `--resume`),
/// and a runtime with no env var has nothing to read; resolving through the
/// registry agrees with what `fl live` advertises by construction.
pub fn own_session(conn: &Connection, from_pid: i32) -> Result<OwnSession> {
    let mut pid = from_pid;
    // Same bound as `find_session_process`: a session process is a handful of
    // levels up at most, and this stops a pid cycle from spinning.
    for _ in 0..16 {
        // One pid may own many rows: a multiplexed runtime registers several
        // `declared` sessions against its host process. A `scan` row is the
        // process's own session, since the scan identifies a session by that
        // process; a `declared` row only means "this runtime named its host
        // pid", which several sessions share. So scan wins, and picking
        // arbitrarily would stamp the wrong sender on every `fl ask`.
        let rows = reach::rows_for_pid(conn, pid)?;
        let scan_owned: Vec<&(String, String)> = rows.iter().filter(|(_, p)| p == "scan").collect();
        if let [(sid, _)] = scan_owned.as_slice() {
            return Ok(OwnSession::Resolved(sid.clone()));
        }
        if scan_owned.is_empty() {
            // No scan-owned session on this pid. A single declared row is still
            // an unambiguous answer; several are not, and ambiguity is reported
            // rather than guessed.
            if let [(sid, _)] = rows.as_slice() {
                return Ok(OwnSession::Resolved(sid.clone()));
            }
            if rows.len() > 1 {
                return Ok(OwnSession::Ambiguous(
                    rows.iter().map(|(sid, _)| sid.clone()).collect(),
                ));
            }
        } else if scan_owned.len() > 1 {
            // Two scan rows on one pid happen transiently while a runtime
            // rotates its session id; ambiguity is still not a guess.
            return Ok(OwnSession::Ambiguous(
                scan_owned.iter().map(|(sid, _)| sid.clone()).collect(),
            ));
        }
        let Some((ppid, _, _)) = ps_info(pid) else {
            break;
        };
        if ppid <= 0 || ppid == pid {
            break;
        }
        pid = ppid;
    }
    Ok(OwnSession::Unknown)
}
/// `(ppid, comm, tty)` for a pid. `comm` is requested last: macOS `ps` gives a
/// middle column a fixed width and truncates it, which would cut a long
/// executable path short of the needle callers match on. The last column is
/// never truncated, so no width guess is needed. `tty` is `??` when there is
/// none, filtered to `None`.
fn ps_info(pid: i32) -> Option<(i32, String, Option<String>)> {
    let out = std::process::Command::new("ps")
        .args(["-o", "ppid=,tty=,comm=", "-p", &pid.to_string()])
        .output()
        .ok()?;
    let line = String::from_utf8_lossy(&out.stdout).trim().to_string();
    let mut parts = line.split_whitespace();
    let ppid = parts.next()?.parse().ok()?;
    let tty = parts
        .next()
        .map(|s| s.to_string())
        .filter(|s| !s.is_empty() && s != "??");
    let comm = parts.next().unwrap_or("").to_string();
    Some((ppid, comm, tty))
}

#[cfg(test)]
mod own_session_tests {
    use super::*;

    fn db() -> tp_db::Db {
        let d = tp_db::Db::open(std::path::Path::new(":memory:")).unwrap();
        d.ensure_self_machine("m1", "h").unwrap();
        d
    }

    /// Ambiguity must be reportable, not just refused: two segments sharing a
    /// pid while a runtime rotates its session id is a different state from
    /// "never registered", and the caller can pick from the candidates.
    #[test]
    fn ambiguity_names_the_candidates_instead_of_looking_unregistered() {
        let d = db();
        for sid in ["m1/codex/one", "m1/codex/two"] {
            d.conn()
                .execute(
                    "INSERT INTO live_session(session_id, pid, source, registered_at, last_seen_at, presence)
                     VALUES (?1, 4242, 'hook', 0, 0, 'scan')",
                    [sid],
                )
                .unwrap();
        }
        match own_session(d.conn(), 4242).unwrap() {
            OwnSession::Ambiguous(mut c) => {
                c.sort();
                assert_eq!(c, vec!["m1/codex/one", "m1/codex/two"]);
            }
            other => panic!("expected Ambiguous, got {other:?}"),
        }
        // And a pid with nothing registered stays distinguishable from it.
        assert_eq!(own_session(d.conn(), 9999).unwrap(), OwnSession::Unknown);
    }

    #[test]
    fn the_scan_owned_session_wins_over_declared_ones_sharing_its_pid() {
        let d = db();
        // Declared rows first, the real ordering: the host is launched from
        // the Claude Code session, so its sessions register while that one is
        // already live, and insertion order must not decide the answer.
        for sid in ["m1/dsh/hosted-a", "m1/dsh/hosted-b"] {
            register_with(d.conn(), sid, 11822, None, None, Presence::Declared, None).unwrap();
        }
        register(
            d.conn(),
            "m1/claude_code/mine",
            11822,
            Some("/dev/ttys7"),
            None,
        )
        .unwrap();

        assert_eq!(
            session_of_process(d.conn(), 11822).unwrap().as_deref(),
            Some("m1/claude_code/mine"),
            "the process's OWN session is the scan-identified one; declared rows              only claim it as a host"
        );
    }

    /// With no scan row to disambiguate, several declared sessions on one pid
    /// is a genuine ambiguity; returning any of them would stamp the wrong
    /// sender.
    #[test]
    fn several_declared_sessions_on_one_pid_are_ambiguous_not_guessed() {
        let d = db();
        for sid in ["m1/dsh/a", "m1/dsh/b"] {
            register_with(d.conn(), sid, 7000, None, None, Presence::Declared, None).unwrap();
        }
        let err = session_of_process(d.conn(), 7000).unwrap_err().to_string();
        assert!(
            err.contains("m1/dsh/a") && err.contains("m1/dsh/b"),
            "ambiguity must name its candidates, got: {err}"
        );
        // And "nothing registered" stays a plain answer, distinct from it.
        assert_eq!(session_of_process(d.conn(), 7002).unwrap(), None);
    }

    /// One declared session alone is unambiguous and must still resolve;
    /// otherwise a runtime that declares its own pid could never identify itself.
    #[test]
    fn a_lone_declared_session_still_resolves() {
        let d = db();
        register_with(
            d.conn(),
            "m1/dsh/only",
            7001,
            None,
            None,
            Presence::Declared,
            None,
        )
        .unwrap();
        assert_eq!(
            session_of_process(d.conn(), 7001).unwrap().as_deref(),
            Some("m1/dsh/only")
        );
    }
}

#[cfg(test)]
mod channel_tests {
    use super::DeliveryChannel;

    #[test]
    fn exec_channels_split_on_whitespace() {
        assert_eq!(
            DeliveryChannel::parse("exec:/usr/local/bin/dsh-tp --wake").unwrap(),
            DeliveryChannel::Exec(vec!["/usr/local/bin/dsh-tp".into(), "--wake".into()])
        );
    }

    /// Enforced in code rather than trusted to the descriptor: a channel makes
    /// an agent act, and there is no legitimate remote channel, so refusing a
    /// routable address is lossless.
    #[test]
    fn http_channels_must_be_loopback() {
        for ok in [
            "http://127.0.0.1:8125/floonet/wake",
            "http://localhost:8125/wake",
        ] {
            assert!(
                DeliveryChannel::parse(ok).is_ok(),
                "{ok} should be accepted"
            );
        }
        for bad in [
            "http://192.0.2.42:8125/wake",
            "http://evil.example.com/wake",
            "https://0.0.0.0:8125/wake",
        ] {
            let err = DeliveryChannel::parse(bad).unwrap_err().to_string();
            assert!(
                err.contains("loopback"),
                "{bad} must be refused as non-loopback, got: {err}"
            );
        }
    }

    /// The host is extracted, not guessed at with a rightmost-colon split: a
    /// bracketed IPv6 literal carries colons of its own, and a userinfo part
    /// can put a loopback-looking string in front of a routable host.
    #[test]
    fn ipv6_and_userinfo_hosts_are_parsed_rather_than_split_on_the_last_colon() {
        for ok in [
            "http://[::1]:47400/wake",
            "http://[::1]/wake",
            "http://[::1]",
        ] {
            assert!(
                DeliveryChannel::parse(ok).is_ok(),
                "{ok} is loopback and should be accepted"
            );
        }
        for bad in [
            "http://[2001:db8::1]:47400/wake",
            "http://127.0.0.1:80@evil.example.com/wake",
            "http://localhost@evil.example.com/wake",
        ] {
            let err = DeliveryChannel::parse(bad).unwrap_err().to_string();
            assert!(
                err.contains("loopback"),
                "{bad} must be refused as non-loopback, got: {err}"
            );
        }
    }

    #[test]
    fn garbage_is_refused_rather_than_guessed() {
        for bad in ["", "exec:", "ftp://127.0.0.1/x", "just-a-string"] {
            assert!(
                DeliveryChannel::parse(bad).is_err(),
                "{bad:?} should be refused"
            );
        }
    }
}

#[cfg(test)]
mod presence_tests {
    use super::*;
    use rusqlite::{params, OptionalExtension};

    fn db() -> tp_db::Db {
        let d = tp_db::Db::open(std::path::Path::new(":memory:")).unwrap();
        d.ensure_self_machine("m1", "h").unwrap();
        d
    }

    /// Backdate a row so the sweep sees it as old, without sleeping.
    fn age(conn: &Connection, sid: &str, by_ms: i64) {
        conn.execute(
            "UPDATE live_session SET last_seen_at = last_seen_at - ?2,
                                     registered_at = registered_at - ?2
              WHERE session_id = ?1",
            params![sid, by_ms],
        )
        .unwrap();
    }

    fn row(conn: &Connection, sid: &str) -> Option<Option<i64>> {
        conn.query_row(
            "SELECT stale_at FROM live_session WHERE session_id = ?1",
            [sid],
            |r| r.get::<_, Option<i64>>(0),
        )
        .optional()
        .unwrap()
    }

    #[test]
    fn expiry_marks_before_it_evicts() {
        let d = db();
        register_with(
            d.conn(),
            "m1/dsh/s",
            1,
            None,
            None,
            Presence::Declared,
            None,
        )
        .unwrap();

        // Fresh: untouched.
        assert_eq!(sweep_declared(d.conn()).unwrap(), (0, 0));
        assert_eq!(
            row(d.conn(), "m1/dsh/s"),
            Some(None),
            "healthy row not marked"
        );

        // Past the TTL: marked, not deleted; a stale session is still
        // addressable, so a message still lands in its mailbox.
        age(d.conn(), "m1/dsh/s", PRESENCE_TTL_MS + 1_000);
        assert_eq!(sweep_declared(d.conn()).unwrap(), (1, 0));
        assert!(
            row(d.conn(), "m1/dsh/s").unwrap().is_some(),
            "should be marked stale"
        );

        // Marking is idempotent; a second sweep must not re-mark.
        assert_eq!(sweep_declared(d.conn()).unwrap(), (0, 0));

        // Only after the much longer eviction window is the row removed.
        d.conn()
            .execute(
                "UPDATE live_session SET stale_at = stale_at - ?1",
                params![PRESENCE_EVICT_AFTER_MS + 1_000],
            )
            .unwrap();
        assert_eq!(sweep_declared(d.conn()).unwrap(), (0, 1));
        assert_eq!(row(d.conn(), "m1/dsh/s"), None, "evicted");
    }

    /// The grace window: a session registered moments ago has not had time to
    /// send its first beat, and a slow start must not sweep its own row.
    #[test]
    fn a_just_registered_session_is_never_marked() {
        let d = db();
        register_with(
            d.conn(),
            "m1/dsh/new",
            1,
            None,
            None,
            Presence::Declared,
            None,
        )
        .unwrap();
        // Old heartbeat, but registered just now.
        d.conn()
            .execute(
                "UPDATE live_session SET last_seen_at = last_seen_at - ?1",
                params![PRESENCE_TTL_MS * 5],
            )
            .unwrap();
        assert_eq!(sweep_declared(d.conn()).unwrap(), (0, 0));
        assert_eq!(row(d.conn(), "m1/dsh/new"), Some(None));
    }

    #[test]
    fn a_heartbeat_clears_the_stale_mark() {
        let d = db();
        register_with(
            d.conn(),
            "m1/dsh/s",
            1,
            None,
            None,
            Presence::Declared,
            None,
        )
        .unwrap();
        age(d.conn(), "m1/dsh/s", PRESENCE_TTL_MS + 1_000);
        sweep_declared(d.conn()).unwrap();
        assert!(row(d.conn(), "m1/dsh/s").unwrap().is_some());

        assert!(heartbeat(d.conn(), "m1/dsh/s").unwrap(), "renewed");
        assert_eq!(row(d.conn(), "m1/dsh/s"), Some(None), "recovered");
        assert_eq!(sweep_declared(d.conn()).unwrap(), (0, 0));
    }

    /// The sweep must never touch a scan-governed row: that one is the process
    /// scan's to prune, and a scan-governed session cannot heartbeat.
    #[test]
    fn scan_sessions_are_not_swept() {
        let d = db();
        register(d.conn(), "m1/claude_code/s", 1, None, None).unwrap();
        age(d.conn(), "m1/claude_code/s", PRESENCE_EVICT_AFTER_MS * 10);
        assert_eq!(sweep_declared(d.conn()).unwrap(), (0, 0));
        assert!(
            row(d.conn(), "m1/claude_code/s").is_some(),
            "must still exist"
        );
    }

    #[test]
    fn heartbeating_an_unknown_session_reports_it() {
        let d = db();
        assert!(!heartbeat(d.conn(), "m1/dsh/never-registered").unwrap());
    }
}
