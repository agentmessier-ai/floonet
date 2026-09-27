//! The "reply with:" line must be the recipient's way of answering.
//!
//! `fl reply` is a shell command, and whether a runtime can run it is a fact
//! about that runtime's sandbox: a dsh session runs `workspace-write` confined
//! to its workspace, and the mailbox is in `~/.teleport`, so the shell command
//! fails while the `floo_reply` tool — spawned from the dsh host process —
//! works. The hint is therefore per-runtime, declared in the descriptor, like
//! `control_string`.

use std::path::Path;
use std::process::Command;

/// `env_clear` is the point rather than hygiene: a message with no sender is
/// not repliable, so the "reply with:" line is never printed, and
/// `own_session_id` falls back to `CLAUDE_CODE_SESSION_ID` from whatever
/// harness runs the suite. HOME isolates the database; PATH is needed to spawn.
fn fl(home: &Path, args: &[&str]) -> (bool, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_fl"))
        .env_clear()
        .env("PATH", std::env::var("PATH").unwrap_or_default())
        .args(args)
        .env("HOME", home)
        .output()
        .expect("run fl");
    let mut s = String::from_utf8_lossy(&out.stdout).to_string();
    s.push_str(&String::from_utf8_lossy(&out.stderr));
    (out.status.success(), s)
}

/// `fl id` prints a labelled block (`device id : XXXX-…`), not a bare id; the
/// first whitespace token is the word "device", which is a well-formed address
/// segment for a machine that does not exist.
fn machine_id(home: &Path) -> String {
    let (ok, out) = fl(home, &["id"]);
    assert!(ok, "fl id failed:\n{out}");
    out.lines()
        .find_map(|l| l.split_once("device id")?.1.split_once(':'))
        .map(|(_, id)| id.trim().to_string())
        .unwrap_or_else(|| panic!("no `device id :` line in `fl id`:\n{out}"))
}

/// Every runtime floonet ships, both instructions, in one drained inbox.
///
/// Two instructions rather than one: answering a message and confirming it is
/// finished are separate acts, and both are shell commands by default. A
/// runtime whose shell cannot write the mailbox needs the tool named for each,
/// and a fix to only one leaves the other failing in exactly the same way.
///
/// The positive and negative assertions are a pair: naming the tool is worth
/// nothing if the shell command is ALSO offered, because an agent handed both
/// tries the first one. That is not hypothetical — it is what a codex session
/// did, hitting "attempt to write a readonly database" and reporting the tool
/// as missing.
#[test]
fn both_inbox_instructions_are_the_recipients_own_way_of_answering() {
    let home = tempfile::tempdir().unwrap();
    let machine = machine_id(home.path());
    // An explicit sender, because a message without one is not repliable and
    // the reply line is never printed; it also decouples the test from its
    // harness.
    let me = format!("{machine}/claude_code/00000000-0000-4000-8000-00000000000a");

    // `confined` is the fact the descriptor encodes: the runtime's shell is
    // sandboxed away from `~/.teleport`, so only its in-process tools can
    // write the mailbox.
    for (runtime, native, confined) in [
        ("dsh", "session-11111111-2222-3333-4444-555555555555", true),
        ("codex", "33333333-4444-5555-6666-777777777777", true),
        ("claude_code", "00000000-1111-2222-3333-444444444444", false),
        ("pi", "22222222-3333-4444-5555-666666666666", false),
    ] {
        let to = format!("{machine}/{runtime}/{native}");
        let (ok, out) = fl(home.path(), &["ask", &to, "ping", "--from-session", &me]);
        assert!(ok, "[{runtime}] ask failed:\n{out}");

        // The default path drains, which prints the per-message reply line and
        // the batch ack line together — the two under test.
        let (ok, inbox) = fl(home.path(), &["inbox", "--session-id", &to]);
        assert!(ok, "[{runtime}] inbox failed:\n{inbox}");

        if confined {
            assert!(
                inbox.contains("floo_reply"),
                "[{runtime}] must be pointed at the tool that can actually write \
                 the mailbox:\n{inbox}"
            );
            assert!(
                !inbox.contains("reply with: fl reply"),
                "[{runtime}] must NOT also be offered the shell command its \
                 sandbox rejects:\n{inbox}"
            );
            assert!(
                inbox.contains("floo_ack"),
                "[{runtime}] acking is the same shell command under the same \
                 sandbox, so it needs the tool named too:\n{inbox}"
            );
            assert!(
                !inbox.contains("fl ack"),
                "[{runtime}] the shell ack must not survive beside the tool:\n{inbox}"
            );
            assert!(
                inbox.contains("sandboxed"),
                "[{runtime}] the reason has to travel with the instruction: an \
                 agent that hits a readonly-database error and does not know why \
                 escalates to a permission prompt instead of using the tool:\n{inbox}"
            );
        } else {
            assert!(
                inbox.contains("fl reply"),
                "[{runtime}] declares no hint, so it keeps the shell command:\n{inbox}"
            );
            assert!(
                inbox.contains("fl ack"),
                "[{runtime}] likewise for acking:\n{inbox}"
            );
            assert!(
                !inbox.contains("floo_reply") && !inbox.contains("floo_ack"),
                "[{runtime}] must not inherit a sandboxed runtime's tools:\n{inbox}"
            );
        }
    }
}

/// The drained line must not name a shell command of its own.
///
/// It used to point at `fl inbox --pending` as the recovery path for an
/// interrupted batch — correct advice, in a command the sandboxed runtimes it
/// was shown to cannot run, sitting in the same sentence as a hint that had
/// just been careful to avoid exactly that.
#[test]
fn the_recovery_path_is_named_without_a_command_no_one_can_run() {
    let home = tempfile::tempdir().unwrap();
    let machine = machine_id(home.path());
    let me = format!("{machine}/claude_code/00000000-0000-4000-8000-00000000000c");
    let dsh = format!("{machine}/dsh/session-44444444-5555-6666-7777-888888888888");

    let (ok, out) = fl(home.path(), &["ask", &dsh, "ping", "--from-session", &me]);
    assert!(ok, "ask failed:\n{out}");
    let (_, inbox) = fl(home.path(), &["inbox", "--session-id", &dsh]);
    assert!(
        !inbox.contains("fl inbox --pending"),
        "the recovery path is named as a view, not as a command the recipient \
         may be unable to run:\n{inbox}"
    );
    assert!(
        inbox.contains("pending view"),
        "...but it still has to be named, or an interrupted batch looks lost:\n{inbox}"
    );
}

/// Read one session's mail through `fl mcp` over stdio, as a runtime does.
///
/// `env_clear` for the reason the CLI helper records. The server answers one
/// JSON object per line and exits when stdin closes, so the last line is the
/// tool result and its `text` is the payload.
fn mcp_inbox(home: &Path, sid: &str) -> serde_json::Value {
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
        child.stdin.take().expect("mcp stdin"),
        "{}\n{}\n",
        serde_json::json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}),
        serde_json::json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{
            "name":"floo_inbox","arguments":{"session_id": sid}}}),
    )
    .expect("write mcp request");
    let out = child.wait_with_output().expect("run fl mcp");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let last = stdout.lines().last().unwrap_or_else(|| {
        panic!(
            "fl mcp said nothing:\n{}",
            String::from_utf8_lossy(&out.stderr)
        )
    });
    let v: serde_json::Value = serde_json::from_str(last).expect("mcp reply is one JSON object");
    serde_json::from_str(
        v["result"]["content"][0]["text"]
            .as_str()
            .expect("tool text"),
    )
    .expect("tool payload is JSON")
}

/// Both instructions must reach the MCP surface, not only the CLI.
///
/// The runtimes that need a non-shell instruction are exactly the ones that
/// cannot read the CLI: draining marks messages read, which is a write, so a
/// sandboxed shell fails at `fl inbox` before it could ever print the line. A
/// hint that lives only on the CLI path is one codex and dsh never see — which
/// is how a codex session ended up running `fl reply` and hitting a readonly
/// database.
#[test]
fn the_mcp_surface_carries_both_instructions_too() {
    let home = tempfile::tempdir().unwrap();
    let machine = machine_id(home.path());
    let me = format!("{machine}/claude_code/00000000-0000-4000-8000-00000000000d");

    for (runtime, native, confined) in [
        ("codex", "33333333-4444-5555-6666-777777777777", true),
        ("claude_code", "55555555-6666-7777-8888-999999999999", false),
    ] {
        let to = format!("{machine}/{runtime}/{native}");
        let (ok, out) = fl(home.path(), &["ask", &to, "ping", "--from-session", &me]);
        assert!(ok, "[{runtime}] ask failed:\n{out}");

        let v = mcp_inbox(home.path(), &to);
        let reply = v["messages"][0]["reply_with"]
            .as_str()
            .unwrap_or_else(|| panic!("[{runtime}] floo_inbox returned no reply_with: {v}"));
        let ack = v["ack_with"]
            .as_str()
            .unwrap_or_else(|| panic!("[{runtime}] floo_inbox returned no ack_with: {v}"));

        if confined {
            assert!(
                reply.contains("floo_reply") && reply.contains("sandboxed"),
                "[{runtime}] must be given the tool and the reason: {reply:?}"
            );
            assert_eq!(
                ack, "the floo_ack tool",
                "[{runtime}] acking is the same shell command under the same sandbox"
            );
        } else {
            assert!(
                reply.starts_with("fl reply"),
                "[{runtime}] declares no hint, so it keeps the shell command: {reply:?}"
            );
            assert!(
                ack.contains("fl ack") && !ack.contains("floo_ack"),
                "[{runtime}] likewise for acking: {ack:?}"
            );
        }
    }
}

/// An unrepliable message must not carry an instruction that cannot be obeyed.
#[test]
fn a_message_with_no_sender_gets_no_reply_instruction() {
    let home = tempfile::tempdir().unwrap();
    let machine = machine_id(home.path());
    let to = format!("{machine}/codex/77777777-8888-9999-aaaa-bbbbbbbbbbbb");

    // No `--from-session`: nothing to answer to.
    let (ok, out) = fl(home.path(), &["note", &to, "fyi"]);
    assert!(ok, "note failed:\n{out}");

    let v = mcp_inbox(home.path(), &to);
    assert_eq!(
        v["messages"][0]["repliable"],
        serde_json::json!(false),
        "a message with no sender is not repliable: {v}"
    );
    assert!(
        v["messages"][0]["reply_with"].is_null(),
        "...so it must carry no reply instruction: {v}"
    );
}

/// The id in the hint has to be the id you can actually answer with.
///
/// `{id}` is a substitution into descriptor text, which is the one place this
/// could silently print a placeholder or the wrong message's id — and a reply
/// sent with a wrong id is accepted, stamped onto someone else's thread, and
/// never reaches the asker.
#[test]
fn a_substituted_hint_carries_the_real_message_id() {
    let home = tempfile::tempdir().unwrap();
    let machine = machine_id(home.path());
    let dsh = format!("{machine}/dsh/session-99999999-8888-7777-6666-555555555555");

    let me = format!("{machine}/claude_code/00000000-0000-4000-8000-00000000000b");
    let (ok, sent) = fl(home.path(), &["ask", &dsh, "hello", "--from-session", &me]);
    assert!(ok, "ask failed:\n{sent}");
    // `queued <id> → <address>` — the id floonet just minted.
    let id = sent
        .split_whitespace()
        .nth(1)
        .expect("send line has an id")
        .to_string();
    assert_eq!(id.len(), 8, "expected the short id, got {id:?}");

    let (_, inbox) = fl(home.path(), &["inbox", "--session-id", &dsh]);
    assert!(
        inbox.contains(&format!("message_id: {id}")),
        "the hint must carry the id that was just queued ({id}), not a \
         placeholder:\n{inbox}"
    );
    assert!(
        !inbox.contains("{id}"),
        "an unsubstituted placeholder reached the model:\n{inbox}"
    );
}
