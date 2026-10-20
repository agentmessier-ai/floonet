#![allow(clippy::print_stdout, clippy::print_stderr)]
//! Exempt from the workspace print lints: this module's output is the product.
//! See [workspace.lints.clippy] in the root Cargo.toml for why the lint exists
//! everywhere else.

//! Retrieval commands: index/reindex, search (local and fan-out), sessions,
//! turns — everything that answers "what happened".

use crate::{app, fmt_ts, parse_time_bound};
use anyhow::Result;
use tp_core::retrieval::{Query, Scope, TurnCursor};
use tp_search::Retrieval;

/// Whether the daemon `daemon_status` last recorded is still running. The
/// command name is checked, not just the pid: `daemon_status` keeps the last
/// start forever, so a recycled pid would otherwise count. Both names, because
/// a pre-rename `tpd` still running is exactly the case worth refusing over.
pub(crate) fn daemon_is_live(pid: i64) -> bool {
    std::process::Command::new("ps")
        .args(["-p", &pid.to_string(), "-o", "comm="])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .is_some_and(|o| {
            let out = String::from_utf8_lossy(&o.stdout);
            let name = out.trim();
            name.ends_with("fld") || name.ends_with("tpd")
        })
}

/// What a scan cannot read in this window, printed next to coverage because it
/// is the same kind of fact: the answer is incomplete, and here is why. Silent
/// when there is no index — a machine with no daemon has nothing to be missing.
pub(crate) fn print_unscannable(r: &Retrieval, scope: &Scope) {
    let Ok(app) = app() else {
        return;
    };
    if let Some(note) = app.unscannable_note(r.capabilities().reports_unscannable, scope) {
        eprintln!("[coverage] {note}");
    }
}

pub(crate) fn print_coverage(c: &tp_core::Coverage) {
    let mut notes = Vec::new();
    if c.truncated {
        notes.push("results truncated by --limit".to_string());
    }
    if let Some(d) = &c.degraded {
        notes.push(d.clone());
    }
    if !notes.is_empty() {
        eprintln!("[coverage] {}", notes.join(" · "));
    }
}

#[allow(clippy::too_many_arguments)] // mirrors the clap command's fields 1:1
pub(crate) fn run_search(
    query: &str,
    include_thinking: bool,
    folder: Option<String>,
    since: &str,
    until: Option<&str>,
    regex: bool,
    limit: usize,
    all: bool,
    only: &[String],
) -> Result<()> {
    let app = app()?;
    let r = app.retrieval();
    let r = r.as_ref();
    let scope = window_scope(folder, since, until)?;
    if let Some(w) = r.scope_warning(&scope) {
        eprintln!("[warn] {w}");
    }
    let q = Query {
        text: query.to_string(),
        regex,
        include_thinking,
        limit,
    };
    let got = tp_app::read::search(r, &q, &scope)?;

    if got.items.is_empty() && !all {
        // Both bounds are shown: a lone date reads as "since that day", which
        // may be the opposite of what was asked.
        let window = match until {
            Some(u) => format!("{since} … {u}"),
            None => format!("{since} … now"),
        };
        println!(
            "no matches for {query:?} (provider: {}, window: {window})",
            r.provider_name()
        );
        // A window the caller never chose has to announce itself when it comes
        // up empty; a silent default bound reads as "never discussed anywhere".
        // The phrasing note comes first because it is the more specific answer:
        // if the phrasing excluded everything, widening the window will not help.
        if let Some(note) = tp_app::read::empty_note(&q, got.items.len()) {
            println!("  {note}");
        }
        if since == DEFAULT_SEARCH_SINCE && until.is_none() {
            println!(
                "  This is the DEFAULT {DEFAULT_SEARCH_SINCE} window, not an exhaustive search — \
                 anything older was never looked at.\n  \
                 Widen before concluding it was never discussed: --since 30d, or --since <date>."
            );
        }
        // A shortfall is the same trap on a different axis: a clean "no
        // matches" while part of the corpus was never consulted. A number in
        // the coverage footer does not undo a negative conclusion stated above
        // it, so it is said in words.
        if let Some(d) = &got.coverage.degraded {
            println!(
                "  NOT an exhaustive search — {d}. The rest was never read, \
                 so this is not evidence it is not there."
            );
        }
    }
    for h in &got.items {
        // The same two marks `fl turns` prints, for the same reasons: whose
        // words matched, and whether they are still context.
        let side = if h.sidechain { " [subagent]" } else { "" };
        let dead = if h.surface == tp_core::turn::Surface::Superseded {
            " [superseded]"
        } else {
            ""
        };
        println!(
            "{}  [{:?}]{side}{dead}  {}",
            fmt_ts(h.at.ts.map(tp_core::Millis::new)),
            h.role,
            h.at.session_id
        );
        println!("    {}", h.excerpt().replace('\n', " "));
    }
    print_coverage(&got.coverage);
    print_unscannable(r, &scope);
    if all || !only.is_empty() {
        run_search_all(&q, &scope, since, &got, only)?;
    }
    Ok(())
}

/// `--since`/`--until` as a `Scope`. `Scope::since` is a duration back from
/// now, so an absolute `--since` is converted to "how long ago" here, in one
/// place, so a date means the same instant to every command.
pub(crate) fn window_scope(
    folder: Option<String>,
    since: &str,
    until: Option<&str>,
) -> Result<Scope> {
    let now = tp_core::now_ms();
    let since_ms = parse_time_bound(since, now)?;
    Ok(Scope {
        folder,
        since: std::time::Duration::from_millis((now.get() - since_ms).max(0) as u64),
        runtimes: vec![],
        until: until.map(|u| parse_time_bound(u, now)).transpose()?,
    })
}

pub(crate) fn run_sessions(
    folder: Option<String>,
    since: &str,
    until: Option<&str>,
    limit: usize,
) -> Result<()> {
    let app = app()?;
    let r = app.retrieval();
    let r = r.as_ref();
    let scope = window_scope(folder, since, until)?;
    let got = tp_app::read::sessions(r, &scope, limit)?;
    if got.items.is_empty() {
        println!(
            "no sessions in the last {since} (provider: {})",
            r.provider_name()
        );
    }
    for s in &got.items {
        let turns = s
            .turn_count
            .map(|n| format!("{n:>5} turns"))
            .unwrap_or_else(|| "     ? turns".to_string());
        println!(
            "{}  {}  {}  {}",
            fmt_ts(s.last_turn_at.map(tp_core::Millis::new)),
            turns,
            s.cwd.clone().unwrap_or_else(|| "-".to_string()),
            s.id
        );
        if let Some(title) = &s.title {
            println!("    {}", title.replace('\n', " "));
        }
    }
    print_coverage(&got.coverage);
    print_unscannable(r, &scope);
    Ok(())
}

/// One turn's body line. Most records in a coding session are tool calls and
/// results with no text; every branch emits something and names which case it
/// is, because a blank line is indistinguishable from an adapter that failed
/// to parse the record, and those want opposite responses from the reader.
pub(crate) fn turn_body(t: &tp_core::turn::NormalizedTurn, include_thinking: bool) -> String {
    if !t.text.is_empty() {
        return t.text.replace('\n', " ");
    }
    if !t.tool_calls.is_empty() {
        let names: Vec<&str> = t.tool_calls.iter().map(|c| c.name.as_str()).collect();
        return format!("({})", names.join(", "));
    }
    if !t.thinking.is_empty() && !include_thinking {
        return "(thinking — pass --include-thinking to show)".to_string();
    }
    // Not gated on `include_thinking`: there is no payload the flag would
    // reveal, and the fallthrough line would claim no reasoning happened,
    // which is the claim `thinking_opaque` exists to prevent.
    if t.thinking_opaque {
        return "(reasoning happened but is encrypted by the runtime — nothing to show)"
            .to_string();
    }
    // Usually a `tool_result`, whose body is deliberately not stored, or a
    // signature-only `thinking` block.
    "(no indexed content — tool result or non-text record)".to_string()
}

/// Pick the session to read when the caller didn't name one: for "what
/// happened in the last few hours" the id is the question, not the input. The
/// most recently active session in the window wins, and the pick is announced
/// with how many others matched — a silent pick among several looks like one.
pub(crate) fn resolve_session(
    r: &Retrieval,
    folder: Option<String>,
    since: &str,
    until: Option<&str>,
) -> Result<String> {
    // The same window the turns will be read with, `until` included: a
    // candidate chosen through a wider window than it is read through can
    // come back empty for a question that had an answer.
    let scope = window_scope(folder.clone(), since, until)?;
    let got = tp_app::read::sessions(r, &scope, 20)?;
    let where_ = folder
        .as_ref()
        .map(|f| format!(" under {f:?}"))
        .unwrap_or_default();
    let window = match until {
        Some(u) => format!("between {since} and {u}"),
        None => format!("in the last {since}"),
    };
    let Some(first) = got.items.first() else {
        anyhow::bail!("no sessions active {window}{where_} — widen --since, or name a session id");
    };

    // Auto-pick needs a narrowing signal: with no folder and several matches,
    // reading one arbitrary session would answer a different question than
    // was asked, so it refuses and shows what to choose from.
    if got.items.len() > 1 && folder.is_none() {
        let mut msg = format!(
            "{} sessions were active {window} — reading one of them would answer a \
             different question than you asked.\nNarrow with --folder, or name one:\n",
            got.items.len()
        );
        for s in got.items.iter().take(8) {
            msg.push_str(&format!(
                "  {}  {}\n",
                s.id,
                s.cwd.as_deref().unwrap_or("(unknown cwd)")
            ));
        }
        if got.items.len() > 8 {
            msg.push_str(&format!("  … and {} more\n", got.items.len() - 8));
        }
        anyhow::bail!(msg);
    }
    if got.items.len() > 1 {
        eprintln!(
            "[note] {} sessions matched; reading the most recent. Others:",
            got.items.len()
        );
        for s in got.items.iter().skip(1).take(4) {
            eprintln!(
                "         {}  {}",
                s.id,
                s.cwd.as_deref().unwrap_or("(unknown cwd)")
            );
        }
    }
    Ok(first.id.clone())
}

#[allow(clippy::too_many_arguments)] // mirrors the clap command's fields 1:1
pub(crate) fn run_turns(
    session_id: Option<String>,
    after_ts: Option<i64>,
    since: Option<String>,
    until: Option<String>,
    folder: Option<String>,
    include_thinking: bool,
    limit: usize,
) -> Result<()> {
    let app = app()?;
    let r = app.retrieval();
    let r = r.as_ref();
    let session_id = match session_id {
        Some(s) => s,
        None => {
            let Some(since) = since.as_deref() else {
                anyhow::bail!(
                    "give a session id, or --since (e.g. --since 4h) to read the most recent session"
                );
            };
            resolve_session(r, folder, since, until.as_deref())?
        }
    };
    let session_id = session_id.as_str();
    let now = tp_core::now_ms();
    let cursor = match (&since, after_ts) {
        (Some(d), _) => TurnCursor::Window {
            since_ms: parse_time_bound(d, now)?,
            before_ms: until
                .as_deref()
                .map(|u| parse_time_bound(u, now))
                .transpose()?,
        },
        (None, Some(ts)) => TurnCursor::AfterTs(ts),
        (None, None) => TurnCursor::Start,
    };
    let got = tp_app::read::turns(r, session_id, cursor, include_thinking, limit, None)?;
    if got.items.is_empty() {
        // Why the answer is empty comes before the answer: an empty read is
        // the one case where "could not read this" and "nothing here" differ
        // in meaning. stderr, matching `print_coverage`: the note is about the
        // answer, not part of it.
        if let Some(d) = &got.coverage.degraded {
            eprintln!("[coverage] {d}");
        }
        // A `scan-pid-N` address can never have turns, and "none found" invites
        // the caller to conclude the session is idle and go looking for a
        // better id. Checked before the window arms: "no turns since 2h" is
        // even more convincing, and even more wrong.
        if session_id
            .rsplit('/')
            .next()
            .is_some_and(|native| native.starts_with("scan-pid-"))
        {
            println!(
                "{session_id:?} is a scan PLACEHOLDER, not a session id — floonet found this \
                 process running but it never registered, so there is nothing to read here. This \
                 says NOTHING about whether the session has turns.\n\
                 \n\
                 A Claude Code or codex session registers once at startup and never again. If \
                 floonet's database was recreated while it was running (uninstall, reinstall, a \
                 manual delete), the registration is gone permanently. Restart that session to \
                 restore it; until then it can be messaged but not read."
            );
        } else {
            match (&since, &until) {
                (Some(d), Some(u)) => println!("no turns between {d} and {u} for {session_id:?}"),
                (Some(d), None) => println!("no turns since {d} for {session_id:?}"),
                _ => println!("no turns found for {session_id:?}"),
            }
        }
    }
    let last_ts = got.items.last().and_then(|t| t.ts);
    for t in &got.items {
        // A subagent's turn is not the operator's. Marked rather than filtered:
        // the content is real work, it just was not said by the session's owner.
        let side = if t.prov.sidechain { " [subagent]" } else { "" };
        // Superseded turns stay in the output as real history, but reading one
        // as live context is what the surface column exists to stop. `Unknown`
        // prints nothing: for a human it is noise, and the MCP surface, where
        // an agent acts on the difference, carries it.
        let dead = if t.surface == tp_core::turn::Surface::Superseded {
            " [superseded]"
        } else {
            ""
        };
        println!(
            "{} [{:?}]{side}{dead} {}",
            fmt_ts(t.ts.map(tp_core::Millis::new)),
            t.role,
            turn_body(t, include_thinking)
        );
        if include_thinking && !t.thinking.is_empty() {
            println!("    thinking: {}", t.thinking.replace('\n', " "));
        }
    }
    // Print the resume cursor, not just the fact of truncation.
    if got.coverage.truncated {
        // The hint matches the direction read: a window read kept the newest
        // turns, so what is missing is older than the first one returned, and
        // `--after-ts` would page away from it.
        let first_ts = got.items.first().and_then(|t| t.ts);
        match (&since, first_ts, last_ts) {
            (Some(d), Some(ts), _) => println!(
                "[truncated] kept the newest turns in the window — page BACK with: fl turns {session_id} --since {d} --until {ts}"
            ),
            (None, _, Some(ts)) => println!(
                "[truncated] stopped at the turn/byte budget — resume with: fl turns {session_id} --after-ts {ts}"
            ),
            _ => println!("[truncated] stopped at the turn/byte budget"),
        }
    }
    Ok(())
}

/// Local hits plus the selected peers', with failures always surfaced. `only`
/// empty means `--all`: every trusted peer; non-empty names them, which works
/// at any count. Takes the built `Query` and `Scope` rather than loose fields
/// so the peer half cannot be narrowed differently from the local half.
pub(crate) fn run_search_all(
    q: &tp_core::retrieval::Query,
    scope: &tp_core::retrieval::Scope,
    since: &str,
    local: &tp_core::Retrieved<tp_core::Hit>,
    only: &[String],
) -> Result<()> {
    let app = app()?;
    let me = app.identity().clone();
    let peers = match app.fanout_select(only)? {
        tp_app::Fanout::Ready { peers, no_address } => {
            for name in &no_address {
                eprintln!("[warn] {name} was not queried — no address; `fl discover` or re-pair");
            }
            peers
        }
        tp_app::Fanout::NothingReachable { no_address } => {
            for name in &no_address {
                eprintln!("[warn] {name} is trusted but has no address — `fl discover` or re-pair");
            }
            eprintln!("[warn] no reachable trusted peers; showing local results only");
            return Ok(());
        }
        // An error, not local-only results: the caller asked a named machine a
        // question, and silence is not an answer from it.
        tp_app::Fanout::NoneUsable {
            unmatched,
            without_address,
        } => {
            let mut msg = String::from("none of the peers you named can be searched.");
            for name in &without_address {
                msg.push_str(&format!(
                    "\n  {name} is trusted but has no address — run `fl discover` or re-pair. \
                     Retyping the name will not help."
                ));
            }
            for want in &unmatched {
                msg.push_str(&format!(
                    "\n  no trusted peer matches {want:?} — `fl peers` lists them."
                ));
            }
            anyhow::bail!(msg);
        }
        tp_app::Fanout::Ambiguous { want, matched } => anyhow::bail!(
            "{want:?} matches {} peers ({}) — use more of the id",
            matched.len(),
            matched.join(", ")
        ),
        tp_app::Fanout::TooMany { reachable } => anyhow::bail!(
            "--all would query {reachable} trusted peers, and each answers by scanning its whole \
             corpus.\nName the ones you mean instead — `fl peers` lists them, and `--peer <id>` \
             is repeatable and works at any number."
        ),
    };

    // `parse_time_bound`, not `parse_duration`: the peer half must accept the
    // same absolute dates the local half does.
    let since_ms = tp_core::now_ms().get() - parse_time_bound(since, tp_core::now_ms())?;
    let narrow = tp_net::PeerQuery {
        regex: q.regex,
        include_thinking: q.include_thinking,
        folder: scope.folder.clone(),
        until_ms: scope.until,
        runtimes: scope.runtimes.clone(),
    };
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    let fan = rt.block_on(tp_net::query_peers(
        &me, &peers, &q.text, since_ms, q.limit, &narrow,
    ))?;
    let merged = tp_app::fanout::merge(&me.device_id, &peers, local, fan);

    println!("\n── peers ──");
    for r in &merged.remote {
        let side = if r.hit.sidechain { " [subagent]" } else { "" };
        let dead = if r.hit.surface == tp_core::turn::Surface::Superseded {
            " [superseded]"
        } else {
            ""
        };
        println!(
            "{}  [{}]{side}{dead}  {}  {}",
            fmt_ts(r.hit.ts.map(tp_core::Millis::new)),
            r.hit.role,
            r.machine,
            r.hit.session_id
        );
        println!("    {}", r.hit.excerpt.replace('\n', " "));
    }
    if let Some(d) = merged.degraded {
        eprintln!("[coverage] {d}");
    }
    Ok(())
}

/// The `--since` a caller gets without asking. Named so the no-match path can
/// tell "you chose this window" from "we chose it for you".
pub(crate) const DEFAULT_SEARCH_SINCE: &str = "6h";
