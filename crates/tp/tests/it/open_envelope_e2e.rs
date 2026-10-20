//! The mailbox has to carry something floonet did not invent.
//!
//! `ask` / `note` / `reply` is the vocabulary for one agent addressing another.
//! It cannot express an event from elsewhere — a failed build, a submitted
//! review — and folding those into `note` discards the type at the door, the
//! last place it was known. Email's lesson applies: open headers let MIME
//! arrive a decade late without a flag day. A message that is one untyped
//! string with nowhere to put unknown fields can only be replaced, not extended.

use std::path::Path;
use std::process::Command;

fn fl(home: &Path, args: &[&str]) -> (bool, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_fl"))
        .env_clear()
        .env("PATH", std::env::var("PATH").unwrap_or_default())
        .env("HOME", home)
        .args(args)
        .output()
        .expect("run fl");
    let mut s = String::from_utf8_lossy(&out.stdout).to_string();
    s.push_str(&String::from_utf8_lossy(&out.stderr));
    (out.status.success(), s)
}

fn machine(home: &Path) -> String {
    let (ok, out) = fl(home, &["id"]);
    assert!(ok, "fl id failed:\n{out}");
    out.lines()
        .find_map(|l| l.split_once("device id")?.1.split_once(':'))
        .map(|(_, id)| id.trim().to_string())
        .expect("device id line")
}

/// An unknown type survives storage and display verbatim, and is not described
/// as something that wants an answer. Anything not `note` or `reply` must not
/// fall through to "reply with: …": that line states the sender is waiting,
/// which is true for an `ask` and an intent nobody expressed for a build result.
#[test]
fn an_unknown_type_passes_through_without_being_called_a_request() {
    let home = tempfile::tempdir().unwrap();
    let m = machine(home.path());
    let target = format!("{m}/dsh/session-ext");
    let sender = format!("{m}/claude_code/conv-0000-1111-2222-3333");

    let (ok, out) = fl(
        home.path(),
        &[
            "ask",
            &target,
            "build 4711 failed",
            "--from-session",
            &sender,
        ],
    );
    assert!(ok, "send failed:\n{out}");

    // Only the type is rewritten; floonet has no sender for an external type
    // yet, and inventing one here would test the fixture instead of the code.
    let db = home.path().join(".teleport/teleport.db");
    let conn = rusqlite::Connection::open(&db).unwrap();
    conn.execute(
        "UPDATE message SET kind = 'ci.build.failed', content_type = 'application/json', \
         extensions = '{\"build_id\":\"4711\"}'",
        [],
    )
    .expect("the envelope columns must exist — 0019");
    drop(conn);

    let (ok, inbox) = fl(home.path(), &["inbox", "--session-id", &target]);
    assert!(ok, "inbox failed:\n{inbox}");
    assert!(
        inbox.contains("[ci.build.failed]"),
        "the type must survive verbatim — collapsing it to `note` throws away \
         the only thing that said what this was:\n{inbox}"
    );
    assert!(
        !inbox.contains("  reply with:"),
        "an external event must not be presented as a request: that line says \
         the sender is waiting, and nothing chose that:\n{inbox}"
    );
    assert!(
        inbox.contains("not a request"),
        "...and it has to say so, or a reader infers it from the absence of a \
         line, which is not something a reader does:\n{inbox}"
    );
    // Replying stays POSSIBLE — there is a return address. Offered as an
    // option, not as the expected next step.
    assert!(
        inbox.contains("something worth sending back"),
        "a reply is still available when there is a return address:\n{inbox}"
    );
}

/// The envelope columns default the way an existing row needs. Every stored
/// message is text a model reads, so `text/plain` is what those rows already
/// were. `extensions` defaults to NULL rather than `{}`: "nobody attached
/// anything" and "someone attached an empty object" are different statements.
#[test]
fn the_envelope_defaults_leave_existing_rows_exactly_as_true() {
    let home = tempfile::tempdir().unwrap();
    let m = machine(home.path());
    let (ok, out) = fl(home.path(), &["ask", &format!("{m}/dsh/s"), "hi"]);
    assert!(ok, "send failed:\n{out}");

    let conn = rusqlite::Connection::open(home.path().join(".teleport/teleport.db")).unwrap();
    let (ct, ext): (String, Option<String>) = conn
        .query_row(
            "SELECT content_type, extensions FROM message LIMIT 1",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .expect("0019 columns");
    assert_eq!(ct, "text/plain", "a message written today is still text");
    assert_eq!(ext, None, "absent is not the same as empty");
}

/// `kind` round-trips through the open parser, and floonet's three types keep
/// their short names. `[ask]` is read by a person and by a model, and
/// `[floonet.ask]` says nothing extra to either; namespacing is for types
/// whose origin is not obvious.
#[test]
fn floonet_keeps_its_short_names_and_everything_else_keeps_its_own() {
    for (stored, shown) in [
        ("ask", "[ask]"),
        ("note", "[note]"),
        ("ci.build.failed", "[ci.build.failed]"),
        ("github.review.submitted", "[github.review.submitted]"),
    ] {
        let home = tempfile::tempdir().unwrap();
        let m = machine(home.path());
        let target = format!("{m}/dsh/s");
        let (ok, out) = fl(home.path(), &["ask", &target, "x"]);
        assert!(ok, "send failed:\n{out}");
        let conn = rusqlite::Connection::open(home.path().join(".teleport/teleport.db")).unwrap();
        conn.execute("UPDATE message SET kind = ?1", [stored])
            .unwrap();
        drop(conn);
        let (_, inbox) = fl(home.path(), &["inbox", "--session-id", &target]);
        assert!(
            inbox.contains(shown),
            "{stored:?} must render as {shown:?}:\n{inbox}"
        );
    }
}
