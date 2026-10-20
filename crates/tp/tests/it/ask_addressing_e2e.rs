//! Who a `floo_ask` reaches, and what it refuses.
//!
//! A session asked to message `ideas` called `floo_ask` three times. The first
//! two passed the recipient as `to` — the name Claude Code's own SendMessage
//! uses — and were told only that `session_id` was required. The third kept the
//! recipient in `to` and put the CALLER's own id in `session_id`; floonet
//! ignored `to` without a word and delivered the request to the sender's own
//! inbox, reporting success. Three faults, each tested here: an argument the
//! tool does not know was dropped silently, the parameter naming the recipient
//! read like "your session id", and a message to oneself was accepted.

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

fn machine_id(home: &Path) -> String {
    let (ok, out) = fl(home, &["id"]);
    assert!(ok, "fl id failed:\n{out}");
    out.lines()
        .find_map(|l| l.split_once("device id")?.1.split_once(':'))
        .map(|(_, id)| id.trim().to_string())
        .expect("device id line")
}

/// One `tools/call` against `fl mcp`. Returns (is_error, text).
fn call(home: &Path, tool: &str, arguments: serde_json::Value) -> (bool, String) {
    use std::io::Write as _;
    let mut child = Command::new(env!("CARGO_BIN_EXE_fl"))
        .arg("mcp")
        .env_clear()
        .env("PATH", std::env::var("PATH").unwrap_or_default())
        .env("HOME", home)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .expect("spawn fl mcp");
    write!(
        child.stdin.take().expect("stdin"),
        "{}\n{}\n",
        serde_json::json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}),
        serde_json::json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{
            "name": tool, "arguments": arguments}}),
    )
    .expect("write request");
    let out = child.wait_with_output().expect("run fl mcp");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let last = stdout.lines().last().expect("fl mcp said nothing");
    let v: serde_json::Value = serde_json::from_str(last).expect("one JSON reply");
    let result = &v["result"];
    (
        result["isError"].as_bool().unwrap_or(false),
        result["content"][0]["text"]
            .as_str()
            .unwrap_or_default()
            .to_string(),
    )
}

/// Bodies waiting in `sid`'s inbox, drained.
fn inbox(home: &Path, sid: &str) -> Vec<String> {
    let (err, text) = call(home, "floo_inbox", serde_json::json!({ "session_id": sid }));
    assert!(!err, "floo_inbox failed: {text}");
    let v: serde_json::Value = serde_json::from_str(&text).expect("inbox JSON");
    v["messages"]
        .as_array()
        .map(|a| {
            a.iter()
                .map(|m| m["body"].as_str().unwrap_or_default().to_string())
                .collect()
        })
        .unwrap_or_default()
}

struct World {
    _tmp: tempfile::TempDir,
    a: String,
    b: String,
}

impl World {
    fn new() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let m = machine_id(tmp.path());
        World {
            a: format!("{m}/claude_code/00000000-0000-4000-8000-00000000000a"),
            b: format!("{m}/claude_code/00000000-0000-4000-8000-00000000000b"),
            _tmp: tmp,
        }
    }
    fn home(&self) -> &Path {
        self._tmp.path()
    }
}

#[test]
fn to_names_the_recipient() {
    let w = World::new();
    let (err, text) = call(
        w.home(),
        "floo_ask",
        serde_json::json!({ "to": w.b, "message": "hello b", "from_session": w.a }),
    );
    assert!(
        !err,
        "`to` is the name SendMessage uses, and the one a model reaches for: {text}"
    );
    assert_eq!(inbox(w.home(), &w.b), vec!["hello b"]);
}

#[test]
fn session_id_still_names_the_recipient_for_existing_callers() {
    let w = World::new();
    let (err, text) = call(
        w.home(),
        "floo_ask",
        serde_json::json!({ "session_id": w.b, "message": "hello b", "from_session": w.a }),
    );
    assert!(!err, "{text}");
    assert_eq!(inbox(w.home(), &w.b), vec!["hello b"]);
}

/// The measured call, exactly: recipient in `to`, the sender's own id in
/// `session_id`. Two different recipients is a contradiction to report, not a
/// choice to make — either guess delivers somewhere the caller did not mean.
#[test]
fn a_recipient_given_twice_and_differently_is_refused_and_nothing_is_sent() {
    let w = World::new();
    let (err, text) = call(
        w.home(),
        "floo_ask",
        serde_json::json!({ "to": w.b, "session_id": w.a, "message": "review please", "from_session": w.a }),
    );
    assert!(
        err,
        "two different recipients must be an error, got: {text}"
    );
    assert!(
        text.contains("to") && text.contains("session_id"),
        "name both fields: {text}"
    );
    assert!(
        inbox(w.home(), &w.a).is_empty(),
        "nothing may reach the sender's own inbox"
    );
    assert!(
        inbox(w.home(), &w.b).is_empty(),
        "and nothing may be sent on a guess"
    );
}

/// A dropped argument is how the recipient vanished. An argument the tool does
/// not know is a mistake the caller can fix in one retry, if it is told.
#[test]
fn an_argument_the_tool_does_not_know_is_refused_by_name() {
    let w = World::new();
    let (err, text) = call(
        w.home(),
        "floo_ask",
        serde_json::json!({ "recipient": w.b, "message": "hi" }),
    );
    assert!(
        err,
        "an unknown argument must not be dropped silently: {text}"
    );
    assert!(text.contains("recipient"), "say which argument: {text}");
    assert!(
        text.contains("`to`"),
        "and what it should have been: {text}"
    );
}

/// A message to one's own session is never the intent — Claude Code's own
/// listing warns that "a message to it would be a message to yourself" — and
/// the same is true of another segment of one's own conversation.
#[test]
fn a_message_to_yourself_is_refused() {
    let w = World::new();
    let (err, text) = call(
        w.home(),
        "floo_ask",
        serde_json::json!({ "to": w.a, "message": "hi", "from_session": w.a }),
    );
    assert!(err, "sending to your own session must be refused: {text}");
    assert!(text.to_lowercase().contains("your own"), "{text}");
    assert!(inbox(w.home(), &w.a).is_empty());
}

/// The conversation case: the caller signs with one segment, the target is a
/// later segment of the same conversation — what a stale id copied from
/// `fl live` looks like after a compaction. Registered with one pid, the way a
/// rotating session re-registers.
#[test]
fn a_message_to_another_segment_of_your_own_conversation_is_refused() {
    let w = World::new();
    let pid = std::process::id().to_string();
    for native in [
        "00000000-0000-4000-8000-0000000000c1",
        "00000000-0000-4000-8000-0000000000c2",
    ] {
        let (ok, out) = fl(
            w.home(),
            &[
                "register",
                "--session-id",
                native,
                "--runtime",
                "claude_code",
                "--pid",
                &pid,
                "--cwd",
                "/w",
            ],
        );
        assert!(ok, "register: {out}");
    }
    let m = w.a.rsplit_once("/claude_code/").unwrap().0.to_string();
    let first = format!("{m}/claude_code/00000000-0000-4000-8000-0000000000c1");
    let second = format!("{m}/claude_code/00000000-0000-4000-8000-0000000000c2");
    let (err, text) = call(
        w.home(),
        "floo_ask",
        serde_json::json!({ "to": second, "message": "hi", "from_session": first }),
    );
    assert!(
        err,
        "another segment of your own conversation is still you: {text}"
    );
    // The reason, not only the refusal: a missing-argument error would also
    // be an error, and would pass for the wrong reason.
    assert!(text.to_lowercase().contains("your own"), "{text}");
}

/// `floo_note` shares the handler, and the naming with it.
#[test]
fn note_takes_to_as_well() {
    let w = World::new();
    let (err, text) = call(
        w.home(),
        "floo_note",
        serde_json::json!({ "to": w.b, "message": "fyi", "from_session": w.a }),
    );
    assert!(!err, "{text}");
    assert_eq!(inbox(w.home(), &w.b), vec!["fyi"]);
}
