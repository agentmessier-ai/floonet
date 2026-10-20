//! What `fl live` and `floo_live` say about each session, beyond its address.
//!
//! Three `~/dev/loop` entries were listed identically — a background session,
//! the owner's terminal, and a background job that failed before it began. A
//! sender chose the background one expecting a wake it can never get, and an
//! agent asked to stop "the other loop" killed the owner's terminal. The
//! listing has to say where each session runs and whether it holds a
//! conversation at all.

use std::path::Path;
use std::process::{Child, Command};

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

fn floo_live(home: &Path) -> serde_json::Value {
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
        child.stdin.take().unwrap(),
        "{}\n{}\n",
        serde_json::json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}),
        serde_json::json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"floo_live","arguments":{}}}),
    )
    .unwrap();
    let out = child.wait_with_output().unwrap();
    let last = String::from_utf8_lossy(&out.stdout)
        .lines()
        .last()
        .unwrap()
        .to_string();
    let v: serde_json::Value = serde_json::from_str(&last).unwrap();
    serde_json::from_str(v["result"]["content"][0]["text"].as_str().unwrap()).unwrap()
}

/// A process `ps` reports as a Claude Code background session. perl rewrites
/// its own command line through `$0`, which is what `ps` shows on macOS.
struct FakeBackground(Child);
impl FakeBackground {
    fn start() -> Self {
        let child = Command::new("perl")
            .args([
                "-e",
                r#"$0 = "claude bg-spare --bg-spare /tmp/fixture.claim.sock"; sleep 60"#,
            ])
            .spawn()
            .expect("spawn perl");
        std::thread::sleep(std::time::Duration::from_millis(300));
        FakeBackground(child)
    }
}
impl Drop for FakeBackground {
    fn drop(&mut self) {
        let _ = self.0.kill();
    }
}

const BG: &str = "00000000-0000-4000-8000-0000000000b9";
const FG: &str = "00000000-0000-4000-8000-0000000000f9";

fn register(home: &Path, native: &str, pid: u32) {
    let (ok, out) = fl(
        home,
        &[
            "register",
            "--session-id",
            native,
            "--runtime",
            "claude_code",
            "--pid",
            &pid.to_string(),
            "--cwd",
            "/w",
        ],
    );
    assert!(ok, "register: {out}");
}

#[test]
fn a_background_session_is_labelled_and_a_terminal_one_is_not() {
    let tmp = tempfile::tempdir().unwrap();
    let bg = FakeBackground::start();
    register(tmp.path(), BG, bg.0.id());
    register(tmp.path(), FG, std::process::id());

    let live = floo_live(tmp.path());
    let items = live["live"].as_array().expect("live array");
    let find = |native: &str| {
        items
            .iter()
            .find(|i| {
                i["session_id"]
                    .as_str()
                    .unwrap_or_default()
                    .ends_with(native)
            })
            .unwrap_or_else(|| panic!("{native} missing from floo_live: {live}"))
    };
    assert_eq!(find(BG)["host"], "background", "{live}");
    assert_ne!(find(FG)["host"], "background", "{live}");
    let note = find(BG)["note"].as_str().unwrap_or_default();
    assert!(
        note.contains("cannot be woken"),
        "a sender has to be told before it chooses this one: {note}"
    );

    let (_, text) = fl(tmp.path(), &["live"]);
    assert!(
        text.contains("background session"),
        "fl live must say so too:\n{text}"
    );
}

/// Neither fixture has a transcript: both have to say so, because a failed
/// background job and a session not yet spoken to are otherwise
/// indistinguishable from one holding a conversation.
#[test]
fn a_session_with_no_transcript_says_so() {
    let tmp = tempfile::tempdir().unwrap();
    register(tmp.path(), FG, std::process::id());
    let live = floo_live(tmp.path());
    let item = &live["live"][0];
    assert_eq!(item["has_conversation"], false, "{live}");
    let (_, text) = fl(tmp.path(), &["live"]);
    assert!(text.contains("no conversation yet"), "{text}");
}
