//! Ambiguity must end the search for a return address, not continue it.
//!
//! Several sessions registered on one process means the registry cannot say
//! which is ours. `$CLAUDE_CODE_SESSION_ID` is not a tie-breaker for that
//! question: it can name any of them, or none. Stamping it anyway costs the
//! RECIPIENT its reply path, because a reply addressed to the wrong one of
//! several live sessions is accepted and read by nobody.

use std::path::Path;
use std::process::Command;

/// `env_clear` is the point rather than hygiene: this test is about what
/// happens when `$CLAUDE_CODE_SESSION_ID` is set, so it must not inherit
/// whatever harness is running the suite. PATH is needed to spawn `ps`, which
/// is how the ancestor walk moves up.
fn fl(home: &Path, env: &[(&str, &str)], args: &[&str]) -> (bool, String) {
    let mut c = Command::new(env!("CARGO_BIN_EXE_fl"));
    c.env_clear()
        .env("PATH", std::env::var("PATH").unwrap_or_default())
        .env("HOME", home);
    for (k, v) in env {
        c.env(k, v);
    }
    let out = c.args(args).output().expect("run fl");
    let mut s = String::from_utf8_lossy(&out.stdout).to_string();
    s.push_str(&String::from_utf8_lossy(&out.stderr));
    (out.status.success(), s)
}

const A: &str = "aaaaaaaa-1111-2222-3333-444444444444";
const B: &str = "bbbbbbbb-1111-2222-3333-444444444444";
const TARGET: &str = "cccccccc-1111-2222-3333-444444444444";

#[test]
fn several_sessions_on_one_pid_leave_a_message_unstamped_rather_than_guessing() {
    let home = tempfile::tempdir().unwrap();
    let me = std::process::id().to_string();

    // Two declared sessions on THIS process, so a child `fl` walking up its
    // ancestors reaches a pid that owns both and can name neither.
    for sid in [A, B] {
        let (ok, out) = fl(
            home.path(),
            &[],
            &[
                "register",
                "--session-id",
                sid,
                "--runtime",
                "claude_code",
                "--cwd",
                "/tmp",
                "--presence",
                "declared",
                "--pid",
                &me,
            ],
        );
        assert!(ok, "register {sid} failed:\n{out}");
    }
    // The addressee, on a pid that is not in this process's ancestor chain, so
    // it cannot itself become a candidate.
    let (ok, out) = fl(
        home.path(),
        &[],
        &[
            "register",
            "--session-id",
            TARGET,
            "--runtime",
            "claude_code",
            "--cwd",
            "/tmp",
            "--presence",
            "declared",
            "--pid",
            "1",
        ],
    );
    assert!(ok, "register target failed:\n{out}");

    let (_, id_out) = fl(home.path(), &[], &["id"]);
    let machine = id_out
        .lines()
        .find_map(|l| l.split_once("device id")?.1.split_once(':'))
        .map(|(_, id)| id.trim().to_string())
        .unwrap_or_else(|| panic!("no `device id :` line:\n{id_out}"));
    let target_addr = format!("{machine}/claude_code/{TARGET}");

    // The environment names a session that IS registered, so the old fallback
    // would have produced an address here. The registry's ambiguity must win.
    let (ok, out) = fl(
        home.path(),
        &[("CLAUDE_CODE_SESSION_ID", A)],
        &["ask", &target_addr, "hello"],
    );
    assert!(ok, "ask failed:\n{out}");

    let (_, inbox) = fl(
        home.path(),
        &[],
        &["inbox", "--session-id", TARGET, "--runtime", "claude_code"],
    );
    assert!(
        inbox.contains("hello"),
        "the message must arrive regardless:\n{inbox}"
    );
    assert!(
        inbox.contains("from-session: (none"),
        "the registry could not say which of two sessions sent this, so the \
         message must carry no return address — guessing one sends the reply \
         to a mailbox nobody reads:\n{inbox}"
    );
}
