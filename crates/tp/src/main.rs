use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use std::path::PathBuf;

mod cmd;
mod mcp;

// Re-exported so `mcp.rs` addresses every command as `crate::`.
pub(crate) use cmd::net::*;
pub(crate) use cmd::reach::*;
pub(crate) use cmd::read::*;

#[derive(Parser)]
#[command(
    name = "fl",
    about = "Floonet — cross-agent session search and reach",
    version = tp_core::VERSION_LINE
)]
struct Cli {
    #[command(subcommand)]
    cmd: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Search across sessions. Returns coordinates + an excerpt, not full transcripts.
    Search {
        query: String,
        /// Also search (and return) `thinking`. Off by default at every layer.
        #[arg(long)]
        include_thinking: bool,
        /// Restrict to sessions whose path matches this folder.
        #[arg(long)]
        folder: Option<String>,
        /// Start of the window: a duration ago (6h, 3d) or an absolute local
        /// time (2026-08-04). Default 6h.
        #[arg(long, default_value = DEFAULT_SEARCH_SINCE)]
        since: String,
        /// End of the window, exclusive — same spellings. Pair with an absolute
        /// `--since` to ask about one day rather than "the last N".
        #[arg(long)]
        until: Option<String>,
        #[arg(long)]
        regex: bool,
        #[arg(long, default_value_t = 20)]
        limit: usize,
        /// Also query every trusted peer. Peers that fail or answer partially
        /// are always reported — a silent partial result would read as "this
        /// was never discussed anywhere".
        #[arg(long)]
        all: bool,
        /// Query one named peer (repeatable). An id prefix is enough — the same
        /// short form `fl peers` prints.
        ///
        /// Prefer this over `--all` once you have more than a handful of
        /// machines: a peer answers a search by scanning its whole corpus, so
        /// `--all` asks every trusted machine to do that. Naming them works at
        /// any number; `--all` refuses past a threshold.
        #[arg(long = "peer")]
        peers: Vec<String>,
    },
    /// List known sessions, most-recently-active first — everything indexed,
    /// including sessions long ended and companion sessions other tools run,
    /// which on a busy machine can crowd out the ones you meant. To find a
    /// session you can message right now, use `fl live` instead.
    Sessions {
        #[arg(long)]
        folder: Option<String>,
        /// Start of the window: a duration ago (7d) or an absolute local time
        /// (2026-08-04). Default 7d.
        #[arg(long, default_value = "7d")]
        since: String,
        /// End of the window, exclusive — same spellings. With an absolute
        /// `--since`, this answers "which sessions were active that day".
        #[arg(long)]
        until: Option<String>,
        #[arg(long, default_value_t = 20)]
        limit: usize,
    },
    /// Fetch turns for one session.
    Turns {
        /// Omit it with `--since`/`--folder` to read the most recent session in
        /// the window instead of naming one.
        session_id: Option<String>,
        /// Resume after this unix-ms timestamp (the universal cursor).
        /// Reads forward and keeps the oldest turns if it overflows.
        #[arg(long)]
        after_ts: Option<i64>,
        /// Start of the window: a duration ago (`4h`, `2d`) or an absolute
        /// local time (`2026-08-04`, `2026-08-04T14:30`). Keeps the newest
        /// turns if it overflows. Mutually exclusive with `--after-ts`.
        #[arg(long, conflicts_with = "after_ts")]
        since: Option<String>,
        /// End of the window, exclusive — same spellings as `--since`. Pair it
        /// with an absolute `--since` to read one specific day; page backward by
        /// passing the earliest `ts` the previous page returned.
        #[arg(long, requires = "since")]
        until: Option<String>,
        /// Which session to read when `session_id` is omitted — folder name,
        /// path, or substring.
        #[arg(long)]
        folder: Option<String>,
        #[arg(long)]
        include_thinking: bool,
        #[arg(long, default_value_t = 200)]
        limit: usize,
    },
    /// Enqueue a message into a session's mailbox (and wake it if reachable).
    Ask {
        session_id: String,
        /// The message body. Use single quotes, or a quoted heredoc, whenever
        /// it contains code: inside a double-quoted argument the shell expands
        /// backticks, `$` and `!` before floonet sees anything, and the message
        /// is delivered in full with a silently different body. See README
        /// "Sending code in a message" for the heredoc form.
        message: String,
        /// Don't attempt to wake the target pane — just park the message.
        #[arg(long)]
        no_wake: bool,
        /// This session's id, stamped on the message as the return address so
        /// the target can `fl reply` to it. Bare native id or composite both
        /// work. Defaults to `$CLAUDE_CODE_SESSION_ID` when set; without
        /// either, the message is one-way and the target is told so.
        #[arg(long)]
        from_session: Option<String>,
        /// Runtime of `--from-session` when that is a bare native id.
        #[arg(long, default_value = "claude_code")]
        runtime: String,
    },
    /// Tell a session something without asking it for anything.
    ///
    /// Same delivery as `ask` — it wakes the target, because a status update
    /// that arrives hours later is not much of an update — but the message is
    /// marked so the receiver is told plainly that no reply is expected, and
    /// this command does not tell you to wait for one.
    ///
    /// Use it for "I pushed the fix", "your build is green", "heads up, I
    /// changed X". Use `ask` when you need something back.
    Note {
        session_id: String,
        /// The message body — see `ask` for how to quote one containing code.
        message: String,
        /// Park it without waking the target.
        #[arg(long)]
        no_wake: bool,
        /// See `ask --from-session`.
        #[arg(long)]
        from_session: Option<String>,
        /// See `ask --runtime`.
        #[arg(long, default_value = "claude_code")]
        runtime: String,
    },
    /// Answer a message from your inbox. The address comes from the original
    /// message, so a reply can't be misrouted the way a hand-addressed
    /// `fl ask` can — and it links the two, so a conversation is followable.
    Reply {
        /// Message id (the short form `fl inbox` prints is enough).
        message_id: String,
        /// The message body — see `ask` for how to quote one containing code.
        message: String,
        /// Don't attempt to wake the original sender.
        #[arg(long)]
        no_wake: bool,
        /// See `ask --from-session` — lets your answer be replied to in turn.
        #[arg(long)]
        from_session: Option<String>,
        /// See `ask --runtime`.
        #[arg(long, default_value = "claude_code")]
        runtime: String,
    },
    /// Drain this session's mailbox (the `/fl inbox` control string, or a
    /// runtime extension's own equivalent command, triggers this).
    ///
    /// Draining marks a message read, not acked — read means shown, ack means
    /// you confirm you finished acting on it (`fl ack`). A message shown but
    /// never acked is not gone: `--pending` recovers it, so a session
    /// interrupted mid-batch (context compaction, a crash) can find exactly
    /// what it left undone rather than having it vanish with the drain that
    /// showed it.
    Inbox {
        #[arg(long)]
        session_id: Option<String>,
        /// Runtime this session belongs to — only matters when `--session-id`
        /// is a bare native id (needs composing); ignored for an
        /// already-composite id. Default matches the Claude Code hook path.
        #[arg(long, default_value = "claude_code")]
        runtime: String,
        /// Show messages that were drained but never acked, instead of
        /// draining new ones. Read-only — checking this never counts as
        /// having handled anything, and it never marks anything itself.
        #[arg(long, conflicts_with = "history")]
        pending: bool,
        /// Show acked messages from the window instead of draining new ones —
        /// "what did that say again". Read-only.
        #[arg(long, conflicts_with = "pending")]
        history: bool,
        /// Window for `--history`: a duration ago (`4h`, `2d`) or an absolute
        /// local time. Ignored without `--history`.
        #[arg(long, default_value = "24h", requires = "history")]
        since: String,
    },
    /// Confirm you finished acting on a message from your inbox — the ack
    /// half of the read/ack split. `fl inbox` marks a message read the moment
    /// it is shown; that is not the same as having acted on it. Ack after you
    /// have actually done what the message asked (or decided a `[note]` needs
    /// no action) — an unacked message stays recoverable forever via
    /// `fl inbox --pending`, so acking is what tells floonet you are done
    /// with it, not what tells floonet you saw it.
    Ack {
        /// Message id (the short form `fl inbox` prints is enough).
        message_id: String,
    },
    /// Type text directly into a tty's pane (tmux or iTerm2) — no mailbox, no
    /// control string, no confirmation gate on the receiving end unless the
    /// target process provides its own. This is not the safe `ask` path: use
    /// it only for a target with no `/fl inbox` (or runtime-native
    /// equivalent — see integrations/pi/floonet.ts) to dereference through. Find
    /// the tty with `ps -o tty=,comm=` or the terminal's own window/tab
    /// title. Deliberately CLI-only — not exposed as an MCP tool, so this
    /// always requires a human to run it explicitly, never an agent invoking
    /// it unprompted.
    Type {
        /// e.g. ttys000 (with or without the /dev/ prefix)
        tty: String,
        message: String,
    },
    /// Register this session as live (SessionStart hook, or a runtime
    /// extension's session-start event). Writes pid + tty.
    Register {
        /// Native session id (the runtime's own id, not floonet's composite
        /// form — `tp` builds that itself). Required unless `--from-hook`.
        #[arg(long)]
        session_id: Option<String>,
        #[arg(long)]
        cwd: Option<String>,
        /// Read `session_id`/`cwd` from the hook's stdin JSON payload instead
        /// of flags; hooks pass event data only on stdin, never as env vars.
        /// Ignored with `--session-id`, which runtime extensions use instead.
        #[arg(long)]
        from_hook: bool,
        /// Runtime this session belongs to — e.g. "pi" for a runtime extension
        /// registering itself directly. Omitted means "work it out": floonet
        /// walks up from this process and matches the nearest ancestor against
        /// every runtime's declared `process_match`, falling back to
        /// "claude_code" if none does.
        #[arg(long)]
        runtime: Option<String>,
        /// Who owns this session's liveness. `scan` (default) lets floonet's
        /// process scan create and prune the row, which is right for a runtime
        /// it can see in `ps` with a tty. `declared` means the runtime owns it
        /// and renews by heartbeat — required for a harness the scan cannot
        /// observe (a web GUI with no tty, a host multiplexing many sessions
        /// onto one pid), whose rows the scan would otherwise prune within one
        /// interval. See the presence rules in `tp-reach`.
        #[arg(long, default_value = "scan")]
        presence: String,
        /// This session's own process id. A `declared` runtime knows which
        /// process hosts it; floonet's fallback — walk up from `tp`'s own pid
        /// to the nearest matching ancestor — finds whatever launched the
        /// runtime instead, and replies would route there.
        #[arg(long)]
        pid: Option<i32>,
        /// How to deliver a wake to this session, when there is no pane to type
        /// into: `exec:<argv>` (spawned, control string on stdin) or a loopback
        /// `http://127.0.0.1:<port>/<path>` (POSTed). Omit to infer a tmux pane
        /// or iTerm2 tty from the process. Only the fixed control string ever
        /// crosses either — never message content.
        #[arg(long)]
        deliver: Option<String>,
    },
    /// Renew a `--presence declared` session's liveness. A runtime that owns its
    /// own presence calls this on a timer; miss enough of them and the session
    /// is marked stale, then evicted. Writes only the timestamp — never cwd,
    /// tty or channel, which registration owns.
    Heartbeat {
        #[arg(long)]
        session_id: String,
        /// See `register --runtime`.
        #[arg(long, default_value = "claude_code")]
        runtime: String,
    },
    /// Unregister a live session (SessionEnd hook, or a runtime extension's
    /// session-shutdown event).
    Unregister {
        #[arg(long)]
        session_id: Option<String>,
        /// See `register --from-hook`.
        #[arg(long)]
        from_hook: bool,
        /// See `register --runtime`.
        #[arg(long)]
        runtime: Option<String>,
    },
    /// List sessions currently live on this machine — reconciled by `fld`'s
    /// active scan, not just hook registrations. Requires `fld` running: this
    /// only reads what it has already reconciled.
    Live,
    /// Whether this machine accepts connections from other machines.
    ///
    /// Off until you turn it on. floonet is otherwise entirely local — it reads
    /// transcripts, writes one database, and types into terminals on this
    /// machine — so a peer port is the one thing that makes it reachable from
    /// outside, and it is not switched on for you. Pairing works either way:
    /// `fl pair` is local and human-approved, so pair first and listen after.
    Listen {
        /// `on`, `off`, or omitted to report the current state.
        #[arg(value_parser = ["on", "off"])]
        state: Option<String>,
        /// Port to listen on. Remembered; omit to keep the current one.
        #[arg(long)]
        port: Option<u16>,
    },
    /// Show this machine's identity — the fingerprint peers compare.
    Id,
    /// List machines we have a relationship with, and their trust state.
    Peers,
    /// Ask a host whether it runs a floonet daemon. Read-only: answering is
    /// not knowing, so nothing is trusted or stored for strangers.
    Discover {
        /// Host to probe, optionally `host:port`. A bare host is tried across
        /// the default port and its next few neighbours.
        host: String,
    },
    /// Pairing. Trust is only ever granted by `approve`, locally, by a human.
    #[command(subcommand)]
    Pair(PairCmd),
    /// Run an MCP server over stdio, exposing search/reach/federation as tools
    /// for an MCP client (e.g. Claude Code) to call directly.
    Mcp,
    /// This binary's build and the daemon's, which are not the same question:
    /// installing binaries does not restart the LaunchAgent.
    Version,
    /// Put back the binaries the last install replaced.
    ///
    /// `install.sh` keeps exactly one generation, in `~/.local/bin/.tp.prev`
    /// and `.tpd.prev`. This swaps them back and restarts the daemon — the
    /// thing you need when a new build starts crashing, which for a LaunchAgent
    /// means launchd restarting it forever while you have nothing to put back.
    Rollback,

    /// Check the index for damage, and say how much of it is irreplaceable:
    /// runtimes delete their transcripts, so on a long-running install some
    /// sessions exist nowhere else.
    Verify {
        /// Full page-by-page check instead of the fast structural one. Slower,
        /// and the only one that reads every page.
        #[arg(long)]
        full: bool,
    },
    /// Copy the index somewhere durable, consistently, while it is in use.
    ///
    /// `VACUUM INTO` rather than `cp`: copying a live SQLite file can capture a
    /// torn page set plus an unapplied WAL, which restores as a corrupt or
    /// silently stale database. This writes one consistent, compacted snapshot.
    Backup {
        /// Destination file. Refused if it already exists — an overwrite here
        /// destroys the previous backup, which is the thing being protected.
        dest: PathBuf,
    },
}

#[derive(Subcommand)]
enum PairCmd {
    /// Introduce this machine to a peer at `host:port`. Records it as
    /// pending on both sides; neither is trusted until each approves.
    Request { addr: String },
    /// Pending and trusted peers, with the fingerprint to compare.
    List,
    /// Trust a peer. Compare its fingerprint out of band first — this is the
    /// step that decides who may read this machine's sessions.
    Approve { device_id: String },
    /// Refuse a peer that has not been trusted yet, removing it. For a peer
    /// that IS trusted, use `revoke` — same effect, different mistake.
    Reject { device_id: String },
    /// Take back trust from a peer that currently has it, removing it.
    /// Purely local and immediate: the very next signed request from that
    /// device is refused, but it is not notified — there is no route for that.
    Revoke { device_id: String },
}

/// A point in time, written the way a person would. A relative bound alone
/// cannot express a specific past day, and a quiet day would silently return
/// an earlier busy day's turns. Accepted, in order of specificity:
///   `2026-08-04`         → local midnight that day
///   `2026-08-04T14:30`   → local wall-clock
///   `1786437984823`      → unix ms, for machine paging
///   `4h` / `2d`          → that long before `now`
fn parse_time_bound(s: &str, now_ms: tp_core::Millis) -> Result<i64> {
    let s = s.trim();
    if let Ok(d) = chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d") {
        return local_ms(d.and_hms_opt(0, 0, 0).unwrap(), s);
    }
    for fmt in ["%Y-%m-%dT%H:%M:%S", "%Y-%m-%dT%H:%M", "%Y-%m-%d %H:%M"] {
        if let Ok(dt) = chrono::NaiveDateTime::parse_from_str(s, fmt) {
            return local_ms(dt, s);
        }
    }
    // Unix ms: only a bare integer this large is unambiguous against `4h`.
    if s.len() >= 10 && s.chars().all(|c| c.is_ascii_digit()) {
        return s.parse().with_context(|| format!("bad timestamp {s:?}"));
    }
    Ok(now_ms
        .saturating_sub_ms(parse_duration(s)?.as_millis() as i64)
        .get())
}

/// Interpreted in the machine's zone, matching how `fmt_ts` prints timestamps:
/// a date typed after reading local-time output must mean the same day.
fn local_ms(dt: chrono::NaiveDateTime, orig: &str) -> Result<i64> {
    use chrono::TimeZone;
    match chrono::Local.from_local_datetime(&dt).earliest() {
        Some(t) => Ok(t.timestamp_millis()),
        // A DST spring-forward gap has no such local time. Saying so beats
        // silently shifting the window by an hour.
        None => anyhow::bail!("{orig:?} does not exist in this timezone (DST gap)"),
    }
}

/// Accepts `5s`, `30m`, `6h`, `3d`, `2w`. A bare number is hours, the useful
/// default for a search window.
fn parse_duration(s: &str) -> Result<std::time::Duration> {
    let s = s.trim();
    let (num, unit) = s.split_at(s.find(|c: char| c.is_alphabetic()).unwrap_or(s.len()));
    let n: u64 = num.parse().with_context(|| format!("bad duration {s:?}"))?;
    // Checked, and overflow is an error rather than a clamp: release builds
    // wrap silently and would hand back an arbitrary window, and a caller
    // that typed a nonsense duration wants to be told, not quietly given a
    // different question's results.
    let mul = |per: u64| {
        n.checked_mul(per)
            .with_context(|| format!("duration {s:?} is too large"))
    };
    let secs = match unit {
        "s" => n,
        "m" => mul(60)?,
        "h" | "" => mul(3600)?,
        "d" => mul(86400)?,
        "w" => mul(604800)?,
        other => anyhow::bail!("unknown duration unit {other:?} (use s/m/h/d/w)"),
    };
    Ok(std::time::Duration::from_secs(secs))
}

#[cfg(test)]
mod duration_tests {
    /// Release builds ship with overflow checks off, so an unchecked multiply
    /// would wrap silently there and panic in debug; it must be an error in both.
    #[test]
    fn a_duration_too_large_to_represent_is_refused_not_wrapped() {
        for s in [
            "999999999999999999w",
            "999999999999999999d",
            "18446744073709551615m",
        ] {
            let err = parse_duration(s).unwrap_err().to_string();
            assert!(err.contains("too large"), "{s}: {err}");
        }
    }

    /// The guard must not refuse any window a person could mean.
    #[test]
    fn ordinary_durations_still_parse() {
        assert_eq!(parse_duration("30s").unwrap().as_secs(), 30);
        assert_eq!(parse_duration("4h").unwrap().as_secs(), 4 * 3600);
        assert_eq!(
            parse_duration("100000d").unwrap().as_secs(),
            100_000 * 86400
        );
    }

    use super::parse_duration;

    #[test]
    fn units() {
        assert_eq!(parse_duration("5s").unwrap().as_secs(), 5);
        assert_eq!(parse_duration("30m").unwrap().as_secs(), 1800);
        assert_eq!(parse_duration("6h").unwrap().as_secs(), 21600);
        assert_eq!(parse_duration("3d").unwrap().as_secs(), 259200);
        assert_eq!(parse_duration("2w").unwrap().as_secs(), 1209600);
        assert_eq!(
            parse_duration("6").unwrap().as_secs(),
            21600,
            "bare number is hours"
        );
        assert!(parse_duration("5y").is_err());
        assert!(parse_duration("abc").is_err());
    }
}

fn db_path() -> PathBuf {
    tp_db::default_db_path()
}

/// An owned `Retrieval` for `fl mcp`. Assembly lives in tp-app; this only
/// supplies this binary's identity.
pub(crate) fn retrieval() -> Result<tp_search::Retrieval> {
    Ok(tp_app::build_retrieval(&machine_id()?))
}

pub(crate) fn app() -> Result<tp_app::App> {
    tp_app::App::open(&db_path(), &tp_net::identity::default_key_path())
}

/// This machine's id — the address prefix every `session.id` carries, and the
/// device identity peers verify signatures against. Deliberately one value,
/// and the key fingerprint rather than a random id: a peer that verifies a
/// signature has thereby verified the id, and a human comparing fingerprints
/// out of band is comparing something load-bearing. Safe as a `SessionId`
/// segment: base32 plus `-` grouping never contains `/`.
fn machine_id() -> Result<String> {
    Ok(identity()?.device_id)
}

fn identity() -> Result<tp_net::Identity> {
    tp_net::Identity::load_or_create(&tp_net::identity::default_key_path())
        .context("load or create device identity")
}

fn hostname() -> String {
    tp_net::identity::hostname()
}

/// Render a stored timestamp (unix ms) in the local timezone.
/// `from_timestamp_millis` yields UTC, which formatted without a marker is
/// indistinguishable from local time. Local without a marker is deliberate,
/// matching `ls -l`: the timestamps are cross-referenced against the user's
/// own clock, and a zone suffix on every row is noise.
fn fmt_ts(ts: Option<tp_core::Millis>) -> String {
    match ts {
        Some(ms) => chrono::DateTime::from_timestamp_millis(ms.get())
            .map(|dt| {
                dt.with_timezone(&chrono::Local)
                    .format("%Y-%m-%d %H:%M:%S")
                    .to_string()
            })
            .unwrap_or_else(|| "-".to_string()),
        None => "-".to_string(),
    }
}

#[cfg(test)]
mod readme_tests {
    use super::{window_scope, Cli};
    use clap::Parser;

    /// Every `fl …` line in the README, enumerated from the file so no
    /// judgement call decides which flag combinations are worth trying.
    fn readme_cli_commands() -> Vec<Vec<String>> {
        let readme = std::fs::read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../README.md"),
        )
        .expect("README.md");
        readme
            .lines()
            .map(str::trim)
            .filter(|l| l.starts_with("fl ") && !l.contains('|'))
            .map(|l| {
                // Drop trailing `# comment`, then split on whitespace.
                let cmd = l.split('#').next().unwrap_or(l).trim();
                shell_words(cmd)
            })
            .collect()
    }

    /// Minimal splitter: honours double quotes, which is all the README uses.
    fn shell_words(s: &str) -> Vec<String> {
        let mut out = Vec::new();
        let mut cur = String::new();
        let mut in_q = false;
        for c in s.chars() {
            match c {
                '"' => in_q = !in_q,
                c if c.is_whitespace() && !in_q => {
                    if !cur.is_empty() {
                        out.push(std::mem::take(&mut cur));
                    }
                }
                c => cur.push(c),
            }
        }
        if !cur.is_empty() {
            out.push(cur);
        }
        out
    }

    #[test]
    fn every_readme_example_parses() {
        let cmds = readme_cli_commands();
        assert!(cmds.len() >= 8, "expected README examples, found {cmds:?}");
        for argv in cmds {
            if let Err(e) = Cli::try_parse_from(&argv) {
                panic!("README example does not parse: `{}`\n{e}", argv.join(" "));
            }
        }
    }

    /// Parsing is not enough: a time bound parses as a String and is converted
    /// later, so anything the README spells as one must survive the conversion
    /// the command performs.
    #[test]
    fn readme_time_bounds_convert() {
        for argv in readme_cli_commands() {
            let val = |flag: &str| {
                argv.iter()
                    .position(|a| a == flag)
                    .and_then(|i| argv.get(i + 1))
                    .cloned()
            };
            let (Some(since), until) = (val("--since"), val("--until")) else {
                continue;
            };
            window_scope(None, &since, until.as_deref()).unwrap_or_else(|e| {
                panic!("README bound rejected: --since {since} --until {until:?}\n{e}")
            });
        }
    }
}

#[cfg(test)]
mod time_bound_tests {
    use super::parse_time_bound;

    const NOW: tp_core::Millis = tp_core::Millis::new(1_786_437_984_823);

    #[test]
    fn duration_is_relative_to_now() {
        assert_eq!(
            parse_time_bound("4h", NOW).unwrap(),
            NOW.get() - 4 * 3_600_000
        );
        assert_eq!(
            parse_time_bound("2d", NOW).unwrap(),
            NOW.get() - 2 * 86_400_000
        );
    }

    /// A bare date means local midnight, the zone the timestamps are printed
    /// in: a date typed after reading the output selects the day the reader saw.
    #[test]
    fn a_date_is_local_midnight() {
        use chrono::TimeZone;
        let got = parse_time_bound("2026-08-04", NOW).unwrap();
        let want = chrono::Local
            .with_ymd_and_hms(2026, 8, 4, 0, 0, 0)
            .unwrap()
            .timestamp_millis();
        assert_eq!(got, want);
    }

    /// A day is a range: without an upper bound a quiet day returns an earlier
    /// busy day's turns and looks like an answer.
    #[test]
    fn a_day_is_a_bounded_range() {
        let start = parse_time_bound("2026-08-04", NOW).unwrap();
        let end = parse_time_bound("2026-08-05", NOW).unwrap();
        assert_eq!(end - start, 86_400_000, "one day apart");
        assert!(start < end);
    }

    #[test]
    fn wall_clock_and_epoch_ms_both_parse() {
        let day = parse_time_bound("2026-08-04", NOW).unwrap();
        let noon = parse_time_bound("2026-08-04T12:00", NOW).unwrap();
        assert_eq!(noon - day, 12 * 3_600_000);
        assert_eq!(parse_time_bound("1786437984823", NOW).unwrap(), NOW.get());
    }

    /// The digit-length rule is what keeps a duration from being read as an
    /// epoch.
    #[test]
    fn short_digits_are_not_mistaken_for_epoch_ms() {
        // "6" with no unit is hours, not 6ms.
        assert_eq!(
            parse_time_bound("6", NOW).unwrap(),
            NOW.get() - 6 * 3_600_000
        );
    }

    #[test]
    fn garbage_is_an_error_not_a_silent_default() {
        assert!(parse_time_bound("last tuesday", NOW).is_err());
        assert!(parse_time_bound("2026-13-99", NOW).is_err());
    }
}

#[cfg(test)]
mod fmt_ts_tests {
    use super::fmt_ts;

    /// Rendering follows the machine's clock, not UTC. The expectation is
    /// computed locally rather than hardcoded so this holds in any zone,
    /// including a UTC runner where the two coincide.
    #[test]
    fn renders_in_local_time_not_utc() {
        let ms = 1_786_150_093_693i64;
        let expected = chrono::DateTime::from_timestamp_millis(ms)
            .unwrap()
            .with_timezone(&chrono::Local)
            .format("%Y-%m-%d %H:%M:%S")
            .to_string();
        assert_eq!(fmt_ts(Some(tp_core::Millis::new(ms))), expected);

        // On a machine offset from UTC the two must differ, or the test would
        // pass against a UTC renderer too.
        let utc = chrono::DateTime::from_timestamp_millis(ms)
            .unwrap()
            .format("%Y-%m-%d %H:%M:%S")
            .to_string();
        let offset_secs = chrono::Local::now().offset().local_minus_utc();
        if offset_secs != 0 {
            assert_ne!(
                fmt_ts(Some(tp_core::Millis::new(ms))),
                utc,
                "off-UTC machine must not render UTC"
            );
        }
    }

    #[test]
    fn missing_timestamp_is_a_dash() {
        assert_eq!(fmt_ts(None), "-");
    }
}

fn main() -> Result<()> {
    // Before any output: a CLI whose stdout is piped into `head` must exit,
    // not panic. Rust ignores SIGPIPE by default and turns the failed write
    // into a panic; this hands the decision back to the kernel.
    tp_core::exit_quietly_on_broken_pipe();
    // Names this process in any log line a library emits. The CLI's own output
    // stays `println!` on stdout — that is the product, not logging.
    tp_core::logging::set_service("tp");
    let cli = Cli::parse();
    match cli.cmd {
        Command::Search {
            query,
            include_thinking,
            folder,
            since,
            until,
            regex,
            limit,
            all,
            peers,
        } => run_search(
            &query,
            include_thinking,
            folder,
            &since,
            until.as_deref(),
            regex,
            limit,
            all,
            &peers,
        ),
        Command::Live => run_live(),
        Command::Listen { state, port } => cmd::net::run_listen(state.as_deref(), port),
        Command::Id => run_id(),
        Command::Peers => run_peers(),
        Command::Discover { host } => run_discover(&host),
        Command::Pair(p) => match p {
            PairCmd::Request { addr } => run_pair_request(&addr),
            PairCmd::List => run_pair_list(),
            PairCmd::Approve { device_id } => run_pair_decide(&device_id, true),
            PairCmd::Reject { device_id } => run_pair_decide(&device_id, false),
            PairCmd::Revoke { device_id } => run_pair_revoke(&device_id),
        },
        Command::Sessions {
            folder,
            since,
            until,
            limit,
        } => run_sessions(folder, &since, until.as_deref(), limit),
        Command::Turns {
            session_id,
            after_ts,
            since,
            until,
            folder,
            include_thinking,
            limit,
        } => run_turns(
            session_id,
            after_ts,
            since,
            until,
            folder,
            include_thinking,
            limit,
        ),
        Command::Ask {
            session_id,
            message,
            no_wake,
            from_session,
            runtime,
        } => run_ask(
            &session_id,
            &message,
            no_wake,
            from_session.as_deref(),
            &runtime,
            tp_app::Kind::Ask,
        ),
        Command::Note {
            session_id,
            message,
            no_wake,
            from_session,
            runtime,
        } => run_ask(
            &session_id,
            &message,
            no_wake,
            from_session.as_deref(),
            &runtime,
            tp_app::Kind::Note,
        ),
        Command::Reply {
            message_id,
            message,
            no_wake,
            from_session,
            runtime,
        } => run_reply(
            &message_id,
            &message,
            no_wake,
            from_session.as_deref(),
            &runtime,
        ),
        Command::Inbox {
            session_id,
            runtime,
            pending,
            history,
            since,
        } => run_inbox(session_id, &runtime, pending, history, &since),
        Command::Ack { message_id } => run_ack(&message_id),
        Command::Type { tty, message } => run_type(&tty, &message),
        Command::Register {
            session_id,
            cwd,
            from_hook,
            runtime,
            presence,
            deliver,
            pid,
        } => run_register(
            session_id,
            cwd,
            from_hook,
            runtime,
            &presence,
            deliver.as_deref(),
            pid,
        ),
        Command::Heartbeat {
            session_id,
            runtime,
        } => run_heartbeat(&session_id, &runtime),
        Command::Unregister {
            session_id,
            from_hook,
            runtime,
        } => run_unregister(session_id, from_hook, runtime),
        Command::Mcp => mcp::serve(),
        Command::Version => run_version(),
        Command::Rollback => cmd::net::run_rollback(),
        Command::Verify { full } => run_verify(full),
        Command::Backup { dest } => run_backup(&dest),
    }
}
