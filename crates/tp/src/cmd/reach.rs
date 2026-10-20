#![allow(clippy::print_stdout, clippy::print_stderr)]
//! Exempt from the workspace print lints: this module's output is the product.
//! See [workspace.lints.clippy] in the root Cargo.toml for why the lint exists
//! everywhere else.

//! Reach commands: the session registry (register/heartbeat/unregister, hook
//! plumbing) and messaging (ask/reply/inbox/ack) — everything that talks TO a
//! session rather than about one.

use crate::{app, fmt_ts, machine_id, parse_duration};
use anyhow::Context as _;
use anyhow::Result;

/// This machine's composite id for a session, matching what search and ingest
/// produce for the same session: register, unregister and inbox must compose
/// the same id from a bare native id, or an id copied out of a search result
/// never resolves to a live binding. `runtime` is a parameter because each
/// harness composes under its own id.
pub(crate) fn live_session_id(native_id: &str, runtime: &str) -> Result<String> {
    Ok(tp_core::SessionId::new(machine_id()?, runtime, native_id).to_string())
}

/// The runtime to register a session under: what the caller said, or what
/// this process is running inside. A harness that registers via `--from-hook`
/// without naming itself must not be composed under another runtime's id —
/// the address would be undeliverable, and `Host::Inferred` would walk the
/// process tree for the wrong needle. An explicit value always wins;
/// detection is the fallback, never an override.
fn runtime_for_registration(stated: Option<String>) -> String {
    if let Some(r) = stated {
        return r;
    }
    tp_reach::resolve::runtime_of_host(
        std::process::id() as i32,
        &tp_ingest::adapter::process_signatures(),
    )
    // Nothing in the chain declared a `process_match`: a harness floonet
    // cannot identify still registers rather than being refused.
    .unwrap_or_else(|| "claude_code".to_string())
}

/// Accept either a bare native id (composed under `runtime`) or an already
/// composite `<machine>/<runtime>/<native>` id (used as-is). Native ids never
/// contain the two `/` separators, so `SessionId::parse` succeeding is the
/// check.
pub(crate) fn resolve_session_id(raw: &str, runtime: &str) -> Result<String> {
    if tp_core::SessionId::parse(raw).is_some() {
        Ok(raw.to_string())
    } else {
        live_session_id(raw, runtime)
    }
}

/// The process-image name to search for when walking up from `tp`'s own pid
/// to the session process that spawned it (`find_session_process`). Falls
/// back to the runtime id itself, which is right for extensions whose process
/// name is their runtime id.
pub(crate) fn ancestor_needle(runtime: &str) -> String {
    // The same descriptor table `recognize_runtime` reads, so the two
    // directions of "what does this runtime's process look like" cannot
    // disagree. A `=`-anchored pattern is an exact `comm`; the walk
    // substring-matches either way, so the anchor is stripped.
    tp_ingest::adapter::process_signature_for(runtime)
        .map(|p| p.trim_start_matches('=').to_string())
        .unwrap_or_else(|| runtime.to_string())
}

/// A hook event's JSON payload, read off stdin: hook commands receive event
/// data only this way, never as env vars.
pub(crate) struct HookEvent {
    session_id: String,
    cwd: Option<String>,
    /// Where the harness is writing this session. Used to ask the transcript
    /// what kind of session it is before registering it as a correspondent.
    transcript_path: Option<String>,
    /// How this session began — `startup`, `resume`, `clear`, `compact` or
    /// `fork`. Recorded in the breadcrumb rather than acted on: a headless
    /// sub-conversation is a new session and arrives as `startup` like any
    /// other, so this field cannot distinguish it.
    source: Option<String>,
}

pub(crate) fn read_hook_event() -> Result<HookEvent> {
    let mut buf = String::new();
    std::io::Read::read_to_string(&mut std::io::stdin(), &mut buf)
        .context("reading hook JSON from stdin")?;
    let v: serde_json::Value =
        serde_json::from_str(&buf).context("hook stdin was not valid JSON")?;
    let session_id = v
        .get("session_id")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| anyhow::anyhow!("hook JSON has no `session_id` field"))?
        .to_string();
    let cwd = v
        .get("cwd")
        .and_then(serde_json::Value::as_str)
        .map(str::to_string);
    let transcript_path = v
        .get("transcript_path")
        .and_then(serde_json::Value::as_str)
        .map(str::to_string);
    let source = v
        .get("source")
        .and_then(serde_json::Value::as_str)
        .map(str::to_string);
    Ok(HookEvent {
        session_id,
        cwd,
        transcript_path,
        source,
    })
}

/// The `type` a transcript declares on its first line, if readable. Best-effort
/// by necessity: this runs inside a hook, on a file the harness may still be
/// creating. Every failure means "cannot tell", and the caller then registers —
/// refusing would trade a stale row for an unreachable session, which is worse.
fn transcript_kind(path: &str) -> Option<String> {
    use std::io::BufRead as _;
    let file = std::fs::File::open(path).ok()?;
    let mut first = String::new();
    std::io::BufReader::new(file).read_line(&mut first).ok()?;
    serde_json::from_str::<serde_json::Value>(&first)
        .ok()?
        .get("type")?
        .as_str()
        .map(str::to_string)
}

/// This session's own composite id — the return address stamped onto every
/// message it sends. Without it a recipient can only guess an address to
/// answer, and a guessed address is accepted and never delivered.
/// `--from-session` is how a runtime extension that knows its own id supplies
/// it; `None` is tolerated (a human running `fl ask` has no session) but
/// costs the recipient the ability to reply, so `run_ask` says so.
pub(crate) fn sender_session_id(from_session: Option<&str>, runtime: &str) -> Option<String> {
    if let Some(raw) = from_session {
        return resolve_session_id(raw, runtime).ok();
    }
    own_session_id()
}

/// The return address to stamp on an outgoing message. Prefers the sender's
/// conversation address: a reply is written minutes or hours later, by which
/// time the sender may have compacted and its segment id may belong to nobody.
/// Falls back to the segment id when there is no conversation yet, so the
/// stamp is never worse than the session id.
pub(crate) fn sender_address(
    app: &tp_app::App,
    from_session: Option<&str>,
    runtime: &str,
) -> Option<String> {
    let sid = sender_session_id(from_session, runtime)?;
    // The pane's most recently seen conversation, not the one this session
    // happens to belong to: a pane can own several, and the one this session
    // joined may be a twin nobody is registered under, so a reply to it would
    // be stored for a mailbox the live window never reads.
    // `conversations_of_pane` orders by `last_seen_at DESC`, so the head is
    // live by construction.
    match app.conversations_of_pane(&sid) {
        Ok(convs) if !convs.is_empty() => Some(convs[0].clone()),
        _ => Some(sid),
    }
}

/// Which session this process belongs to, with the reason when there is none.
/// Separate from `own_session_id` because the CLI needs only the happy path,
/// while a tool result has to tell a model whether to pick from candidates or
/// to check its daemon.
pub(crate) fn own_session() -> Result<tp_reach::OwnSession> {
    app()?.own_session(std::process::id() as i32)
}

/// This process's own session, sources in order of trust: the registry by pid
/// first, because walking up to a registered ancestor yields the id `fl live`
/// publishes and other sessions address, for every runtime; then
/// `$CLAUDE_CODE_SESSION_ID`, which can disagree with the registry after a
/// `--resume` but is the only source when nothing is registered.
///
/// Ambiguity ends the chain rather than continuing down it. Several sessions
/// on one pid means the registry cannot say which is ours, and the environment
/// variable is not a tie-breaker for that question: it can name any of them, or
/// none. Returning nothing costs this sender its reply path, which `run_ask`
/// reports; guessing costs the RECIPIENT its reply path, silently, because a
/// reply addressed to the wrong one of several live sessions is accepted and
/// read by nobody.
pub(crate) fn own_session_id() -> Option<String> {
    if let Ok(app) = app() {
        match app.own_session(std::process::id() as i32) {
            Ok(tp_reach::OwnSession::Resolved(sid)) => return Some(sid),
            Ok(tp_reach::OwnSession::Ambiguous(_)) => return None,
            Ok(tp_reach::OwnSession::Unknown) | Err(_) => {}
        }
    }
    let native = std::env::var("CLAUDE_CODE_SESSION_ID").ok()?;
    live_session_id(&native, "claude_code").ok()
}

pub(crate) fn run_ask(
    session_id: &str,
    message: &str,
    no_wake: bool,
    from_session: Option<&str>,
    runtime: &str,
    kind: tp_app::Kind,
) -> Result<()> {
    let app = app()?;
    app.ensure_self_machine()?;
    let from = sender_address(&app, from_session, runtime);
    let sent = app.send(session_id, message, kind, from)?;

    // A note carries a return address too, but nothing about it asks for one.
    // `sent.kind`, not the parameter: the stored kind is what a reader sees.
    if sent.from.is_none() && sent.kind.expects_reply() {
        // Never silent: a one-way message looks identical to a two-way one at
        // the call site, and the difference shows up only as an answer that
        // never comes.
        eprintln!("[warn] no return address on this message — the target cannot reply to it. Pass --from-session <your session id> if you expect an answer.");
    }

    // The anti-polling hint is only honest when a reply can come back, and
    // there are two independent ways it cannot: no return address, or nothing
    // on the other end to read the message. `deliverable` is asked of `Sent`
    // rather than re-derived here so the CLI and MCP share one rule.
    let deliverable = sent.deliverable();
    // Bound outside the match so the one arm that formats its hint can be
    // borrowed alongside the static ones.
    let unknown_kind_hint;
    let hint: &str = match (&sent.kind, sent.from.is_some(), deliverable) {
        // Undeliverable outranks everything: the kind of a message nobody
        // drains does not matter.
        (_, _, false) => "Do NOT wait for an answer: nothing is currently registered to read this mailbox, so no reply can come back until something registers under this exact id. Find the live address with `fl live` and send again.",
        (tp_app::Kind::Note, _, true) => "This is a NOTE: the target is told it does not need to answer. Do not wait for a reply, and do not send the same thing again as an `ask` to get one.",
        (tp_app::Kind::Ask, true, true) => "This does NOT wait for an answer. The reply arrives later as a `/fl inbox` wake that resumes you — end your turn and say you're waiting. Do not `sleep`-poll for it; that delays nothing but you.",
        (tp_app::Kind::Ask, false, true) => "This does NOT wait, and carries no return address, so no answer can come back. Check the work directly (its output files), not this message.",
        // `fl reply` has its own path and never arrives here. No wildcard
        // anywhere in this match: one would silently absorb the next kind, and
        // the hint is the thing most likely to be wrong for it.
        (tp_app::Kind::Reply, _, _) => unreachable!("replies are sent through run_reply"),
        // A type floonet did not define — an event from CI, a webhook, a job
        // that finished. Not folded into `Note`: a note came from an agent that
        // chose not to ask, while this came from something with no concept of
        // asking, so "the target was told it need not answer" would describe a
        // decision nobody made.
        (tp_app::Kind::Other(t), _, true) => {
            unknown_kind_hint = format!(
                "Delivered as `{t}`, a type floonet does not define. The receiver was woken and \
                 gets it verbatim. Nothing here asks for a reply."
            );
            &unknown_kind_hint
        }
    };
    finish_send(
        &app,
        &sent.target,
        &sent.message_id,
        no_wake,
        "queued",
        Some(hint),
    )
}

/// Answer a message, addressed automatically to whoever sent it.
pub(crate) fn run_reply(
    message_id: &str,
    message: &str,
    no_wake: bool,
    from_session: Option<&str>,
    runtime: &str,
) -> Result<()> {
    let app = app()?;
    app.ensure_self_machine()?;
    let from = sender_address(&app, from_session, runtime);
    let sent = app.reply(message_id, message, from)?;

    // No anti-polling hint here: the replier is finishing an exchange, not
    // starting one, so there is nothing to wait for.
    let hint = if sent.from.is_none() {
        Some("Your answer carries no return address, so this exchange ends here — the sender cannot come back with a follow-up.")
    } else {
        None
    };
    finish_send(
        &app,
        &sent.target,
        &sent.message_id,
        no_wake,
        "replied",
        hint,
    )
}

/// What to tell the caller about an address that could not be woken. Never
/// "delivered on next /fl inbox" unless something is expected to run one. For
/// a dormant address the likeliest cause is named: a Claude Code conversation
/// is issued a new session id at every compaction, so an address that worked
/// an hour ago can belong to no one while the conversation runs on.
pub(crate) fn undeliverable_note(app: &tp_app::App, target: &str) -> Result<String> {
    Ok(match app.addressability(target)? {
        tp_core::Addressability::Registered => {
            "session not injectable right now — delivered on its next /fl inbox".to_string()
        }
        // Every line below says STORED first and what is missing second: a
        // sender that reads non-delivery as rejection resends duplicates.
        tp_core::Addressability::DormantConversation => {
            "STORED — this conversation has no registered session right now, so nobody was woken. \
             It is collected as soon as any segment of that conversation registers again; do not \
             resend"
                .to_string()
        }
        tp_core::Addressability::EndedConversation => {
            "STORED but nothing will drain it — this conversation's host process has exited, and a \
             conversation cannot outlive it: a new session in that same pane forms a NEW \
             conversation, so no segment will ever register into this one again. Do NOT wait for \
             it to be collected. Find the correspondent's current address with `fl live` and send \
             there"
                .to_string()
        }
        tp_core::Addressability::Dormant => {
            "STORED but nothing will drain it — this session is indexed and no longer registered. \
             A Claude Code session id changes at every compaction, so the same conversation is \
             probably live under a different address. Do not resend to this one: find the current \
             address with `fl live`"
                .to_string()
        }
        // Two different failures wear the same `Unknown`, and telling them
        // apart needs the transcript roots, not the database: `fl sessions`
        // lists what is readable and `fl live` what is reachable, and the ids
        // look identical.
        tp_core::Addressability::Unknown {
            transcript_readable,
        } => {
            if transcript_readable {
                "STORED but nothing will drain it — floonet can READ this session's transcript, \
                 but nothing is registered at this address, and only a registered session is \
                 delivered to. Either it is not running, or its runtime does not register with \
                 floonet. `fl sessions` lists what is readable; `fl live` lists what can be \
                 written to. Send to an address from `fl live`"
                    .to_string()
            } else {
                "STORED but nothing will drain it — floonet has never seen this session id, and \
                 it is delivered only if a session registers under exactly this id. Do not resend \
                 to this one: check `fl live`"
                    .to_string()
            }
        }
    })
}

/// What to type into a target's pane, by its runtime: the control string is
/// the runtime's vocabulary, not floonet's, and a runtime that does not know
/// `/fl inbox` cannot act on the wake. Resolved here rather than inside
/// `tp-reach`, which holds no runtime knowledge by design.
pub(crate) fn control_string_for(target: &str) -> String {
    target
        .split('/')
        .nth(1)
        .and_then(tp_ingest::adapter::control_string_for)
        .unwrap_or_else(|| tp_reach::CONTROL_STRING.to_string())
}

/// The literal text to put after "reply with:" for whoever is reading this
/// message — keyed on `to_session`, the recipient, not the sender. `{id}` is
/// the only substitution, and it is a message id floonet generated, never
/// anything a request supplied.
pub(crate) fn reply_hint_for(to_session: &str, short_id: &str) -> String {
    to_session
        .split('/')
        .nth(1)
        .and_then(tp_ingest::adapter::reply_hint_for)
        .map_or_else(
            || format!("fl reply {short_id} \"...\""),
            |h| h.replace("{id}", short_id),
        )
}

/// How the session whose inbox this is confirms it has finished with a
/// message. Keyed on that session, for the same reason `reply_hint_for` is:
/// whether the shell can write the mailbox is a fact about the recipient's
/// runtime, not about floonet.
///
/// No id is substituted. These lines summarise a drained batch, and naming one
/// message's id would be a claim about which one is meant.
pub(crate) fn ack_hint_for(session: &str) -> String {
    session
        .split('/')
        .nth(1)
        .and_then(tp_ingest::adapter::ack_hint_for)
        .unwrap_or_else(|| "`fl ack <id>`".to_string())
}

pub(crate) fn finish_send(
    app: &tp_app::App,
    target: &str,
    msg_id: &str,
    no_wake: bool,
    verb: &str,
    hint: Option<&str>,
) -> Result<()> {
    let short = &msg_id[..8];
    let outcome = if no_wake {
        "no wake".to_string()
    } else {
        // Always a CLI process, never `fld`, so any backend including iTerm2
        // AppleScript is fair game (see `tp_reach::Caller`).
        match app.attempt_wake(target, &control_string_for(target), tp_reach::Caller::Cli)? {
            tp_reach::DeliveryOutcome::Woke(tp_reach::Target::Tmux(pane)) => {
                format!("woke tmux pane {pane}")
            }
            tp_reach::DeliveryOutcome::Woke(tp_reach::Target::Terminal { id, tty }) => {
                format!("woke {id} session {tty}")
            }
            tp_reach::DeliveryOutcome::Woke(tp_reach::Target::Channel(chan)) => {
                format!("woke via the runtime's own channel ({chan:?})")
            }
            // The two non-injectable targets, spelled out rather than caught
            // by a wildcard, so this match is exhaustive over `Target` and a
            // new variant is a compile error on this surface, not only in
            // tp-reach.
            tp_reach::DeliveryOutcome::Woke(
                other @ (tp_reach::Target::Unreachable | tp_reach::Target::NotLive),
            ) => {
                unreachable!("attempt_wake only returns Woke for injectable targets, got {other:?}")
            }
            tp_reach::DeliveryOutcome::Coalesced => format!(
                "already woken in the last {}ms — will be drained with the rest",
                tp_reach::WAKE_COALESCE_MS
            ),
            // Cannot happen right after our own enqueue succeeded; handled
            // rather than assumed.
            tp_reach::DeliveryOutcome::NoMessages => "queued".to_string(),
            tp_reach::DeliveryOutcome::NotInjectable(tp_reach::Target::Unreachable) => {
                // A registered session with no injectable pane: drained by
                // someone, just not pokeable from here.
                "registered but not injectable — target checks on next /fl inbox".to_string()
            }
            tp_reach::DeliveryOutcome::NotInjectable(_) => undeliverable_note(app, target)?,
        }
    };
    println!("{verb} {short} → {target} ({outcome})");
    if let Some(h) = hint {
        println!("  {h}");
    }
    Ok(())
}

pub(crate) fn run_type(tty: &str, message: &str) -> Result<()> {
    let tty = tty.trim_start_matches("/dev/");
    let target = tp_reach::resolve_tty(tty)?;
    if matches!(target, tp_reach::Target::Unreachable) {
        anyhow::bail!("no tmux pane or iTerm2 session found with tty {tty} — check `ps -o tty=,comm=` for the right one");
    }
    // A CLI process, so any backend including iTerm2 AppleScript is fair game.
    tp_reach::type_raw(&target, message, tp_reach::Caller::Cli)?;
    println!("typed into {target:?}");
    Ok(())
}

/// An explicit `--session-id` may be bare or composite; the fallback is
/// registry-first (see `own_session_id`), because the env var can point at a
/// mailbox nobody writes to after a resume. Shared by every command that reads
/// a mailbox, so the resolution rule cannot drift between them.
pub(crate) fn resolve_inbox_session(session_id: Option<String>, runtime: &str) -> Result<String> {
    match session_id {
        Some(sid) => resolve_session_id(&sid, runtime),
        None => own_session_id().ok_or_else(|| {
            anyhow::anyhow!(
                "no session id — pass --session-id, or run from a registered agent session (is tpd running?)"
            )
        }),
    }
}

/// Why an inbox came back empty, when "empty" is not the whole answer. `None`
/// means genuinely empty at an address that works. The receive-side mirror of
/// `undeliverable_note`: "nobody wrote to you" and "nobody has ever used that
/// address" must not be answered with the same words, because a session that
/// passes a bare native id by hand cannot otherwise learn that its live
/// session is reachable under a different string.
pub(crate) fn empty_inbox_note(app: &tp_app::App, sid: &str) -> Result<Option<String>> {
    Ok(match app.addressability(sid)? {
        // The address is fine and nothing is waiting. Saying anything here
        // would train readers to skim past the cases that do matter.
        tp_core::Addressability::Registered => None,
        tp_core::Addressability::DormantConversation => Some(
            "— this conversation has no registered session right now. Anything sent to it is \
             HELD, not lost, and arrives once a segment registers"
                .to_string(),
        ),
        tp_core::Addressability::EndedConversation => Some(
            "— this conversation's host process has exited, so nothing will ever register into \
             it again and nothing here will be drained. If you are looking for your OWN inbox, \
             you are reading the wrong address: find it with `fl live`"
                .to_string(),
        ),
        tp_core::Addressability::Dormant => Some(
            "— but this session id is indexed and no longer registered, so nothing drains this \
             address any more. If you are a live session, your id has rotated: find the current \
             one with `fl live`"
                .to_string(),
        ),
        tp_core::Addressability::Unknown {
            transcript_readable,
        } => Some(if transcript_readable {
            "— but nothing is registered at this address, so this says nothing about whether \
             anyone wrote to you. floonet can read this session's transcript; being readable is \
             not being reachable, and only a registered address has an inbox. If you are that \
             session and it is live, register it or find your address with `fl live`"
                .to_string()
        } else {
            "— but floonet has never seen this session id, so this says nothing about whether \
             anyone wrote to you; it says the ADDRESS is wrong. An address is \
             `<machine>/<runtime>/<native>` — a bare native id matches nothing. Find yours with \
             `fl live`"
                .to_string()
        }),
    })
}

/// Print an empty-inbox line, with the reason appended when there is one.
fn print_empty(app: &tp_app::App, sid: &str, line: &str) -> Result<()> {
    match empty_inbox_note(app, sid)? {
        Some(note) => println!("{line} {note}"),
        None => println!("{line}"),
    }
    Ok(())
}

/// Render one message. Shared by `fl inbox`'s three modes — drain, pending,
/// history — because a message means the same thing in all three; only the
/// footer around the list changes.
pub(crate) fn print_message(m: &tp_reach::Message) {
    // The id and the sender are part of the message, not decoration: a
    // recipient that cannot see them invents an address instead. The exact
    // reply command is printed rather than described.
    println!("[{}] from {}", m.kind, m.from_machine);
    println!("  id: {}", &m.id[..8]);
    match &m.from_session {
        Some(from) => {
            println!("  from-session: {from}");
            // A note wakes the reader like anything else, so without this line
            // it is indistinguishable from a request, and two agents spend a
            // turn each being polite. One arm per kind: an answer shown with a
            // reply instruction under it gets answered, and the sender is
            // woken for nothing.
            match m.kind.as_str() {
                "note" => {
                    println!("  FYI — no reply expected. Answer only if you have something to add.")
                }
                "reply" => println!(
                    "  This ANSWERS something you asked. Nothing to do unless you have a follow-up."
                ),
                // A type floonet did not define — a build result, a webhook, a
                // finished job. "reply with:" is a claim about intent, and
                // nothing chose that here; the command is still offered as an
                // option, not as the expected next step.
                other if tp_app::Kind::parse(other) != tp_app::Kind::Ask => println!(
                    "  This is a `{other}` event, not a request — nothing here asks for an \
                     answer. If you have something worth sending back: {}",
                    reply_hint_for(&m.to_session, &m.id[..8])
                ),
                // Per-runtime for the same reason `control_string` is: `fl
                // reply` is a shell command, and a runtime that sandboxes its
                // shell may still let its own floonet tool write the mailbox.
                // Read from the descriptor, so the answer is the recipient's.
                _ => println!(
                    "  reply with: {}",
                    reply_hint_for(&m.to_session, &m.id[..8])
                ),
            }
        }
        None => {
            println!("  from-session: (none — this message cannot be replied to)");
        }
    }
    println!("  {}", m.body);
}

pub(crate) fn run_inbox(
    session_id: Option<String>,
    runtime: &str,
    pending: bool,
    history: bool,
    since: &str,
) -> Result<()> {
    let sid = resolve_inbox_session(session_id, runtime)?;
    let app = app()?;
    // `--pending` and `--history` are read-only views: checking them never
    // counts as having processed a message. Only the default path drains.
    if pending {
        let msgs = app.pending(&sid)?;
        if msgs.is_empty() {
            print_empty(&app, &sid, &format!("nothing pending ack for {sid}"))?;
            return Ok(());
        }
        for m in &msgs {
            print_message(m);
        }
        println!(
            "({} message(s) delivered but not yet acked — {} once you've handled each)",
            msgs.len(),
            ack_hint_for(&sid)
        );
        return Ok(());
    }
    if history {
        let since_ms =
            tp_core::now_ms().saturating_sub_ms(parse_duration(since)?.as_millis() as i64);
        let msgs = app.history(&sid, since_ms)?;
        if msgs.is_empty() {
            print_empty(
                &app,
                &sid,
                &format!("no acked messages for {sid} since {since}"),
            )?;
            return Ok(());
        }
        for m in &msgs {
            print_message(m);
            if m.acked_at.is_some() {
                println!("  acked: {}", fmt_ts(m.acked_at));
            }
        }
        println!("({} acked message(s) since {since})", msgs.len());
        return Ok(());
    }

    let drained = app.drain(&sid)?;
    let msgs = &drained.messages;
    if msgs.is_empty() {
        print_empty(&app, &sid, &format!("inbox empty for {sid}"))?;
        return Ok(());
    }
    for m in msgs {
        print_message(m);
    }
    println!(
        "({} message(s) drained — {} once you've acted on each, so an interrupted \
         batch stays recoverable from the pending view)",
        msgs.len(),
        ack_hint_for(&sid)
    );
    Ok(())
}

pub(crate) fn run_ack(message_id: &str) -> Result<()> {
    let m = app()?.ack(message_id)?;
    println!(
        "acked {} — [{}] from {}",
        &m.id[..8],
        m.kind,
        m.from_machine
    );
    Ok(())
}

/// Keep the trail bounded without erasing what is in it.
///
/// The cap exists because this is a breadcrumb trail, not an audit log, and
/// an unbounded file on a machine that registers hundreds of times a day is
/// its own problem. Truncating it WHOLE is what the cap used to mean, and the
/// cost is not hypothetical: the rows under investigation had registered
/// before the cut both times it mattered, so the lines that would have said
/// how they got there were gone and the question could not be answered from
/// the data at all.
///
/// Half the cap rather than all of it, so the next cut is not one line away.
/// Whole lines only: a severed first line is noise a reader has to recognise
/// before they can skip it, and this file exists to be read under suspicion.
/// Best-effort throughout — a trail that cannot be trimmed must not stop a
/// hook from registering, which is the thing that actually matters.
fn bound_breadcrumb_file(path: &std::path::Path, cap: u64) {
    if !std::fs::metadata(path).is_ok_and(|m| m.len() > cap) {
        return;
    }
    let Ok(text) = std::fs::read_to_string(path) else {
        // Unreadable: the cap still has to hold, and a trail nobody can read
        // is not evidence worth keeping.
        let _ = std::fs::remove_file(path);
        return;
    };
    let budget = (cap / 2) as usize;
    let mut kept: Vec<&str> = Vec::new();
    let mut used = 0usize;
    for line in text.lines().rev() {
        let cost = line.len() + 1;
        if used + cost > budget {
            break;
        }
        used += cost;
        kept.push(line);
    }
    kept.reverse();
    let mut tail = kept.join("\n");
    if !tail.is_empty() {
        tail.push('\n');
    }
    let _ = std::fs::write(path, tail);
}

/// Record what a hook invocation did, in a file. A hook has no reader — its
/// stdout goes wherever the harness decides, possibly nowhere — and it is the
/// one mechanism whose job is to make a session reachable: failing is allowed,
/// failing invisibly is not. Only for `--from-hook`, since a human sees the
/// terminal. Best-effort: a hook that cannot write its breadcrumb still registers.
fn hook_breadcrumb(outcome: &str) {
    // Beside the database, following TP_DB with it, so a probe against a
    // scratch database does not append to the real trail.
    let Some(dir) = tp_db::default_db_path()
        .parent()
        .map(std::path::Path::to_path_buf)
    else {
        return;
    };
    let path = dir.join("hook.log");
    bound_breadcrumb_file(&path, 256 * 1024);
    let line = format!(
        "{} pid {} ppid {} — {outcome}\n",
        crate::fmt_ts(Some(tp_core::now_ms())),
        std::process::id(),
        // The harness that invoked us: which hook failed is a different
        // investigation from whether one did.
        std::os::unix::process::parent_id(),
    );
    use std::io::Write as _;
    let _ = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .and_then(|mut f| f.write_all(line.as_bytes()));
}

pub(crate) fn run_register(
    session_id: Option<String>,
    cwd: Option<String>,
    from_hook: bool,
    stated_runtime: Option<String>,
    presence: &str,
    deliver: Option<&str>,
    declared_pid: Option<i32>,
) -> Result<()> {
    let r = do_register(
        session_id,
        cwd,
        from_hook,
        stated_runtime,
        presence,
        deliver,
        declared_pid,
    );
    // Written for both outcomes: "it ran and succeeded" and "it never ran" are
    // the two answers this exists to settle, and only one is visible from the
    // database.
    if from_hook {
        hook_breadcrumb(&match &r {
            Ok((sid, src)) => {
                format!(
                    "registered [source={}] {sid}",
                    src.as_deref().unwrap_or("-")
                )
            }
            Err(e) => format!("FAILED: {e:#}"),
        });
    }
    r.map(|_| ())
}

fn do_register(
    session_id: Option<String>,
    cwd: Option<String>,
    from_hook: bool,
    stated_runtime: Option<String>,
    presence: &str,
    deliver: Option<&str>,
    declared_pid: Option<i32>,
) -> Result<(String, Option<String>)> {
    let presence = tp_app::session::parse_presence(presence)
        .map_err(|e| anyhow::anyhow!("{e} (--presence)"))?;
    let (native_id, cwd, transcript_path, source) = if from_hook {
        let ev = read_hook_event()?;
        (ev.session_id, ev.cwd, ev.transcript_path, ev.source)
    } else {
        let sid = session_id
            .ok_or_else(|| anyhow::anyhow!("--session-id is required without --from-hook"))?;
        (sid, cwd, None, None)
    };
    let runtime = &runtime_for_registration(stated_runtime);

    // Not every session is a correspondent. Claude Code spawns headless `-p`
    // sub-conversations inside a live pane; each fires SessionStart, and a
    // hook row is reaped only when its process — the pane — dies, hours later.
    // The transcript says what it is, so this asks instead of guessing, and
    // which values mean "not a conversation" comes from the descriptor, as
    // with `control_string` and `reply_hint`.
    if let Some(kind) = transcript_path.as_deref().and_then(transcript_kind) {
        if tp_ingest::adapter::non_conversation_types_for(runtime).contains(&kind) {
            println!(
                "not registering {native_id}: this is a {kind}, not a conversation — nobody \
                 addresses one, and registering it would leave a row behind that outlives it"
            );
            return Ok((native_id, source));
        }
    }
    let session_id = live_session_id(&native_id, runtime)?;

    let app = app()?;
    app.ensure_self_machine()?;
    // `tp` is a child of the session (or a shell wrapping it), so an
    // undeclared host is found by walking up. A runtime that states its own
    // pid is taken at its word: the walk cannot tell "the process hosting this
    // session" from "whatever launched it".
    let host = match declared_pid {
        Some(pid) => tp_app::Host::Declared(pid),
        None => tp_app::Host::Inferred {
            from_pid: std::process::id() as i32,
            needle: ancestor_needle(runtime),
        },
    };

    let r = app.register(&session_id, host, cwd.as_deref(), presence, deliver)?;
    println!(
        "registered {} → pid {} tty {} [{}{}]",
        r.session_id,
        r.pid,
        r.tty.as_deref().unwrap_or("(none)"),
        if r.presence == tp_reach::resolve::Presence::Declared {
            "declared"
        } else {
            "scan"
        },
        r.deliver
            .map(|d| format!(", deliver {d}"))
            .unwrap_or_default(),
    );
    Ok((r.session_id, source))
}

pub(crate) fn run_heartbeat(session_id: &str, runtime: &str) -> Result<()> {
    let session_id = live_session_id(session_id, runtime)?;
    let app = app()?;
    // Report the no-op: a runtime beating into a row floonet already evicted
    // must learn it has to re-register.
    if app.heartbeat(&session_id)? {
        println!("heartbeat {session_id}");
    } else {
        println!("no live registration for {session_id} — re-register to be reachable");
    }
    Ok(())
}

pub(crate) fn run_unregister(
    session_id: Option<String>,
    from_hook: bool,
    stated_runtime: Option<String>,
) -> Result<()> {
    // Same resolution as registration, or SessionEnd would target a row that
    // SessionStart never wrote and leave the session live forever.
    let runtime = &runtime_for_registration(stated_runtime);
    let native_id = if from_hook {
        read_hook_event()?.session_id
    } else {
        session_id.ok_or_else(|| anyhow::anyhow!("--session-id is required without --from-hook"))?
    };
    let session_id = live_session_id(&native_id, runtime)?;

    // Pin the delete to our own resolved ancestor pid: if this session_id was
    // reused (`/clear`) and a new SessionStart already reclaimed the row, this
    // SessionEnd must not remove the newer binding. Without a matching
    // ancestor there is no identity to compare, so the delete is unconditional
    // rather than pinned to `tp`'s unrelated pid.
    let self_pid = std::process::id() as i32;
    let expected_pid = tp_reach::resolve::find_session_process(self_pid, &ancestor_needle(runtime))
        .map(|(pid, _)| pid);

    let removed = app()?.unregister(&session_id, expected_pid)?;
    // Nothing guarantees a harness fires SessionEnd where this hook runs; if
    // it does not, the row stays and the only evidence is a panel that fills
    // up. Tracing the teardown keeps "the signal never came" and "the signal
    // came and the delete missed" distinguishable.
    if from_hook {
        hook_breadcrumb(&if removed {
            format!("unregistered {session_id}")
        } else {
            format!("unregister MISSED (no row on pid {expected_pid:?}) {session_id}")
        });
    }
    if removed {
        println!("unregistered {session_id}");
    } else if let Some(pid) = expected_pid {
        // Name the pin: this is the outcome it exists to produce, and "nothing
        // happened" is otherwise indistinguishable from a typo.
        println!(
            "nothing to unregister for {session_id} — no row on pid {pid}. Either it was already \
             gone, or it belongs to a newer session that reused this id, which this delete is \
             pinned NOT to remove"
        );
    } else {
        println!("nothing to unregister for {session_id} — no such row");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::bound_breadcrumb_file;

    /// The trail has to survive being bounded.
    ///
    /// Deleting it whole was the old behaviour and it cost two investigations
    /// their evidence in one day: both times the rows under inspection had
    /// registered BEFORE the cut, so the lines that would have said how they
    /// got there were gone, and the question could not be answered from the
    /// data at all. A bounded file keeps the cap; an emptied one keeps nothing.
    #[test]
    fn bounding_the_trail_keeps_the_end_of_it() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("hook.log");

        // Every line names itself, so the assertions can say which survived.
        let mut text = String::new();
        for i in 0..4000 {
            text.push_str(&format!("line {i}\n"));
        }
        std::fs::write(&path, &text).unwrap();

        let cap = 8 * 1024;
        assert!(
            std::fs::metadata(&path).unwrap().len() > cap,
            "the fixture must start over the cap or this tests nothing"
        );

        bound_breadcrumb_file(&path, cap);

        let after = std::fs::read_to_string(&path).expect(
            "the trail was deleted rather than bounded — the evidence a reader \
             came for is gone, which is the failure this test exists for",
        );
        assert!(
            after.len() as u64 <= cap,
            "still over the cap after bounding: {} bytes",
            after.len()
        );
        assert!(
            after.contains("line 3999\n"),
            "the newest line is the one a reader came for, and it did not survive"
        );
        assert!(
            !after.contains("line 0\n"),
            "the oldest lines should be the ones dropped"
        );
        assert!(
            after.starts_with("line "),
            "a half line at the head is noise a reader has to recognise and skip: {:?}",
            &after[..20.min(after.len())]
        );
    }
}
