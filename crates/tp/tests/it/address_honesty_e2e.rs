//! End-to-end: `fl ask` must never claim delivery it cannot make.
//!
//! Two ways a send can be undeliverable while reading like success with a
//! delay: an address that was never an address (a bare native id, a bare
//! machine id, a truncated runtime), and an address to a real session whose id
//! has rotated — Claude Code mints a new session id at every compaction. A
//! unit test on the classifier cannot catch a send path that never asks it, so
//! this runs the real binary and asserts on what a caller sees.

use std::path::Path;
use std::process::Command;

fn tp(home: &Path, args: &[&str]) -> (bool, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_fl"))
        .args(args)
        .env("HOME", home)
        .output()
        .expect("run tp");
    let mut s = String::from_utf8_lossy(&out.stdout).to_string();
    s.push_str(&String::from_utf8_lossy(&out.stderr));
    (out.status.success(), s)
}

/// Every malformed address shape, as one table. Each must fail before the
/// message is stored — a rejected send is recoverable, a stored one that is
/// never delivered is not.
#[test]
fn a_malformed_address_is_refused_and_nothing_is_enqueued() {
    let home = tempfile::tempdir().unwrap();
    for bad in [
        "e2e0a11c-0000-4000-8000-000000000001", // bare native id
        "AAAA-BBBB-CCCC-DDDD",                  // bare machine id
        "AAAA-BBBB-CCCC-DDDD/claud",            // truncated before the native id
        "machine//native",                      // empty runtime segment
    ] {
        let (ok, out) = tp(home.path(), &["ask", bad, "hello"]);
        assert!(!ok, "{bad:?} must be rejected, got success:\n{out}");
        assert!(
            out.contains("is not a session address"),
            "{bad:?} must say why it is not an address:\n{out}"
        );
        assert!(
            out.contains("fl live"),
            "{bad:?} must point at where real addresses come from:\n{out}"
        );
    }

    // Nothing was written. If a refusal still enqueued, the message would be
    // exactly as lost as before, just louder.
    let (_, inbox) = tp(home.path(), &["inbox", "--session-id", "machine/rt/native"]);
    assert!(
        inbox.contains("inbox empty"),
        "a refused send must not leave a message behind:\n{inbox}"
    );
}

/// A well-formed but unknown address is accepted — a mailbox deliberately has
/// no FK to `session`, so an id can legitimately arrive before its session
/// does. What must change is the claim: it is parked, not delivered.
#[test]
fn an_unknown_address_is_accepted_but_never_reported_as_delivered() {
    let home = tempfile::tempdir().unwrap();
    let target = "AAAA-BBBB-CCCC-DDDD/claude_code/00000000-1111-2222-3333-444444444444";

    let (ok, out) = tp(home.path(), &["ask", target, "hello"]);
    assert!(
        ok,
        "a well-formed unknown address must still be accepted:\n{out}"
    );
    // Two claims: the message must not be reported as delivered, and not as
    // failed either, because it is stored — an apparent failure invites a resend.
    assert!(
        out.contains("STORED"),
        "an undeliverable send must still say the message was kept:\n{out}"
    );
    assert!(
        out.contains("nothing will drain it"),
        "...and must say plainly that nobody is going to read it:\n{out}"
    );
    assert!(
        out.contains("Do not resend"),
        "...and must say not to retry, which is what a reader does with an \
         apparent failure:\n{out}"
    );
    assert!(
        !out.contains("delivered on next /fl inbox"),
        "the old wording promised delivery floonet has no basis for:\n{out}"
    );
    // The hint below the status line is a second, independent promise. Saying
    // "end your turn and wait for the reply" under a PARKED line is the same
    // lie one line further down.
    assert!(
        out.contains("Do NOT wait"),
        "an undeliverable send must not tell the caller to wait for a reply:\n{out}"
    );
    assert!(
        !out.contains("The reply arrives later"),
        "the wait-for-reply hint must be suppressed when nothing can reply:\n{out}"
    );

    // Accepted really does mean stored — the message is there for the session
    // that may yet register under this id.
    let (_, inbox) = tp(home.path(), &["inbox", "--session-id", target]);
    assert!(
        inbox.contains("hello"),
        "an accepted message must be readable by its addressee:\n{inbox}"
    );
}

/// The return address a reply stamps must be the pane's live conversation, not
/// whichever one the sending session happens to belong to.
/// `conversations_of_pane` can be correct and `sender_address` can still not
/// call it, so this runs the binary. The pane is twinned the way ordinary work
/// twins one: same pid and pid_start, a changed cwd.
#[test]
fn a_reply_stamps_the_live_twin_not_the_session_s_own() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path();

    let (stale_conv, live_conv, stale_session) = {
        let db_path = home.join(".teleport/teleport.db");
        std::fs::create_dir_all(db_path.parent().unwrap()).unwrap();
        let mut db = tp_db::Db::open(&db_path).unwrap();
        db.ensure_self_machine("m1", "TestMac").unwrap();
        db.ensure_runtime("claude_code", "/root").unwrap();

        let key = |cwd| tp_db::reach::ConversationKey {
            machine_id: "m1",
            runtime_id: "claude_code",
            pid: 4242,
            pid_start: Some("Fri Aug 21 09:00:00 2026"),
            cwd: Some(cwd),
        };
        let conn = db.conn_mut();
        // Older segment first, then a cwd change splits the pane.
        let stale = tp_db::reach::join_conversation(
            conn,
            "m1/claude_code/segOld",
            key("/work"),
            tp_core::Millis::new(1_000),
            "m1/claude_code/conv-stale",
        )
        .unwrap();
        let live = tp_db::reach::join_conversation(
            conn,
            "m1/claude_code/segNew",
            key("/work/sub"),
            tp_core::Millis::new(2_000),
            "m1/claude_code/conv-live",
        )
        .unwrap();
        assert_ne!(
            stale, live,
            "the cwd change must split, or this proves nothing"
        );
        (stale, live, "m1/claude_code/segOld".to_string())
    };

    // A message sent by the session that belongs to the stale twin: the return
    // address must be the live twin, which is the one a reply would reach.
    let (ok, out) = tp(
        home,
        &[
            "note",
            "m1/claude_code/someone-else",
            "hello",
            "--from-session",
            &stale_session,
        ],
    );
    assert!(ok, "{out}");

    let stamped: String = rusqlite::Connection::open(home.join(".teleport/teleport.db"))
        .unwrap()
        .query_row(
            "SELECT from_session FROM message ORDER BY created_at DESC LIMIT 1",
            [],
            |r| r.get(0),
        )
        .unwrap();

    assert_eq!(
        stamped, live_conv,
        "stamped the stale twin ({stale_conv}) — a reply to it reaches nobody"
    );
}

/// The receive half of the same question. `inbox empty for {sid}` answers
/// "nobody wrote to you" and "nobody has ever used that address" identically,
/// and empty is not a fact about a mailbox that does not exist. Runs the binary
/// rather than the classifier: the defect this guards is the caller not asking.
#[test]
fn an_empty_inbox_says_whether_the_address_is_even_real() {
    let home = tempfile::tempdir().unwrap();

    // An address nothing has ever registered or indexed.
    let (ok, out) = tp(
        home.path(),
        &[
            "inbox",
            "--session-id",
            "AAAA-BBBB-CCCC-DDDD/claude_code/00000000-1111-2222-3333-444444444444",
        ],
    );
    assert!(ok, "reading an unknown address must not fail:\n{out}");
    assert!(
        out.contains("never seen this session id"),
        "an empty inbox at an unknown address must say the ADDRESS is wrong, not \
         imply nobody wrote:\n{out}"
    );
    assert!(
        out.contains("<machine>/<runtime>/<native>"),
        "...and must show the form, because the observed failure was a caller \
         passing a bare native id:\n{out}"
    );

    // The mirror, and the half that makes the first half worth anything: a
    // registered address with nothing waiting must stay QUIET. A note on every
    // empty inbox trains the reader to skim past the one that matters.
    let sid = "00000000-1111-2222-3333-555555555555";
    // `--runtime` stated explicitly. Omitted, register walks this process's
    // ancestor chain for a runtime signature, so the session would land under
    // whatever the test was run from.
    let (ok, reg) = tp(
        home.path(),
        &[
            "register",
            "--session-id",
            sid,
            "--pid",
            "1",
            "--runtime",
            "claude_code",
        ],
    );
    assert!(ok, "register failed:\n{reg}");
    let (ok, out) = tp(home.path(), &["inbox", "--session-id", sid]);
    assert!(ok, "reading a live address must not fail:\n{out}");
    assert!(
        out.contains("inbox empty"),
        "a registered session with no mail still reports empty:\n{out}"
    );
    assert!(
        !out.contains("never seen this session id"),
        "a WORKING empty inbox must not be annotated:\n{out}"
    );
}

/// Readable is not reachable. `fl sessions` hands out ids for every transcript
/// on disk, `fl live` addresses for sessions that registered, and the two are
/// the same string shape, so writing to a read conversation's author is the
/// obvious next step. The distinction cannot come from the database, which is
/// exactly what does not know this session; it comes from the transcript
/// roots, which is why `transcript_exists` lives in tp-ingest and not in tp-db.
#[test]
fn a_readable_but_unregistered_address_is_not_called_unknown() {
    let home = tempfile::tempdir().unwrap();

    // A transcript on disk and nothing else — no register, no daemon. This is
    // every session that ran before floonet was installed, plus every session
    // of a runtime that does not register at all.
    let native = "00000000-1111-2222-3333-666666666666";
    let dir = home.path().join(".claude/projects/-tmp-somewhere");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join(format!("{native}.jsonl")),
        format!(
            "{}\n",
            serde_json::json!({
                "type": "user",
                "uuid": "00000000-0000-4000-8000-000000000001",
                "sessionId": native,
                "timestamp": "2026-01-01T00:00:00.000Z",
                "cwd": "/tmp/somewhere",
                "message": {"role": "user", "content": "hello"},
            })
        ),
    )
    .unwrap();

    // `fl id` prints `device id : XXXX-…`; the first token is the word
    // "device", a valid-looking segment for a machine that does not exist.
    let machine = {
        let (ok, out) = tp(home.path(), &["id"]);
        assert!(ok, "fl id failed:\n{out}");
        out.lines()
            .find_map(|l| l.split_once("device id")?.1.split_once(':'))
            .map(|(_, id)| id.trim().to_string())
            .unwrap_or_else(|| panic!("no `device id :` line in `fl id`:\n{out}"))
    };
    let target = format!("{machine}/claude_code/{native}");

    // SEND: accepted and parked.
    let (ok, out) = tp(home.path(), &["ask", &target, "hello"]);
    assert!(ok, "a readable address must still be accepted:\n{out}");
    assert!(
        !out.contains("never seen this session id"),
        "floonet CAN see this id — it is holding the transcript:\n{out}"
    );
    assert!(
        out.contains("readable") || out.contains("READ"),
        "the note must say the transcript is readable, so the sender stops \
         re-checking an address that is not the problem:\n{out}"
    );
    assert!(
        out.contains("fl live"),
        "...and must still say where a reachable address comes from:\n{out}"
    );

    // RECEIVE: the same split, because the same confusion runs the other way —
    // a session that passes its own native id and is told its address is wrong.
    let (ok, out) = tp(home.path(), &["inbox", "--session-id", &target]);
    assert!(ok, "reading a readable address must not fail:\n{out}");
    assert!(
        !out.contains("never seen this session id"),
        "an empty inbox at a readable address must not deny the id:\n{out}"
    );

    // And the contrast that gives the above its meaning: an id with NO
    // transcript still gets the blunt answer. If both said the same thing the
    // split would be decoration.
    let ghost = format!("{machine}/claude_code/00000000-1111-2222-3333-777777777777");
    let (_, out) = tp(home.path(), &["ask", &ghost, "hello"]);
    assert!(
        out.contains("never seen this session id"),
        "an id with no transcript and no registration is genuinely unknown:\n{out}"
    );
}

/// A `scan-pid-N` address is deliverable and unreadable, and must say so.
///
/// The process scan finds harnesses that never registered and mints a
/// placeholder rather than guessing a transcript id: a guess is a coin flip
/// with two sessions in one directory. The scan knows the pid and tty so a
/// wake works; nothing maps the id to a transcript so a read cannot. The cause
/// is stated where a user meets it because the remedy is theirs: Claude Code
/// and codex register once, at SessionStart, with no heartbeat, so anything
/// that empties the database unregisters every session alive at that moment.
#[test]
fn a_scan_placeholder_says_it_cannot_be_read() {
    let home = tempfile::tempdir().unwrap();
    let machine = {
        let (ok, out) = tp(home.path(), &["id"]);
        assert!(ok, "fl id failed:\n{out}");
        out.lines()
            .find_map(|l| l.split_once("device id")?.1.split_once(':'))
            .map(|(_, id)| id.trim().to_string())
            .unwrap_or_else(|| panic!("no `device id :` line:\n{out}"))
    };
    let placeholder = format!("{machine}/claude_code/scan-pid-4242");

    let (ok, out) = tp(home.path(), &["turns", &placeholder]);
    assert!(ok, "reading a placeholder must not fail:\n{out}");
    assert!(
        !out.contains("no turns found"),
        "\"no turns found\" reads as \"this session is idle\" and sends the \
         caller hunting for a better id — which is exactly what went wrong:\n{out}"
    );
    assert!(
        out.contains("never registered"),
        "the answer must name the cause, not just decline:\n{out}"
    );
    assert!(
        out.contains("Restart"),
        "...and the remedy, which only the operator can apply:\n{out}"
    );

    // The window arms are the more convincing lie — "no turns since 2h" sounds
    // like a measurement — so the placeholder check has to come first.
    let (_, windowed) = tp(home.path(), &["turns", &placeholder, "--since", "2h"]);
    assert!(
        !windowed.contains("no turns since"),
        "a time-bounded read of a placeholder must not imply the window was \
         searched:\n{windowed}"
    );

    // The contrast. A real id with genuinely nothing in it keeps the plain
    // answer — if both said the same thing the distinction would be decoration.
    let real = format!("{machine}/claude_code/00000000-1111-2222-3333-999999999999");
    let (_, out) = tp(home.path(), &["turns", &real]);
    assert!(
        out.contains("no turns found"),
        "a real id with no turns is still just empty:\n{out}"
    );
}

/// Not every session is a correspondent.
///
/// Claude Code spawns short-lived internal `queue-operation` sessions inside a
/// live pane. Each fires SessionStart like a real conversation, and nothing
/// removes it: a hook row is reaped when its process dies, and the process is
/// the pane. The transcript declares what it is, so floonet asks rather than
/// guesses, and the descriptor decides which values mean "not a conversation".
/// The third case — an unreadable transcript — is the easiest to get wrong.
#[test]
fn an_internal_session_is_not_registered_but_an_unreadable_one_still_is() {
    let home = tempfile::tempdir().unwrap();
    let dir = home.path().join("transcripts");
    std::fs::create_dir_all(&dir).unwrap();

    let internal = dir.join("q.jsonl");
    std::fs::write(&internal, "{\"type\":\"queue-operation\",\"uuid\":\"x\"}\n").unwrap();
    let real = dir.join("r.jsonl");
    std::fs::write(&real, "{\"type\":\"user\",\"uuid\":\"y\"}\n").unwrap();

    let hook = |sid: &str, path: &str| -> String {
        let out = std::process::Command::new(env!("CARGO_BIN_EXE_fl"))
            .args(["register", "--from-hook", "--runtime", "claude_code"])
            .env("HOME", home.path())
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .and_then(|mut c| {
                use std::io::Write as _;
                let payload = format!(
                    "{{\"session_id\":\"{sid}\",\"cwd\":\"/w\",\"transcript_path\":\"{path}\"}}"
                );
                c.stdin.take().unwrap().write_all(payload.as_bytes())?;
                c.wait_with_output()
            })
            .expect("run register");
        let mut s = String::from_utf8_lossy(&out.stdout).to_string();
        s.push_str(&String::from_utf8_lossy(&out.stderr));
        s
    };

    let out = hook("internal-1", internal.to_str().unwrap());
    assert!(
        out.contains("not registering"),
        "an internal session must not become a correspondent:\n{out}"
    );
    assert!(
        out.contains("queue-operation"),
        "...and must say what it decided it was, or this is unexplainable when \
         it is wrong:\n{out}"
    );

    let out = hook("real-1", real.to_str().unwrap());
    assert!(
        out.contains("registered"),
        "an ordinary session is unaffected:\n{out}"
    );

    // The harness may still be creating the file when SessionStart fires, so
    // "I cannot tell" must mean register: a stale row beats an unreachable
    // session.
    let out = hook("nofile-1", "/definitely/not/here.jsonl");
    assert!(
        out.contains("registered"),
        "an unreadable transcript must not cost a session its reachability:\n{out}"
    );

    let (ok, live) = tp(home.path(), &["live"]);
    assert!(ok, "fl live failed:\n{live}");
    assert!(
        !live.contains("internal-1"),
        "the internal session must be absent from the correspondent list:\n{live}"
    );
    for wanted in ["real-1", "nofile-1"] {
        assert!(
            live.contains(wanted),
            "{wanted} must still be listed:\n{live}"
        );
    }
}
