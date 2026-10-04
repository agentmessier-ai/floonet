//! End-to-end: run the real `fl` binary against a fixture corpus.
//!
//! A unit test of `window_scope` proves nothing about a caller that never
//! reaches it; the CLI's argument paths are the thing under test. So this
//! spawns the built binary with a fixture `HOME` and asserts on what a user
//! sees.

use std::path::Path;
use std::process::Command;

const UUID: &str = "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee";

fn fixture(home: &Path) {
    let day = |h: u32, m: u32| {
        use chrono::TimeZone;
        chrono::Local
            .with_ymd_and_hms(2026, 8, 4, h, m, 0)
            .unwrap()
            .to_rfc3339()
    };
    let write = |dir: &str, uuid: &str, cwd: &str, text: &str, ts: String| {
        let proj = home.join(".claude/projects").join(dir);
        std::fs::create_dir_all(&proj).unwrap();
        let line = serde_json::json!({
            "type": "user", "cwd": cwd, "timestamp": ts,
            "message": {"content": text}
        });
        std::fs::write(proj.join(format!("{uuid}.jsonl")), format!("{line}\n")).unwrap();
    };
    write(
        "-Users-test-dev-demo",
        UUID,
        "/Users/test/dev/demo",
        "the demo session said this on the fourth",
        day(10, 0),
    );
    // A second, MORE RECENT session elsewhere — so "just pick the newest" is
    // observably the wrong answer when no folder narrows it.
    write(
        "-Users-test-dev-other",
        "11111111-2222-3333-4444-555555555555",
        "/Users/test/dev/other",
        "unrelated project, same day",
        day(23, 0),
    );
}

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

/// The shape the README documents: an absolute day, no session id, narrowed
/// by folder.
#[test]
fn a_specific_day_reads_without_a_session_id() {
    let home = tempfile::tempdir().unwrap();
    fixture(home.path());

    let (ok, out) = tp(
        home.path(),
        &[
            "turns",
            "--since",
            "2026-08-04",
            "--until",
            "2026-08-05",
            "--folder",
            "/Users/test/dev/demo",
        ],
    );
    assert!(ok, "should succeed, got:\n{out}");
    assert!(
        out.contains("the demo session said this on the fourth"),
        "must read that day's turns, got:\n{out}"
    );
    assert!(
        !out.contains("unrelated project"),
        "--folder must exclude the other session, got:\n{out}"
    );
}

/// Ambiguity must be refused, not resolved by guessing. Reading one arbitrary
/// session and presenting it as "that day" answers a different question than
/// the one asked.
#[test]
fn an_ambiguous_window_lists_candidates_instead_of_picking_one() {
    let home = tempfile::tempdir().unwrap();
    fixture(home.path());

    let (ok, out) = tp(
        home.path(),
        &["turns", "--since", "2026-08-04", "--until", "2026-08-05"],
    );
    assert!(!ok, "ambiguous read must fail, got:\n{out}");
    assert!(
        out.contains("2 sessions were active"),
        "must say how many matched, got:\n{out}"
    );
    assert!(
        out.contains("--folder"),
        "must say how to narrow, got:\n{out}"
    );
    assert!(
        out.contains("/Users/test/dev/demo") && out.contains("/Users/test/dev/other"),
        "must list the candidates by cwd, got:\n{out}"
    );
}

/// The upper bound has to reach the session choice, not just the turn filter:
/// otherwise candidates are "sessions since the 4th", read through the 4th's
/// window, and a day with content yields an empty result.
#[test]
fn the_window_bounds_which_sessions_are_candidates() {
    let home = tempfile::tempdir().unwrap();
    fixture(home.path());

    let (ok, out) = tp(
        home.path(),
        &[
            "turns",
            "--since",
            "2026-08-05",
            "--until",
            "2026-08-06",
            "--folder",
            "/Users/test/dev/demo",
        ],
    );
    assert!(!ok, "a day with no activity must not silently succeed");
    assert!(
        out.contains("no sessions active between 2026-08-05 and 2026-08-06"),
        "must name the window it found nothing in, got:\n{out}"
    );
}

/// A duration bound still works, and still auto-picks when a folder narrows it.
#[test]
fn a_relative_window_still_works() {
    let home = tempfile::tempdir().unwrap();
    fixture(home.path());
    let (ok, out) = tp(
        home.path(),
        &["sessions", "--since", "2026-08-04", "--until", "2026-08-05"],
    );
    assert!(ok, "sessions should succeed, got:\n{out}");
    assert!(out.contains("/Users/test/dev/demo"), "got:\n{out}");
    assert!(out.contains("/Users/test/dev/other"), "got:\n{out}");
}

/// `subagent` and `thinking_opaque` must survive to both output surfaces.
/// Below the surface the flags are tested; the last inch is the CLI marker
/// and the MCP JSON, and an opaque turn falling through to "(no indexed
/// content)" is exactly the "no reasoning happened" claim `thinking_state =
/// 'opaque'` exists to prevent. The codex half installs the shipped descriptor
/// into the fixture home: a config invented here would pass while the shipped
/// file stayed broken.
#[test]
fn subagent_and_opaque_reach_both_output_surfaces() {
    use std::io::Write as _;
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path();
    fixture(home);

    // A subagent transcript, where Claude Code really writes one.
    let sub = home
        .join(".claude/projects/-Users-test-dev-demo")
        .join(UUID)
        .join("subagents");
    std::fs::create_dir_all(&sub).unwrap();
    let line = serde_json::json!({
        "type": "user", "isSidechain": true, "cwd": "/Users/test/dev/demo",
        "timestamp": "2026-08-04T10:05:00+00:00",
        "message": {"content": "the subagent reporting in"}
    });
    std::fs::write(sub.join("agent-e2e01.jsonl"), format!("{line}\n")).unwrap();

    // A codex rollout whose only reasoning is an encrypted blob.
    let codex_dir = home.join(".codex/sessions/2026/08/04");
    std::fs::create_dir_all(&codex_dir).unwrap();
    let msg = serde_json::json!({
        "timestamp": "2026-08-04T17:00:00.000Z", "type": "response_item",
        "payload": {"type": "message", "role": "assistant",
                     "content": [{"type": "output_text", "text": "an answer"}]}
    });
    let reasoning = serde_json::json!({
        "timestamp": "2026-08-04T17:00:01.000Z", "type": "response_item",
        "payload": {"type": "reasoning", "summary": [],
                     "encrypted_content": "gAAAAABqgTJt-not-readable"}
    });
    std::fs::write(
        codex_dir.join("rollout-2026-08-04T17-00-00-019fcdb7-c280-7abc-afed-cba987654321.jsonl"),
        format!("{msg}\n{reasoning}\n"),
    )
    .unwrap();
    let rt = home.join(".teleport/runtimes.d");
    std::fs::create_dir_all(&rt).unwrap();
    std::fs::copy(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../install/runtimes.d/codex.toml"),
        rt.join("codex.toml"),
    )
    .unwrap();

    // `fl id` prints a labelled block for humans; the id is one field of it.
    let machine = tp(home, &["id"])
        .1
        .lines()
        .find_map(|l| l.strip_prefix("device id : ").map(str::to_string))
        .unwrap();
    let sub_sid = format!("{machine}/claude_code/agent-e2e01");
    let codex_sid = format!("{machine}/codex/019fcdb7-c280-7abc-afed-cba987654321");

    // CLI surface.
    let (ok, out) = tp(home, &["turns", &sub_sid]);
    assert!(ok, "{out}");
    assert!(out.contains("[subagent]"), "CLI must mark the turn: {out}");
    let (ok, out) = tp(home, &["turns", &codex_sid]);
    assert!(ok, "{out}");
    assert!(
        out.contains("encrypted by the runtime"),
        "an opaque turn must not read as \"no indexed content\": {out}"
    );

    // MCP surface — the same two sessions through `fl mcp` over stdio, read
    // from the transcripts: the index holds pushed sessions only.
    let call = |sid: &str, include_thinking: bool| -> serde_json::Value {
        let mut child = Command::new(env!("CARGO_BIN_EXE_fl"))
            .arg("mcp")
            .env("HOME", home)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        write!(
            child.stdin.take().unwrap(),
            "{}\n{}\n",
            serde_json::json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}),
            serde_json::json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{
                "name":"floo_turns",
                "arguments":{"session_id": sid,
                              "include_thinking": include_thinking}}}),
        )
        .unwrap();
        let out = child.wait_with_output().unwrap();
        let last = String::from_utf8_lossy(&out.stdout);
        let last = last.lines().last().unwrap();
        let v: serde_json::Value = serde_json::from_str(last).unwrap();
        serde_json::from_str(v["result"]["content"][0]["text"].as_str().unwrap()).unwrap()
    };

    let turns = call(&sub_sid, false);
    assert_eq!(
        turns["turns"][0]["subagent"],
        serde_json::json!(true),
        "MCP must say whose words these are: {turns}"
    );

    let turns = call(&codex_sid, true);
    let opaque: Vec<_> = turns["turns"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|t| t["thinking_opaque"] == serde_json::json!(true))
        .collect();
    assert_eq!(opaque.len(), 1, "the encrypted reasoning record: {turns}");
    // And WITHOUT the opt-in the key stays absent — no thinking keys at all
    // means nothing is being claimed either way.
    let turns = call(&codex_sid, false);
    assert!(
        turns["turns"]
            .as_array()
            .unwrap()
            .iter()
            .all(|t| t.get("thinking_opaque").is_none()),
        "{turns}"
    );
}

/// A superseded turn must say so on both output surfaces. This drives the
/// shipped claude_code descriptor (the built-in adapter deliberately carries
/// no compaction rules) and checks the one line that differs: the kept turn
/// carries no marker.
#[test]
fn superseded_turns_say_so_on_both_surfaces_from_both_providers() {
    use std::io::Write as _;
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path();
    let proj = home.join(".claude/projects/-Users-test-dev-demo");
    std::fs::create_dir_all(&proj).unwrap();
    let msg = |text: &str, ts: &str| {
        serde_json::json!({
            "type": "user", "cwd": "/Users/test/dev/demo", "timestamp": ts,
            "message": {"content": text}
        })
        .to_string()
    };
    std::fs::write(
        proj.join(format!("{UUID}.jsonl")),
        format!(
            "{}\n{}\n{}\n{}\n",
            msg("before the cut one", "2026-08-04T10:00:00+00:00"),
            msg("before the cut two", "2026-08-04T10:01:00+00:00"),
            serde_json::json!({
                "type": "system", "subtype": "compact_boundary",
                "timestamp": "2026-08-04T10:02:00+00:00"
            }),
            msg("kept after the cut", "2026-08-04T10:03:00+00:00"),
        ),
    )
    .unwrap();
    let rt = home.join(".teleport/runtimes.d");
    std::fs::create_dir_all(&rt).unwrap();
    std::fs::copy(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../install/runtimes.d/claude_code.toml"),
        rt.join("claude_code.toml"),
    )
    .unwrap();

    let machine = tp(home, &["id"])
        .1
        .lines()
        .find_map(|l| l.strip_prefix("device id : ").map(str::to_string))
        .unwrap();
    let sid = format!("{machine}/claude_code/{UUID}");

    // CLI: a session that writes a transcript is read from it, so there is one
    // reader to check.
    let (ok, out) = tp(home, &["turns", &sid]);
    assert!(ok, "{out}");
    assert_eq!(out.matches("[superseded]").count(), 2, "{out}");
    let kept = out
        .lines()
        .find(|l| l.contains("kept after the cut"))
        .unwrap();
    assert!(!kept.contains("[superseded]"), "{kept}");

    // MCP (always the scan provider).
    let mut child = Command::new(env!("CARGO_BIN_EXE_fl"))
        .arg("mcp")
        .env("HOME", home)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    write!(
        child.stdin.take().unwrap(),
        "{}\n{}\n",
        serde_json::json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}),
        serde_json::json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{
            "name":"floo_turns","arguments":{"session_id": sid}}}),
    )
    .unwrap();
    let out = child.wait_with_output().unwrap();
    let last = String::from_utf8_lossy(&out.stdout);
    let v: serde_json::Value = serde_json::from_str(last.lines().last().unwrap()).unwrap();
    let turns: serde_json::Value =
        serde_json::from_str(v["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
    let turns = turns["turns"].as_array().unwrap();
    assert_eq!(turns.len(), 3, "{turns:?}");
    assert_eq!(turns[0]["surface"], serde_json::json!("superseded"));
    assert_eq!(turns[1]["surface"], serde_json::json!("superseded"));
    assert!(
        turns[2].get("surface").is_none(),
        "absent means current — the kept turn makes that claim: {:?}",
        turns[2]
    );
}

/// A search hit, through a real `fl mcp` process, says whose words matched and
/// whether they are still context. A runtime that writes a transcript is read
/// from it and from nowhere else, so a deleted transcript is deleted content.
#[test]
fn mcp_search_carries_surface_and_sidechain_flags() {
    use std::io::Write as _;
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path();
    let proj = home.join(".claude/projects/-Users-test-dev-demo");
    std::fs::create_dir_all(&proj).unwrap();
    // Recent timestamps: MCP search defaults to a 6h window.
    let now = chrono::Utc::now().timestamp_millis();
    let iso = |ms: i64| {
        chrono::DateTime::from_timestamp(ms / 1000, 0)
            .unwrap()
            .to_rfc3339()
    };
    let msg = |text: &str, ms: i64| {
        serde_json::json!({
            "type": "user", "cwd": "/Users/test/dev/demo", "timestamp": iso(ms),
            "message": {"content": text}
        })
        .to_string()
    };
    std::fs::write(
        proj.join(format!("{UUID}.jsonl")),
        format!(
            "{}\n{}\n{}\n",
            msg("harpoon before the cut", now - 300_000),
            serde_json::json!({
                "type": "system", "subtype": "compact_boundary",
                "timestamp": iso(now - 240_000)
            }),
            msg("harpoon after the cut", now - 180_000),
        ),
    )
    .unwrap();
    let rt = home.join(".teleport/runtimes.d");
    std::fs::create_dir_all(&rt).unwrap();
    std::fs::copy(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../install/runtimes.d/claude_code.toml"),
        rt.join("claude_code.toml"),
    )
    .unwrap();
    let machine = tp(home, &["id"])
        .1
        .lines()
        .find_map(|l| l.strip_prefix("device id : ").map(str::to_string))
        .unwrap();
    let sid = format!("{machine}/claude_code/{UUID}");

    let mcp = |body: serde_json::Value| -> serde_json::Value {
        let mut child = Command::new(env!("CARGO_BIN_EXE_fl"))
            .arg("mcp")
            .env("HOME", home)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        write!(
            child.stdin.take().unwrap(),
            "{}\n{}\n",
            serde_json::json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}),
            serde_json::json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":body}),
        )
        .unwrap();
        let out = child.wait_with_output().unwrap();
        let last = String::from_utf8_lossy(&out.stdout);
        let v: serde_json::Value = serde_json::from_str(last.lines().last().unwrap()).unwrap();
        serde_json::from_str(v["result"]["content"][0]["text"].as_str().unwrap()).unwrap()
    };

    // Search: the superseded match says so, the kept one claims current by absence.
    let found = mcp(serde_json::json!({"name":"floo_search","arguments":{"query":"harpoon"}}));
    let items = found["items"].as_array().unwrap();
    assert_eq!(items.len(), 2, "{found}");
    let by = |needle: &str| {
        items
            .iter()
            .find(|i| i["excerpt"].as_str().unwrap().contains(needle))
            .unwrap()
    };
    assert_eq!(by("before")["surface"], serde_json::json!("superseded"));
    assert!(by("after").get("surface").is_none(), "{found}");

    // The same two facts on a turns read, from the transcript.
    let got = mcp(serde_json::json!({"name":"floo_turns","arguments":{"session_id": sid}}));
    let turns = got["turns"].as_array().unwrap();
    assert_eq!(turns.len(), 2, "{got}");
    assert_eq!(turns[0]["surface"], serde_json::json!("superseded"));
}

/// `fl version` is the drift surface, and descriptor staleness is a drift: a
/// file in ~/.teleport/runtimes.d wins over the embedded descriptor, so a stale
/// one changes behaviour without changing the binary. Version must name the
/// file; a clean home must say nothing.
#[test]
fn version_names_descriptor_overrides() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path();

    // Clean: no runtimes.d at all → no descriptor lines.
    let (ok, out) = tp(home, &["version"]);
    assert!(ok, "{out}");
    assert!(!out.contains("descriptor override"), "{out}");

    // A stale-or-customized override and a redundant identical copy.
    let rt = home.join(".teleport/runtimes.d");
    std::fs::create_dir_all(&rt).unwrap();
    let repo = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../install/runtimes.d");
    std::fs::copy(repo.join("pi.toml"), rt.join("pi.toml")).unwrap();
    let cc = std::fs::read_to_string(repo.join("claude_code.toml")).unwrap();
    std::fs::write(
        rt.join("claude_code.toml"),
        cc.replace("subagents", "helpers"),
    )
    .unwrap();

    let (ok, out) = tp(home, &["version"]);
    assert!(ok, "{out}");
    let differs = out
        .lines()
        .find(|l| l.contains("DIFFERS"))
        .unwrap_or_else(|| panic!("{out}"));
    assert!(
        differs.contains("claude_code.toml") && differs.contains("embedded claude_code"),
        "{differs}"
    );
    let redundant = out
        .lines()
        .find(|l| l.contains("byte-identical"))
        .unwrap_or_else(|| panic!("{out}"));
    assert!(redundant.contains("pi.toml"), "{redundant}");
}

/// Piping into a reader that quits early must exit, not panic.
///
/// Rust sets SIGPIPE to SIG_IGN before `main`, so the failed write returns
/// EPIPE and `println!` panics on it. Driven through a real pipe rather than by
/// asserting on the signal disposition: the disposition is the mechanism, and
/// the promise is that stderr stays empty.
#[test]
fn a_reader_that_quits_early_does_not_panic() {
    use std::io::Read as _;
    use std::process::Stdio;

    let dir = tempfile::tempdir().unwrap();
    let home = dir.path();
    fixture(home);

    // `version` prints several lines and needs no index; `sessions` exercises a
    // command whose output length varies with the corpus.
    for args in [vec!["version"], vec!["sessions", "--since", "30d"]] {
        let mut producer = Command::new(env!("CARGO_BIN_EXE_fl"))
            .args(&args)
            .env("HOME", home)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();

        // Read ONE line, then drop the pipe — this is what `| head -1` does.
        {
            let mut out = producer.stdout.take().unwrap();
            let mut one = [0u8; 64];
            let _ = out.read(&mut one);
        }

        let mut stderr = String::new();
        producer
            .stderr
            .take()
            .unwrap()
            .read_to_string(&mut stderr)
            .unwrap();
        let _ = producer.wait();

        assert!(
            !stderr.contains("panicked") && !stderr.contains("Broken pipe"),
            "tp {args:?} panicked when its reader went away:\n{stderr}"
        );
    }
}

/// The CLI half of the unreadable-runtime answer. The provider being right does
/// not help if the caller drops what it said, and the real binary's output is
/// the only place the two halves meet.
#[test]
fn a_session_the_scan_cannot_read_says_why_instead_of_nothing() {
    let home = tempfile::tempdir().unwrap();
    fixture(home.path());

    // A runtime with no descriptor and no root here: one floonet is asked about
    // and cannot read by any route. The id is invented — naming a shipped
    // runtime would assert that floonet cannot read something it can.
    let (_ok, out) = tp(
        home.path(),
        &[
            "turns",
            "someone-elses-machine/acme-harness/session-0bdc4c9f",
        ],
    );
    assert!(
        out.contains("acme-harness"),
        "the runtime that cannot be read must be named, got:\n{out}"
    );
    // No provider can read it, so the answer has to say the redirect does not
    // exist, or the reader goes looking for one.
    assert!(
        out.contains("NOT an empty session"),
        "the emptiness must be attributed to floonet, not to the session:\n{out}"
    );
    assert!(
        out.contains("no other route") || out.contains("by any route"),
        "and it must close the door it used to open — there is no second \
         provider to try:\n{out}"
    );
    assert!(
        !out.contains("--index"),
        "--index no longer exists; naming it sends the reader to run a command \
         that will fail:\n{out}"
    );

    // A session that is simply absent stays a plain empty answer. If every
    // empty read carried a warning the warning would mean nothing, which is the
    // way this kind of signal usually dies.
    let (_ok2, out2) = tp(
        home.path(),
        &["turns", "someone-elses-machine/claude_code/no-such-session"],
    );
    assert!(
        out2.contains("no turns found"),
        "expected the plain empty answer, got:\n{out2}"
    );
    assert!(
        !out2.contains("--index"),
        "an absent session must not be dressed up as a provider limitation, got:\n{out2}"
    );
}
